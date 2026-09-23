use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

pub const PER_FILE_OVERHEAD: u64 = 512;
pub const PACK_BASE_OVERHEAD: u64 = 256;

/// 1-byte, stack-allocated representation of supported language syntax highlighters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Markdown,
    Rust,
    Python,
    JavaScript,
    TypeScript,
    C,
    Cpp,
    CSharp,
    Go,
    Java,
    Json,
    Yaml,
    Toml,
    Bash,
    PowerShell,
    Html,
    Css,
    Sql,
    Unknown,
}

impl Lang {
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_ascii_lowercase().as_str() {
            "md" | "markdown" => Self::Markdown,
            "rs" => Self::Rust,
            "py" => Self::Python,
            "js" | "mjs" | "cjs" => Self::JavaScript,
            "ts" | "mts" | "cts" => Self::TypeScript,
            "c" | "h" => Self::C,
            "cpp" | "cxx" | "cc" | "hpp" | "hh" => Self::Cpp,
            "cs" => Self::CSharp,
            "go" => Self::Go,
            "java" => Self::Java,
            "json" => Self::Json,
            "yaml" | "yml" => Self::Yaml,
            "toml" => Self::Toml,
            "sh" | "bash" => Self::Bash,
            "ps1" | "psm1" => Self::PowerShell,
            "html" | "htm" => Self::Html,
            "css" => Self::Css,
            "sql" => Self::Sql,
            _ => Self::Unknown,
        }
    }

    pub fn is_code(&self) -> bool {
        !matches!(self, Self::Markdown | Self::Unknown)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::Rust => "rust",
            Self::Python => "python",
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::CSharp => "csharp",
            Self::Go => "go",
            Self::Java => "java",
            Self::Json => "json",
            Self::Yaml => "yaml",
            Self::Toml => "toml",
            Self::Bash => "bash",
            Self::PowerShell => "powershell",
            Self::Html => "html",
            Self::Css => "css",
            Self::Sql => "sql",
            Self::Unknown => "text",
        }
    }
}

/// Data-Oriented representation of a vault file.
#[derive(Debug, Clone)]
pub struct MdFile {
    pub abs: PathBuf,
    pub rel_unix: String,
    pub size: u64,
    pub mtime: i64,
    pub lang: Lang,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortBy {
    Recent, // Descending mtime (newest first)
    Oldest, // Ascending mtime
    Path,   // Alphabetical rel_unix
    Size,   // Descending size
}

pub fn sort_files(files: &mut [MdFile], sort_by: SortBy) {
    match sort_by {
        SortBy::Recent => {
            files.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.rel_unix.cmp(&b.rel_unix)));
        }
        SortBy::Oldest => {
            files.sort_by(|a, b| a.mtime.cmp(&b.mtime).then_with(|| a.rel_unix.cmp(&b.rel_unix)));
        }
        SortBy::Path => {
            files.sort_by(|a, b| a.rel_unix.cmp(&b.rel_unix));
        }
        SortBy::Size => {
            files.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.rel_unix.cmp(&b.rel_unix)));
        }
    }
}

pub fn format_unix_timestamp(ts_secs: i64) -> String {
    use jiff::Timestamp;
    match Timestamp::from_second(ts_secs) {
        Ok(ts) => ts.strftime("%Y-%m-%d %H:%M:%S UTC").to_string(),
        Err(_) => "1970-01-01 00:00:00 UTC".to_string(),
    }
}

pub fn scan_vault_flat(
    root: &Path,
    out_dir: &Path,
    glob_patterns: &[String],
) -> Result<Vec<MdFile>> {
    let root_canonical = dunce::canonicalize(root)
        .with_context(|| format!("Failed to canonicalize vault root: {}", root.display()))?;

    let out_abs = if out_dir.is_absolute() {
        out_dir.to_path_buf()
    } else {
        root_canonical.join(out_dir)
    };
    let out_canonical = dunce::canonicalize(&out_abs).ok();

    let mut builder = globset::GlobSetBuilder::new();
    for pat in glob_patterns {
        let glob = globset::Glob::new(pat)
            .with_context(|| format!("Invalid glob pattern: '{pat}'"))?;
        builder.add(glob);
    }
    let glob_set = builder.build().context("Failed to build glob set")?;

    let mut files = Vec::new();
    let walker = walkdir::WalkDir::new(&root_canonical).follow_links(false);

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                eprintln!("Warning: traversal error encountering entry: {e}");
                continue;
            }
        };

        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();

        if path.starts_with(&out_abs)
            || out_canonical
            .as_ref()
            .is_some_and(|o| path.starts_with(o))
        {
            continue;
        }

        let Ok(rel_path) = path.strip_prefix(&root_canonical) else {
            continue;
        };

        let rel_unix = rel_path.to_string_lossy().replace('\\', "/");

        if glob_set.is_match(&rel_unix) {
            continue;
        }

        let Ok(metadata) = entry.metadata() else {
            continue;
        };

        let size = metadata.len();
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let lang = Lang::from_extension(ext);

        files.push(MdFile {
            abs: path.to_path_buf(),
            rel_unix,
            size,
            mtime,
            lang,
        });
    }

    Ok(files)
}

#[derive(Debug, Clone)]
pub struct PackPlan {
    pub boundaries: Vec<(usize, usize)>,
}

pub fn plan_packs(files: &[MdFile], available_bytes: u64) -> PackPlan {
    let mut boundaries = Vec::new();
    if files.is_empty() {
        return PackPlan { boundaries };
    }

    let mut start = 0;
    let mut current_size: u64 = 0;

    for (i, f) in files.iter().enumerate() {
        let effective = f.size.saturating_add(PER_FILE_OVERHEAD);
        let single_file_total = PACK_BASE_OVERHEAD.saturating_add(effective);

        if single_file_total > available_bytes {
            eprintln!(
                "Warning: File '{}' ({} bytes + {} overhead) exceeds capacity ({} bytes). Placed in dedicated pack.",
                f.rel_unix, f.size, PER_FILE_OVERHEAD + PACK_BASE_OVERHEAD, available_bytes
            );
            if i > start {
                boundaries.push((start, i));
            }
            boundaries.push((i, i + 1));
            start = i + 1;
            current_size = 0;
            continue;
        }

        let pack_total = PACK_BASE_OVERHEAD.saturating_add(current_size).saturating_add(effective);

        if pack_total > available_bytes && current_size > 0 {
            boundaries.push((start, i));
            start = i;
            current_size = effective;
        } else {
            current_size = current_size.saturating_add(effective);
        }
    }

    if start < files.len() {
        boundaries.push((start, files.len()));
    }

    PackPlan { boundaries }
}

pub fn escape_md_cell(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '|' => out.push_str("\\|"),
            _ => out.push(c),
        }
    }
    out
}

pub fn unescape_md_cell(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('\\') => out.push('\\'),
                Some('|') => out.push('|'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn split_unescaped_pipes(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut escaped = false;

    for c in s.chars() {
        if escaped {
            current.push('\\');
            current.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '|' {
            parts.push(std::mem::take(&mut current));
        } else {
            current.push(c);
        }
    }
    if escaped {
        current.push('\\');
    }
    parts.push(current);
    parts
}

pub fn read_existing_manifest_paths(manifest_path: &Path) -> HashSet<String> {
    let mut packed = HashSet::new();
    let file = match File::open(manifest_path) {
        Ok(f) => f,
        Err(_) => return packed,
    };

    let reader = BufReader::new(file);
    for line in reader.lines().map_while(Result::ok) {
        let line = line.trim();
        if !line.starts_with('|') || line.starts_with("| Relative Path") || line.starts_with("|---") {
            continue;
        }

        let parts = split_unescaped_pipes(line);
        if parts.len() >= 4 {
            let path = unescape_md_cell(parts[1].trim());
            if !path.is_empty() {
                packed.insert(path);
            }
        }
    }
    packed
}

pub fn find_highest_pack_index(out_dir: &Path, vault_name: &str) -> usize {
    let prefix = format!("{vault_name}-pack_");
    let mut highest = 0;

    if let Ok(entries) = fs::read_dir(out_dir) {
        for entry in entries.map_while(Result::ok) {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(rest) = name.strip_prefix(&prefix)
                && let Some(num_str) = rest.strip_suffix(".md")
                && let Ok(num) = num_str.parse::<usize>()
                && num > highest
            {
                highest = num;
            }
        }
    }
    highest
}

pub fn clean_existing_pack_outputs(
    out_dir: &Path,
    vault_name: &str,
    manifest_name: &str,
) -> Result<()> {
    let pack_prefix = format!("{vault_name}-pack_");
    let mut failures = Vec::new();

    if let Ok(entries) = fs::read_dir(out_dir) {
        for entry in entries.map_while(Result::ok) {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if ((name.starts_with(&pack_prefix) && name.ends_with(".md")) || name == manifest_name)
                && let Err(e) = fs::remove_file(&path)
            {
                failures.push((path, e));
            }
        }
    }

    if !failures.is_empty() {
        for (path, e) in &failures {
            eprintln!("Warning: could not remove '{}': {}", path.display(), e);
        }
        bail!(
            "--force could not remove {} existing output file(s); close any applications locking them and retry.",
            failures.len()
        );
    }

    Ok(())
}

pub fn persist_with_retry(mut temp: NamedTempFile, target: &Path) -> Result<()> {
    let mut last_err = None;
    for attempt in 0..5 {
        match temp.persist(target) {
            Ok(_) => return Ok(()),
            Err(e) => {
                temp = e.file;
                last_err = Some(e.error);
                if attempt < 4 {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }
    }
    Err(anyhow::anyhow!(
        "Failed to persist '{}' after 5 attempts: {}. Ensure destination path is not locked by another process.",
        target.display(),
        last_err.unwrap()
    ))
}

pub struct PackOptions<'a> {
    pub vault_name: &'a str,
    pub out_dir: &'a Path,
    pub resume: bool,
    pub write_manifest: bool,
    pub available_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct ManifestEntry {
    pub file_idx: usize,
    pub pack_num: usize,
}

pub fn execute_pack_write(
    files: &[MdFile],
    plan: &PackPlan,
    opts: &PackOptions,
) -> Result<()> {
    fs::create_dir_all(opts.out_dir)
        .with_context(|| format!("Failed to create output directory: {}", opts.out_dir.display()))?;

    let manifest_name = format!("{}-manifest.md", opts.vault_name);
    let manifest_path = opts.out_dir.join(&manifest_name);

    if opts.resume && !manifest_path.exists() {
        bail!(
            "--resume specified but no manifest found at '{}'. Run without --resume for a fresh pack, or use --force to purge existing outputs.",
            manifest_path.display()
        );
    }

    let already_packed = if opts.resume {
        read_existing_manifest_paths(&manifest_path)
    } else {
        HashSet::new()
    };

    let pack_start_idx = if opts.resume {
        find_highest_pack_index(opts.out_dir, opts.vault_name) + 1
    } else {
        1
    };

    for &(start, end) in &plan.boundaries {
        let est: u64 = files[start..end]
            .iter()
            .map(|f| f.size.saturating_add(PER_FILE_OVERHEAD))
            .sum::<u64>()
            .saturating_add(PACK_BASE_OVERHEAD);

        if est > opts.available_bytes && (end - start) > 1 {
            eprintln!(
                "Warning: Pack slice [{}..{}] estimated at {} bytes exceeds target capacity of {} bytes.",
                start, end, est, opts.available_bytes
            );
        }
    }

    let mut written_pack_num = pack_start_idx;
    let mut manifest_entries = Vec::new();

    let generation_ts = jiff::Timestamp::now().as_second();
    let generation_str = format_unix_timestamp(generation_ts);

    let pb = indicatif::ProgressBar::new(plan.boundaries.len() as u64);
    const PROGRESS_TEMPLATE: &str = "[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} packs ({msg})";
    pb.set_style(
        indicatif::ProgressStyle::default_bar()
            .template(PROGRESS_TEMPLATE)
            .with_context(|| format!("Failed to construct progress bar template: '{PROGRESS_TEMPLATE}'"))?
            .progress_chars("#>-"),
    );

    for &(start, end) in &plan.boundaries {
        let active_indices: Vec<usize> = (start..end)
            .filter(|&idx| !already_packed.contains(&files[idx].rel_unix))
            .collect();

        if active_indices.is_empty() {
            pb.inc(1);
            continue;
        }

        let pack_num = written_pack_num;
        let pack_filename = format!("{}-pack_{:04}.md", opts.vault_name, pack_num);
        let pack_path = opts.out_dir.join(&pack_filename);
        pb.set_message(format!("pack {pack_num:04}"));

        let temp_file = NamedTempFile::new_in(opts.out_dir)
            .with_context(|| format!("Failed to create temporary pack file in {}", opts.out_dir.display()))?;

        {
            let mut writer = BufWriter::new(&temp_file);

            writeln!(writer, "# Pack {pack_num:04}: {}", opts.vault_name)?;
            writeln!(writer, "<!-- Pack Generated: {generation_str} -->\n")?;

            for &idx in &active_indices {
                let f = &files[idx];
                let formatted_mtime = format_unix_timestamp(f.mtime);

                writeln!(
                    writer,
                    "<!-- File: {} | Last Modified: {} | Size: {} bytes -->",
                    f.rel_unix, formatted_mtime, f.size
                )?;

                if f.lang.is_code() {
                    writeln!(writer, "```{}", f.lang.as_str())?;
                    let mut src = File::open(&f.abs)
                        .with_context(|| format!("Failed to open file: {}", f.abs.display()))?;
                    std::io::copy(&mut src, &mut writer)?;
                    writeln!(writer, "\n```\n")?;
                } else {
                    let mut src = File::open(&f.abs)
                        .with_context(|| format!("Failed to open file: {}", f.abs.display()))?;
                    std::io::copy(&mut src, &mut writer)?;
                    writeln!(writer, "\n\n---\n")?;
                }

                if opts.write_manifest {
                    manifest_entries.push(ManifestEntry {
                        file_idx: idx,
                        pack_num,
                    });
                }
            }

            writer.flush()?;
        }

        persist_with_retry(temp_file, &pack_path)?;

        written_pack_num += 1;
        pb.inc(1);
    }

    pb.finish_with_message("Packs written successfully");

    if opts.write_manifest && !manifest_entries.is_empty() {
        let m_temp = NamedTempFile::new_in(opts.out_dir)
            .with_context(|| format!("Failed to create temp manifest file in {}", opts.out_dir.display()))?;

        {
            let mut m_writer = BufWriter::new(&m_temp);

            let append_mode = opts.resume && manifest_path.exists();

            if append_mode {
                let mut existing = File::open(&manifest_path)
                    .with_context(|| format!("Failed to open existing manifest for resume: {}", manifest_path.display()))?;
                std::io::copy(&mut existing, &mut m_writer)?;
            } else {
                writeln!(m_writer, "# Vault Manifest: {}\n", opts.vault_name)?;
                writeln!(
                    m_writer,
                    "<!-- Manifest Generated: {generation_str} -->\n"
                )?;
                writeln!(m_writer, "| Relative Path | Pack File | Last Modified |")?;
                writeln!(m_writer, "|---|---|---|")?;
            }

            for entry in manifest_entries {
                let f = &files[entry.file_idx];
                let escaped_path = escape_md_cell(&f.rel_unix);
                let pack_filename = format!("{}-pack_{:04}.md", opts.vault_name, entry.pack_num);
                let mtime_str = format_unix_timestamp(f.mtime);
                writeln!(m_writer, "| {escaped_path} | {pack_filename} | {mtime_str} |")?;
            }

            m_writer.flush()?;
        }

        persist_with_retry(m_temp, &manifest_path)?;
    }

    Ok(())
}