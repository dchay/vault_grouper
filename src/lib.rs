use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use indicatif::{ProgressBar, ProgressStyle};
use phf::phf_set;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::NamedTempFile;

pub const PER_FILE_OVERHEAD: u64 = 256;
pub const PACK_BASE_OVERHEAD: u64 = 512;

const SCAN_SPINNER_TEMPLATE: &str =
    "{spinner:.green} [{elapsed_precise}] Scanning vault entries... ({pos} files matched)";
const PACK_PROGRESS_TEMPLATE: &str =
    "[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} packs written";

pub static PACKABLE_EXTENSIONS: phf::Set<&'static str> = phf_set! {
    "md", "markdown", "txt", "rs", "py", "js", "mjs", "cjs", "ts", "mts", "cts",
    "c", "h", "cpp", "cxx", "cc", "hpp", "hh", "cs", "go", "java", "json",
    "yaml", "yml", "toml", "sh", "bash", "ps1", "psm1", "html", "htm",
    "css", "sql", "xml", "csv", "log", "rst", "adoc"
};

pub struct MdFile {
    pub abs: PathBuf,
    pub rel_unix: String,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Debug, Clone, Copy)]
pub struct ManifestEntry {
    pub file_idx: usize,
    pub pack_num: usize,
}

#[derive(Debug)]
pub struct PackPlan {
    pub boundaries: Vec<(usize, usize)>,
}

pub struct PackOptions<'a> {
    pub vault_name: &'a str,
    pub out_dir: &'a Path,
    pub resume: bool,
    pub write_manifest: bool,
    pub available_bytes: u64,
    pub quiet: bool,
}

#[inline]
pub fn format_unix_timestamp(ts: i64) -> String {
    jiff::Timestamp::from_second(ts)
        .map(|t| t.strftime("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|_| "1970-01-01 00:00:00 UTC".to_string())
}

pub fn escape_md_cell(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '|' => out.push_str("\\|"),
            '`' => out.push_str("\\`"),
            '*' => out.push_str("\\*"),
            '_' => out.push_str("\\_"),
            '[' => out.push_str("\\["),             ']' => out.push_str("\\]"),
            '\n' | '\r' => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

pub fn should_retry(err: &std::io::Error) -> bool {
    matches!(err.kind(), std::io::ErrorKind::PermissionDenied)
        || matches!(err.raw_os_error(), Some(32 | 33)) // 32: ERROR_SHARING_VIOLATION, 33: ERROR_LOCK_VIOLATION
}

pub fn persist_with_retry(temp: NamedTempFile, target: &Path) -> Result<()> {
    let backoff = backoff::ExponentialBackoffBuilder::new()
        .with_initial_interval(Duration::from_millis(5))
        .with_max_interval(Duration::from_millis(100))
        .with_max_elapsed_time(Some(Duration::from_secs(3)))
        .build();

    let mut temp_opt = Some(temp);

    let op = || {
        let temp_file = temp_opt
            .take()
            .expect("temp_opt must be present for retry attempt");

        match temp_file.persist(target) {
            Ok(_) => Ok(()),
            Err(e) => {
                temp_opt = Some(e.file);
                if should_retry(&e.error) {
                    Err(backoff::Error::transient(e.error))
                } else {
                    Err(backoff::Error::permanent(e.error))
                }
            }
        }
    };

    backoff::retry(backoff, op)
        .map_err(|e| anyhow::anyhow!("Failed to persist '{}': {}", target.display(), e))?;

    Ok(())
}

pub fn build_glob_set(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        let trimmed = p.trim();
        if !trimmed.is_empty() {
            let glob = Glob::new(trimmed)
                .with_context(|| format!("Invalid glob pattern: '{trimmed}'"))?;
            builder.add(glob);
        }
    }
    Ok(Some(builder.build()?))
}

pub fn scan_vault_flat(
    vault_root: &Path,
    exclude_globs: Option<&GlobSet>,
    include_hidden: bool,
    quiet: bool,
) -> Result<Vec<MdFile>> {
    let root_canonical = dunce::canonicalize(vault_root)
        .with_context(|| format!("Failed to resolve vault root: {}", vault_root.display()))?;

    let pb = if !quiet {
        let p = ProgressBar::new_spinner();
        p.set_style(
            ProgressStyle::default_spinner()
                .template(SCAN_SPINNER_TEMPLATE)
                .with_context(|| format!("Invalid spinner template: '{SCAN_SPINNER_TEMPLATE}'"))?,
        );
        p.enable_steady_tick(Duration::from_millis(100));
        Some(p)
    } else {
        None
    };

    let mut files = Vec::with_capacity(1024);

    for entry in walkdir::WalkDir::new(&root_canonical)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if include_hidden {
                true
            } else if let Some(name) = e.file_name().to_str() {
                !name.starts_with('.')
            } else {
                true
            }
        })
    {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                if !quiet {
                    let msg = e.to_string();
                    if msg.contains("does not exist") || msg.contains("path too long") {
                        eprintln!(
                            "Warning: Cannot traverse path (likely exceeds Windows MAX_PATH): {}",
                            e.path().map(|p| p.display().to_string()).unwrap_or_default()
                        );
                    } else {
                        eprintln!("Warning: Skipping unreadable entry during scan: {e}");
                    }
                }
                continue;
            }
        };

        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();
        let Some(ext_os) = path.extension() else { continue };
        let Some(ext) = ext_os.to_str() else { continue };
        let ext_lower = ext.to_lowercase();

        if !PACKABLE_EXTENSIONS.contains(ext_lower.as_str()) {
            continue;
        }

        let Ok(rel_path) = path.strip_prefix(&root_canonical) else {
            continue;
        };
        let rel_unix = rel_path.to_string_lossy().replace('\\', "/");

        if let Some(globs) = exclude_globs
            && globs.is_match(&rel_unix)
        {
            continue;
        }

        let Ok(metadata) = entry.metadata() else {
            if !quiet {
                eprintln!("Warning: Could not read metadata for '{}'", rel_unix);
            }
            continue;
        };

        let size = metadata.len();
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        files.push(MdFile {
            abs: path.to_path_buf(),
            rel_unix,
            size,
            mtime,
        });

        if let Some(ref p) = pb {
            p.inc(1);
        }
    }

    if let Some(p) = pb {
        p.finish_and_clear();
    }

    files.sort_by(|a, b| {
        b.mtime
            .cmp(&a.mtime)
            .then_with(|| a.rel_unix.cmp(&b.rel_unix))
    });

    Ok(files)
}

pub fn plan_packs(files: &[MdFile], available_bytes: u64, quiet: bool) -> PackPlan {
    let mut boundaries = Vec::new();
    let n = files.len();
    if n == 0 {
        return PackPlan { boundaries };
    }

    let mut start = 0;
    let mut current_size = PACK_BASE_OVERHEAD;

    for (i, f) in files.iter().enumerate() {
        let item_cost = f.size.saturating_add(PER_FILE_OVERHEAD);

        if PACK_BASE_OVERHEAD.saturating_add(item_cost) > available_bytes {
            if start < i {
                boundaries.push((start, i));
            }
            boundaries.push((i, i + 1));
            if !quiet {
                eprintln!(
                    "Warning: File '{}' ({} bytes total, including {} bytes overhead) exceeds capacity ({} bytes). Placed in dedicated pack.",
                    f.rel_unix,
                    f.size.saturating_add(PER_FILE_OVERHEAD).saturating_add(PACK_BASE_OVERHEAD),
                    PER_FILE_OVERHEAD + PACK_BASE_OVERHEAD,
                    available_bytes
                );
            }
            start = i + 1;
            current_size = PACK_BASE_OVERHEAD;
            continue;
        }

        if current_size.saturating_add(item_cost) > available_bytes {
            boundaries.push((start, i));
            start = i;
            current_size = PACK_BASE_OVERHEAD.saturating_add(item_cost);
        } else {
            current_size = current_size.saturating_add(item_cost);
        }
    }

    if start < n {
        boundaries.push((start, n));
    }

    PackPlan { boundaries }
}

pub fn find_highest_pack_index(out_dir: &Path, pack_prefix: &str) -> usize {
    let mut highest = 0;
    if let Ok(entries) = fs::read_dir(out_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && let Some(rest) = name.strip_prefix(pack_prefix)
                && let Some(num_str) = rest.strip_suffix(".md")
                && let Ok(num) = num_str.parse::<usize>()
            {
                highest = highest.max(num);
            }
        }
    }
    highest
}

pub fn read_already_packed_rel_paths(
    out_dir: &Path,
    pack_prefix: &str,
) -> Result<HashSet<String>> {
    let mut set = HashSet::new();
    if !out_dir.exists() {
        return Ok(set);
    }

    for entry in fs::read_dir(out_dir)?.flatten() {
        let path = entry.path();
        if path.is_file()
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
            && name.starts_with(pack_prefix)
            && name.ends_with(".md")
            && let Ok(file) = File::open(&path)
        {
            let reader = BufReader::new(file);
            for line in reader.lines().map_while(Result::ok) {
                if let Some(rest) = line.strip_prefix("<!-- File: ")
                    && let Some(rel) = rest.split(" | ").next()
                {
                    set.insert(rel.to_string());
                }
            }
        }
    }
    Ok(set)
}

pub fn clean_existing_pack_outputs(
    out_dir: &Path,
    pack_prefix: &str,
    manifest_name: &str,
    quiet: bool,
) -> Result<()> {
    if !out_dir.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(out_dir)?.flatten() {
        let path = entry.path();
        if path.is_file()
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
            && ((name.starts_with(pack_prefix) && name.ends_with(".md")) || name == manifest_name)
            && let Err(e) = fs::remove_file(&path)
            && !quiet
        {
            eprintln!("Warning: Failed to clean output file '{}': {e}", path.display());
        }
    }
    Ok(())
}

pub fn execute_pack_write(
    files: &[MdFile],
    plan: &PackPlan,
    opts: &PackOptions,
) -> Result<Vec<ManifestEntry>> {
    fs::create_dir_all(opts.out_dir)
        .with_context(|| format!("Failed to create output dir: {}", opts.out_dir.display()))?;

    let lock_path = opts.out_dir.join(".vault_grouper.lock");
    let lock_file = File::create(&lock_path)
        .with_context(|| format!("Failed to create lock file at {}", lock_path.display()))?;

    fs4::FileExt::lock(&lock_file)
        .with_context(|| format!("Failed to acquire lock on {}", lock_path.display()))?;

    debug_assert!(
        plan.boundaries.iter().all(|&(s, e)| {
            let est: u64 = files[s..e]
                .iter()
                .map(|f| f.size.saturating_add(PER_FILE_OVERHEAD))
                .sum::<u64>()
                .saturating_add(PACK_BASE_OVERHEAD);
            est <= opts.available_bytes || (e - s) == 1
        }),
        "plan_packs produced an oversized pack; this is a bug in plan_packs"
    );

    let pack_prefix = format!("{}_pack_", opts.vault_name);
    let start_pack_num = if opts.resume {
        find_highest_pack_index(opts.out_dir, &pack_prefix) + 1
    } else {
        1
    };

    let generation_ts = jiff::Timestamp::now().as_second();
    let generation_str = format_unix_timestamp(generation_ts);

    let pb = if !opts.quiet {
        let p = ProgressBar::new(plan.boundaries.len() as u64);
        p.set_style(
            ProgressStyle::default_bar()
                .template(PACK_PROGRESS_TEMPLATE)
                .with_context(|| format!("Invalid template: '{PACK_PROGRESS_TEMPLATE}'"))?,
        );
        Some(p)
    } else {
        None
    };

    let mut manifest_entries = Vec::with_capacity(files.len());

    for (i, &(start, end)) in plan.boundaries.iter().enumerate() {
        let current_pack_num = start_pack_num + i;
        let pack_filename = format!("{pack_prefix}{current_pack_num:04}.md");
        let pack_target_path = opts.out_dir.join(&pack_filename);

        let parent_dir = pack_target_path
            .parent()
            .unwrap_or_else(|| Path::new("."));
        let mut temp_pack = NamedTempFile::new_in(parent_dir)
            .with_context(|| format!("Failed to create temporary file in {}", parent_dir.display()))?;

        {
            let mut writer = BufWriter::new(&mut temp_pack);
            writeln!(writer, "# Vault Pack: {}", opts.vault_name)?;
            writeln!(writer, "<!-- Pack Number: {current_pack_num} -->")?;
            writeln!(writer, "<!-- Generation Date: {generation_str} -->\n")?;

            let mut read_buf = vec![0u8; 64 * 1024];

            for (offset, file) in files[start..end].iter().enumerate() {
                let idx = start + offset;
                writeln!(
                    writer,
                    "<!-- File: {} | Last Modified: {} -->",
                    file.rel_unix,
                    format_unix_timestamp(file.mtime)
                )?;
                writeln!(writer, "## Path: {}\n", file.rel_unix)?;

                match File::open(&file.abs) {
                    Ok(f) => {
                        let mut reader = BufReader::new(f);
                        loop {
                            let bytes_read = reader.read(&mut read_buf)?;
                            if bytes_read == 0 {
                                break;
                            }
                            writer.write_all(&read_buf[..bytes_read])?;
                        }
                    }
                    Err(e) => {
                        if !opts.quiet {
                            eprintln!(
                                "Warning: Unreadable file skipped during pack write '{}': {e}",
                                file.rel_unix
                            );
                        }
                        writeln!(writer, "*[File contents unreadable]*")?;
                    }
                }

                writeln!(writer, "\n---\n")?;
                manifest_entries.push(ManifestEntry {
                    file_idx: idx,
                    pack_num: current_pack_num,
                });
            }
            writer.flush()?;
        }

        persist_with_retry(temp_pack, &pack_target_path)?;

        if let Some(ref p) = pb {
            p.inc(1);
        }
    }

    if let Some(p) = pb {
        p.finish_and_clear();
    }

    if opts.write_manifest && !manifest_entries.is_empty() {
        let manifest_filename = format!("{}_manifest.md", opts.vault_name);
        let manifest_target_path = opts.out_dir.join(&manifest_filename);

        let parent_dir = manifest_target_path
            .parent()
            .unwrap_or_else(|| Path::new("."));
        let mut temp_manifest = NamedTempFile::new_in(parent_dir)?;

        {
            let mut m_writer = BufWriter::new(&mut temp_manifest);

            if opts.resume && manifest_target_path.exists() {
                if let Ok(mut existing) = File::open(&manifest_target_path) {
                    std::io::copy(&mut existing, &mut m_writer)?;
                    writeln!(m_writer)?;
                }
            } else {
                writeln!(m_writer, "# Manifest for {}", opts.vault_name)?;
                writeln!(m_writer, "<!-- Generation Date: {generation_str} -->\n")?;
                writeln!(
                    m_writer,
                    "| Relative Path | Pack File | Last Modified | Size (bytes) |"
                )?;
                writeln!(
                    m_writer,
                    "| :--- | :--- | :--- | :--- |"
                )?;
            }

            for entry in &manifest_entries {
                let file = &files[entry.file_idx];
                let pack_filename = format!("{pack_prefix}{:04}.md", entry.pack_num);
                writeln!(
                    m_writer,
                    "| {} | {} | {} | {} |",
                    escape_md_cell(&file.rel_unix),
                    escape_md_cell(&pack_filename),
                    format_unix_timestamp(file.mtime),
                    file.size
                )?;
            }
            m_writer.flush()?;
        }

        persist_with_retry(temp_manifest, &manifest_target_path)?;
    }

    Ok(manifest_entries)
}