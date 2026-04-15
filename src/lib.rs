//! obsidian_vault_grouper v0.1.9
//! Library crate: core logic + CLI runner.

#![forbid(unsafe_code)]
#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions, clippy::missing_errors_doc)]

use std::{
    collections::HashMap,
    ffi::OsStr,
    fs::{self, File},
    io::{BufWriter, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{bail, Context, Result};
use clap::{ArgAction, Parser, ValueEnum};
use globset::{Glob, GlobSet, GlobSetBuilder};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use walkdir::WalkDir;

pub const DEFAULT_MAX_MB: f64 = 39.0;
pub const DEFAULT_OVERHEAD_PER_CHAPTER: u64 = 512;
pub const DEFAULT_READ_CHUNK: usize = 256 * 1024;
pub const DEFAULT_WRITE_BUFFER: usize = 256 * 1024;
const SCAN_TICK_BATCH: usize = 50;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SortBy {
    Name,
    Mtime,
}

#[derive(Debug, Parser)]
#[command(
    name = "obsidian_vault_grouper",
    version = env!("CARGO_PKG_VERSION"),
    about = "Pack an Obsidian Vault's .md files into readable group-files.",
)]
pub struct Cli {
    /// Root of the Obsidian vault
    pub vault_root: PathBuf,

    /// Output directory (default: <vault_root>/_grouped)
    pub output_dir: Option<PathBuf>,

    /// Per-group size ceiling in MB
    #[arg(long, default_value_t = DEFAULT_MAX_MB)]
    pub max_mb: f64,

    /// Show plan; write nothing
    #[arg(long, action = ArgAction::SetTrue)]
    pub dry_run: bool,

    /// Print vault statistics only; do not write group-files
    #[arg(long, action = ArgAction::SetTrue)]
    pub stats_only: bool,

    /// Overwrite existing group-files
    #[arg(long, action = ArgAction::SetTrue)]
    pub force: bool,

    /// Skip groups whose output already exists
    #[arg(long, action = ArgAction::SetTrue)]
    pub resume: bool,

    /// Extra diagnostic output on stderr
    #[arg(short, long, action = ArgAction::SetTrue)]
    pub verbose: bool,

    /// Suppress progress/report (warnings/errors still printed)
    #[arg(short, long, action = ArgAction::SetTrue)]
    pub quiet: bool,

    /// File ordering inside groups
    #[arg(long, value_enum, default_value_t = SortBy::Name)]
    pub sort_by: SortBy,

    /// Exclude glob pattern (repeatable, supports **)
    #[arg(long = "exclude")]
    pub exclude_patterns: Vec<String>,

    /// Skip writing vault_manifest.json
    #[arg(long = "no-manifest", action = ArgAction::SetTrue)]
    pub no_manifest: bool,

    /// Pretty-print manifest JSON (default: compact)
    #[arg(long = "indent-manifest", action = ArgAction::SetTrue)]
    pub indent_manifest: bool,

    /// Maximum chapters per group (0 = unlimited)
    #[arg(long = "max-chapters", default_value_t = 0)]
    pub max_chapters: usize,

    /// Disable progress bars (useful in CI / pipes)
    #[arg(long = "no-progress", action = ArgAction::SetTrue)]
    pub no_progress: bool,

    /// Filename prefix for group-files (default: vault_group_)
    #[arg(long = "output-prefix", default_value = "vault_group_")]
    pub output_prefix: String,
}

#[derive(Debug)]
pub struct VaultConfig {
    pub max_bytes: u64,
    pub chapter_separator: String,
    pub overhead_per_chapter: u64,
    pub read_chunk: usize,
    pub write_buffer: usize,
    pub sort_by: SortBy,
    pub exclude: GlobSet,
    pub max_chapters: usize,
    pub output_prefix: String,
}

impl VaultConfig {
    pub fn from_cli(cli: &Cli) -> Result<Self> {
        if cli.max_mb <= 0.0 {
            bail!("--max-mb must be > 0, got {}", cli.max_mb);
        }

        let mut builder = GlobSetBuilder::new();
        for pat in &cli.exclude_patterns {
            builder.add(Glob::new(pat).with_context(|| format!("Invalid glob pattern: {pat}"))?);
        }

        let exclude = builder.build().context("Failed to build globset")?;

        Ok(Self {
            max_bytes: (cli.max_mb * 1_048_576.0) as u64,
            chapter_separator: "\n\n---\n\n".to_string(),
            overhead_per_chapter: DEFAULT_OVERHEAD_PER_CHAPTER,
            read_chunk: DEFAULT_READ_CHUNK,
            write_buffer: DEFAULT_WRITE_BUFFER,
            sort_by: cli.sort_by,
            exclude,
            max_chapters: cli.max_chapters,
            output_prefix: cli.output_prefix.clone(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct MdFile {
    pub abs: PathBuf,
    pub rel: PathBuf,
    pub size: u64,
    pub mtime: SystemTime,
}

#[derive(Debug)]
pub struct GroupPlan {
    pub index: usize,
    pub files: Vec<MdFile>,
    pub estimated_bytes: u64,
}

impl GroupPlan {
    pub fn new(index: usize) -> Self {
        Self { index, files: Vec::new(), estimated_bytes: 0 }
    }

    pub fn chapter_count(&self) -> usize {
        self.files.len()
    }

    pub fn can_fit(&self, md: &MdFile, cfg: &VaultConfig) -> bool {
        if cfg.max_chapters > 0 && self.files.len() >= cfg.max_chapters {
            return false;
        }
        self.estimated_bytes + md.size + cfg.overhead_per_chapter < cfg.max_bytes
    }

    pub fn add(&mut self, md: MdFile, cfg: &VaultConfig, is_solo: bool) {
        let overhead = if is_solo { 0 } else { cfg.overhead_per_chapter };
        self.estimated_bytes += md.size + overhead;
        self.files.push(md);
    }
}

#[derive(Debug)]
pub struct WriteResult {
    pub group_index: usize,
    pub path: PathBuf,
    pub bytes_written: u64,
    pub chapter_count: usize,
    pub skipped: bool,
}

#[derive(Debug, Serialize)]
pub struct ManifestChapter {
    pub chapter: usize,
    pub source: String,
    pub bytes: u64,
    pub mtime: f64,
    pub sha256: String,
}

#[derive(Debug, Serialize)]
pub struct ManifestGroup {
    pub index: usize,
    pub filename: String,
    pub bytes_written: Option<u64>,
    pub skipped: bool,
    pub chapters: Vec<ManifestChapter>,
}

#[derive(Debug, Serialize)]
pub struct ManifestRoot {
    pub version: String,
    pub generated_at: String,
    pub vault_root_posix: String,
    pub vault_root_native: String,
    pub max_mb: f64,
    pub groups: Vec<ManifestGroup>,
}

pub fn path_to_posix(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

pub fn make_group_filename(prefix: &str, index: usize) -> String {
    format!("{prefix}{index:04}.md")
}

fn is_excluded(rel: &Path, cfg: &VaultConfig) -> bool {
    if cfg.exclude.is_empty() {
        return false;
    }
    let posix = path_to_posix(rel);
    if cfg.exclude.is_match(&posix) {
        return true;
    }
    cfg.exclude.is_match(format!("{posix}/"))
}

pub fn discover_md_files(
    root: &Path,
    exclude_out_dir: Option<&Path>,
    cfg: &VaultConfig,
    scan_pb: &Option<ProgressBar>,
) -> (Vec<MdFile>, Vec<String>) {
    let mut results = Vec::new();
    let mut warnings = Vec::new();
    let mut count = 0usize;

    let rel_out: Option<PathBuf> = exclude_out_dir
        .and_then(|p| p.strip_prefix(root).ok())
        .map(|p| p.to_path_buf());

    let cmp_fn: Box<
        dyn Fn(&walkdir::DirEntry, &walkdir::DirEntry) -> std::cmp::Ordering + Send + Sync,
    > = match cfg.sort_by {
        SortBy::Name => Box::new(|a, b| {
            a.file_name()
                .to_string_lossy()
                .to_lowercase()
                .cmp(&b.file_name().to_string_lossy().to_lowercase())
        }),
        SortBy::Mtime => Box::new(|a, b| {
            let ta = a
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let tb = b
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            ta.cmp(&tb).then_with(|| a.file_name().to_string_lossy().cmp(&b.file_name().to_string_lossy()))
        }),
    };

    let walker = WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .sort_by(cmp_fn);

    for entry_r in walker {
        let entry = match entry_r {
            Ok(e) => e,
            Err(e) => {
                warnings.push(format!("Walk error: {e}"));
                continue;
            }
        };

        let path = entry.path();
        let rel = match path.strip_prefix(root) {
            Ok(r) => r,
            Err(_) => continue,
        };

        if let Some(ref out_rel) = rel_out {
            if rel.starts_with(out_rel) {
                continue;
            }
        }

        if is_excluded(rel, cfg) {
            continue;
        }

        if entry.file_type().is_dir() {
            continue;
        }

        if entry.file_type().is_file() && path.extension() == Some(OsStr::new("md")) {
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(e) => {
                    warnings.push(format!("Cannot stat {}: {e}", path.display()));
                    continue;
                }
            };

            let size = meta.len();
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            results.push(MdFile {
                abs: path.to_path_buf(),
                rel: rel.to_path_buf(),
                size,
                mtime,
            });

            count += 1;
            if count % SCAN_TICK_BATCH == 0 {
                if let Some(pb) = scan_pb {
                    pb.set_message(format!("Scanning… ({count} files found)"));
                    pb.tick();
                }
            }
        }
    }

    if let Some(pb) = scan_pb {
        pb.set_message(format!("Scan complete — {count} .md file(s) found"));
        pb.finish_and_clear();
    }

    (results, warnings)
}

pub fn pack_into_groups(md_files: &[MdFile], cfg: &VaultConfig) -> (Vec<GroupPlan>, Vec<String>) {
    let mut groups = Vec::new();
    let mut warnings = Vec::new();

    for md in md_files.iter().cloned() {
        if md.size >= cfg.max_bytes {
            warnings.push(format!(
                "{} ({:.1} MB) >= limit {:.0} MB — placed in solo group.",
                path_to_posix(&md.rel),
                md.size as f64 / 1_048_576.0,
                cfg.max_bytes as f64 / 1_048_576.0,
            ));
            let mut solo = GroupPlan::new(groups.len() + 1);
            solo.add(md, cfg, true);
            groups.push(solo);
            continue;
        }

        if md.size + cfg.overhead_per_chapter >= cfg.max_bytes {
            warnings.push(format!(
                "{} + chapter overhead would reach/exceed limit — placed in solo group.",
                path_to_posix(&md.rel),
            ));
            let mut solo = GroupPlan::new(groups.len() + 1);
            solo.add(md, cfg, true);
            groups.push(solo);
            continue;
        }

        if let Some(g) = groups.last_mut() {
            if g.can_fit(&md, cfg) {
                g.add(md, cfg, false);
                continue;
            }
        }

        let mut new_group = GroupPlan::new(groups.len() + 1);
        new_group.add(md, cfg, false);
        groups.push(new_group);
    }

    (groups, warnings)
}

pub fn write_group(
    group: &GroupPlan,
    output_dir: &Path,
    cfg: &VaultConfig,
    checksums: &mut HashMap<String, String>,
    warnings: &mut Vec<String>,
    write_pb: &Option<ProgressBar>,
    verbose: bool,
) -> Result<WriteResult> {
    let filename = make_group_filename(&cfg.output_prefix, group.index);
    let final_path = output_dir.join(&filename);
    let total = group.chapter_count();

    let mut tmp = NamedTempFile::new_in(output_dir)
        .with_context(|| format!("Cannot create temp file in {}", output_dir.display()))?;
    let mut w = BufWriter::with_capacity(cfg.write_buffer, tmp.as_file_mut());

    writeln!(w, "---")?;
    writeln!(w, "vault_group: {}", group.index)?;
    writeln!(w, "chapters: {total}")?;
    writeln!(w, "---\n")?;
    writeln!(w, "# Vault Group {}\n", group.index)?;
    writeln!(w, "## Table of Contents\n")?;
    for (i, md) in group.files.iter().enumerate() {
        writeln!(w, "{}. `{}`", i + 1, path_to_posix(&md.rel))?;
    }
    writeln!(w)?;

    for (i, md) in group.files.iter().enumerate() {
        if i > 0 {
            w.write_all(cfg.chapter_separator.as_bytes())?;
        }

        let rel_posix = path_to_posix(&md.rel);
        writeln!(
            w,
            "# Chapter {} of {total}\n\n**source:** {}\n\n**File:** `{rel_posix}`\n",
            i + 1,
            rel_posix,
        )?;

        if verbose {
            eprintln!(" -> {}", md.abs.display());
        }

        let mut file = File::open(&md.abs)
            .with_context(|| format!("Cannot open {}", md.abs.display()))?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0_u8; cfg.read_chunk];
        let mut had_lossy = false;

        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            let chunk = &buf[..n];
            hasher.update(chunk);

            match std::str::from_utf8(chunk) {
                Ok(s) => w.write_all(s.as_bytes())?,
                Err(_) => {
                    had_lossy = true;
                    let lossy = String::from_utf8_lossy(chunk);
                    w.write_all(lossy.as_bytes())?;
                }
            }
        }

        w.write_all(b"\n")?;

        if had_lossy {
            warnings.push(format!(
                "Non-UTF-8 bytes in {} — replaced with U+FFFD.",
                rel_posix
            ));
        }

        checksums.insert(rel_posix, format!("{:x}", hasher.finalize()));

        if let Some(pb) = write_pb {
            pb.inc(1);
        }
    }

    w.flush()?;

    // Explicitly drop the writer so the borrow on `tmp` ends
    drop(w);
    
    tmp.persist(&final_path)
        .with_context(|| format!("Cannot persist to {}", final_path.display()))?;
    let bytes_written = fs::metadata(&final_path)?.len();

    Ok(WriteResult {
        group_index: group.index,
        path: final_path,
        bytes_written,
        chapter_count: group.chapter_count(),
        skipped: false,
    })
}

pub fn write_manifest(
    root: &Path,
    output_dir: &Path,
    cfg: &VaultConfig,
    groups: &[GroupPlan],
    write_results: &[WriteResult],
    checksums: &HashMap<String, String>,
    indent: bool,
) -> Result<PathBuf> {
    let result_map: HashMap<usize, &WriteResult> =
        write_results.iter().map(|wr| (wr.group_index, wr)).collect();

    let manifest_groups: Vec<ManifestGroup> = groups
        .iter()
        .map(|g| {
            let fname = make_group_filename(&cfg.output_prefix, g.index);
            let out_path = output_dir.join(&fname);

            let (bytes_written, skipped) = if let Some(wr) = result_map.get(&g.index) {
                (Some(wr.bytes_written), wr.skipped)
            } else {
                (fs::metadata(&out_path).map(|m| m.len()).ok(), true)
            };

            let chapters = g
                .files
                .iter()
                .enumerate()
                .map(|(i, md)| {
                    let rel_posix = path_to_posix(&md.rel);
                    let sha = checksums.get(&rel_posix).cloned().unwrap_or_default();
                    let mtime = md
                        .mtime
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or(Duration::ZERO)
                        .as_secs_f64();
                    ManifestChapter {
                        chapter: i + 1,
                        source: rel_posix,
                        bytes: md.size,
                        mtime,
                        sha256: sha,
                    }
                })
                .collect();

            ManifestGroup {
                index: g.index,
                filename: fname,
                bytes_written,
                skipped,
                chapters,
            }
        })
        .collect();

    let max_mb_rounded = (cfg.max_bytes as f64 / 1_048_576.0 * 10_000.0).round() / 10_000.0;

    let manifest = ManifestRoot {
        version: env!("CARGO_PKG_VERSION").to_string(),
        generated_at: OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string()),
        vault_root_posix: path_to_posix(root),
        vault_root_native: root.to_string_lossy().into_owned(),
        max_mb: max_mb_rounded,
        groups: manifest_groups,
    };

    let manifest_path = output_dir.join("vault_manifest.json");
    let file = File::create(&manifest_path)
        .with_context(|| format!("Cannot create {}", manifest_path.display()))?;
    let writer = BufWriter::new(file);

    if indent {
        serde_json::to_writer_pretty(writer, &manifest)?;
    } else {
        serde_json::to_writer(writer, &manifest)?;
    }

    Ok(manifest_path)
}

pub fn print_stats(root: &Path, md_files: &[MdFile], elapsed: f64) {
    println!("\nVault statistics: {}", root.display());
    if md_files.is_empty() {
        println!(" No .md files found.");
        println!(" Elapsed: {:.1}s", elapsed);
        return;
    }

    let total: u64 = md_files.iter().map(|f| f.size).sum();
    let max = md_files.iter().map(|f| f.size).max().unwrap_or(0);
    let min = md_files.iter().map(|f| f.size).min().unwrap_or(0);
    let avg = total as f64 / md_files.len() as f64;
    println!(" Files: {}", md_files.len());
    println!(" Total: {:.2} MB", total as f64 / 1_048_576.0);
    println!(" Average: {:.1} KB", avg / 1024.0);
    println!(" Largest: {:.2} MB", max as f64 / 1_048_576.0);
    println!(" Smallest:{} B", min);
    println!(" Elapsed: {:.1}s", elapsed);
}

pub fn make_scan_pb(mp: &MultiProgress, enabled: bool) -> Option<ProgressBar> {
    if !enabled {
        return None;
    }

    let pb = mp.add(ProgressBar::new_spinner());
    pb.set_style(
        ProgressStyle::with_template("{spinner} {msg}")
            .expect("valid template")
            .tick_chars("/-\\| "),
    );
    pb.set_message("Scanning vault…");
    Some(pb)
}

pub fn make_write_pb(mp: &MultiProgress, total: u64, enabled: bool) -> Option<ProgressBar> {
    if !enabled || total == 0 {
        return None;
    }

    let pb = mp.add(ProgressBar::new(total));
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner} Writing [{bar:40.cyan/blue}] {pos}/{len} chapters • {elapsed_precise} • ETA {eta_precise}",
        )
        .expect("valid template")
        .progress_chars("█▉▊▋▌▍▎▏ "),
    );
    Some(pb)
}

pub fn run(cli: Cli) -> Result<()> {
    if cli.quiet && cli.verbose {
        bail!("--quiet and --verbose are mutually exclusive.");
    }
    if cli.force && cli.resume {
        bail!("--force and --resume are mutually exclusive.");
    }
    if cli.dry_run && cli.stats_only {
        bail!("--dry-run and --stats-only are mutually exclusive.");
    }

    let cfg = VaultConfig::from_cli(&cli)?;

    if !cli.vault_root.is_dir() {
        bail!(
            "Vault root not found or not a directory: {}",
            cli.vault_root.display()
        );
    }

    let output_dir_raw = cli
        .output_dir
        .clone()
        .unwrap_or_else(|| cli.vault_root.join("_grouped"));

    if !cli.dry_run && !cli.stats_only {
        fs::create_dir_all(&output_dir_raw)
            .with_context(|| format!("Cannot create output dir: {}", output_dir_raw.display()))?;
    }

    let output_dir = output_dir_raw
        .canonicalize()
        .unwrap_or_else(|_| output_dir_raw.clone());

    let mp = MultiProgress::new();
    let show_pb = !cli.no_progress && !cli.quiet;
    let scan_pb = make_scan_pb(&mp, show_pb);

    let start = std::time::Instant::now();
    let (md_files, mut warnings) = discover_md_files(
        &cli.vault_root,
        if cli.dry_run || cli.stats_only {
            None
        } else {
            Some(&output_dir)
        },
        &cfg,
        &scan_pb,
    );

    let elapsed_scan = start.elapsed().as_secs_f64();
    let total_source_bytes: u64 = md_files.iter().map(|f| f.size).sum();

    if cli.stats_only {
        print_stats(&cli.vault_root, &md_files, elapsed_scan);
        return Ok(());
    }

    if md_files.is_empty() {
        if !cli.quiet {
            println!("No .md files found — nothing to do.");
        }
        return Ok(());
    }

    let (normal_files, oversized_files): (Vec<MdFile>, Vec<MdFile>) =
        md_files.into_iter().partition(|f| f.size < cfg.max_bytes);

    for f in &oversized_files {
        warnings.push(format!(
            "Oversized file ({}): {:.2} MB — not grouped.",
            path_to_posix(&f.rel),
            f.size as f64 / 1_048_576.0,
        ));
    }

    let (groups, pack_warnings) = pack_into_groups(&normal_files, &cfg);
    warnings.extend(pack_warnings);

    if cli.dry_run {
        println!("\n[DRY RUN] Planned output (no files written):");
        for g in &groups {
            println!(
                " [{:04}] {:<44} {:>5} chapter(s) ~{:.2} MB",
                g.index,
                make_group_filename(&cfg.output_prefix, g.index),
                g.chapter_count(),
                g.estimated_bytes as f64 / 1_048_576.0,
            );
        }
        println!(
            "\nTotal .md files: {} ({:.2} MB source) → {} group-file(s)",
            normal_files.len(),
            total_source_bytes as f64 / 1_048_576.0,
            groups.len(),
        );
        println!("Elapsed: {:.1}s", elapsed_scan);
        return Ok(());
    }

    if !cli.force && !cli.resume {
        let conflicts: Vec<PathBuf> = groups
            .iter()
            .map(|g| output_dir.join(make_group_filename(&cfg.output_prefix, g.index)))
            .filter(|p| p.exists())
            .collect();
        if !conflicts.is_empty() {
            let samples: Vec<String> = conflicts
                .iter()
                .take(5)
                .map(|p| p.file_name().unwrap_or_default().to_string_lossy().into_owned())
                .collect();
            let extra = if conflicts.len() > 5 {
                format!(" (+{} more)", conflicts.len() - 5)
            } else {
                String::new()
            };
            bail!(
                "Output file(s) already exist: {}{}\nUse --force to overwrite or --resume to skip.",
                samples.join(", "),
                extra
            );
        }
    }

    let total_chapters_to_write: u64 = groups
        .iter()
        .filter(|g| {
            let p = output_dir.join(make_group_filename(&cfg.output_prefix, g.index));
            !(cli.resume && p.exists())
        })
        .map(|g| g.chapter_count() as u64)
        .sum();

    let write_pb = make_write_pb(&mp, total_chapters_to_write, show_pb);

    let mut write_results: Vec<WriteResult> = Vec::new();
    let mut checksums: HashMap<String, String> = HashMap::new();

    for g in &groups {
        let out_path = output_dir.join(make_group_filename(&cfg.output_prefix, g.index));

        if cli.resume && out_path.exists() {
            let bw = fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
            write_results.push(WriteResult {
                group_index: g.index,
                path: out_path,
                bytes_written: bw,
                chapter_count: g.chapter_count(),
                skipped: true,
            });
            continue;
        }

        if cli.verbose {
            eprintln!(
                "Writing group {}/{} ({} chapters)…",
                g.index,
                groups.len(),
                g.chapter_count()
            );
        }

        let wr = write_group(
            g,
            &output_dir,
            &cfg,
            &mut checksums,
            &mut warnings,
            &write_pb,
            cli.verbose,
        )?;
        write_results.push(wr);
    }

    if let Some(pb) = &write_pb {
        pb.finish_and_clear();
    }

    let manifest_path = if cli.no_manifest {
        None
    } else {
        Some(write_manifest(
            &cli.vault_root,
            &output_dir,
            &cfg,
            &groups,
            &write_results,
            &checksums,
            cli.indent_manifest,
        )?)
    };

    if !cli.quiet {
        let written = write_results.iter().filter(|w| !w.skipped).count();
        let skipped = write_results.iter().filter(|w| w.skipped).count();
        let elapsed_total = start.elapsed().as_secs_f64();

        println!(
            "\nObsidian Vault Grouper v{} — Summary",
            env!("CARGO_PKG_VERSION")
        );
        println!(" Vault root     : {}", cli.vault_root.display());
        println!(" .md files      : {}", normal_files.len());
        println!(
            " Source total   : {:.2} MB",
            total_source_bytes as f64 / 1_048_576.0
        );
        println!(" Groups planned : {}", groups.len());
        println!(" Groups written : {written}");
        if skipped > 0 {
            println!(" Groups skipped : {skipped} (--resume)");
        }
        if let Some(p) = &manifest_path {
            println!(" Manifest       : {}", p.display());
        }
        println!(" Elapsed        : {:.1}s\n", elapsed_total);

        for wr in &write_results {
            let tag = if wr.skipped { "skipped" } else { "written" };
            println!(
                " [{tag:7}] {:<42} {:.2} MB",
                wr.path.file_name().unwrap_or_default().to_string_lossy(),
                wr.bytes_written as f64 / 1_048_576.0,
            );
        }

        if !warnings.is_empty() {
            println!("\n{} warning(s):", warnings.len());
            for w in warnings.iter().take(10) {
                println!(" - {w}");
            }
            if warnings.len() > 10 {
                println!(" ... and {} more", warnings.len() - 10);
            }
        }
    }

    Ok(())
}

pub fn run_cli() -> Result<()> {
    let cli = Cli::parse();
    run(cli)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use tempfile::TempDir;

    fn default_cfg() -> VaultConfig {
        VaultConfig {
            max_bytes: 10 * 1024 * 1024,
            chapter_separator: "\n\n---\n\n".to_string(),
            overhead_per_chapter: DEFAULT_OVERHEAD_PER_CHAPTER,
            read_chunk: DEFAULT_READ_CHUNK,
            write_buffer: DEFAULT_WRITE_BUFFER,
            sort_by: SortBy::Name,
            exclude: GlobSetBuilder::new().build().unwrap(),
            max_chapters: 0,
            output_prefix: "vault_group_".to_string(),
        }
    }

    fn make_md(rel: &str, size: u64) -> MdFile {
        MdFile {
            abs: PathBuf::from(format!("/vault/{rel}")),
            rel: PathBuf::from(rel),
            size,
            mtime: SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn pack_empty() {
        let cfg = default_cfg();
        let (groups, warnings) = pack_into_groups(&[], &cfg);
        assert!(groups.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn pack_single_file() {
        let cfg = default_cfg();
        let files = vec![make_md("a.md", 1024)];
        let (groups, warnings) = pack_into_groups(&files, &cfg);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].chapter_count(), 1);
        assert_eq!(groups[0].files[0].rel, PathBuf::from("a.md"));
        assert!(warnings.is_empty());
    }

    #[test]
    fn pack_no_duplication() {
        let cfg = default_cfg();
        let files: Vec<MdFile> = (0..20).map(|i| make_md(&format!("{i}.md"), 100)).collect();
        let (groups, _) = pack_into_groups(&files, &cfg);
        let mut seen = std::collections::HashSet::new();
        for g in &groups {
            for f in &g.files {
                let key = f.rel.to_string_lossy().into_owned();
                assert!(seen.insert(key.clone()), "Duplicate file: {key}");
            }
        }
        let total: usize = groups.iter().map(GroupPlan::chapter_count).sum();
        assert_eq!(total, 20);
    }

    #[test]
    fn pack_oversized_solo_group() {
        let cfg = default_cfg();
        let files = vec![make_md("giant.md", 11 * 1024 * 1024)];
        let (groups, warnings) = pack_into_groups(&files, &cfg);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].chapter_count(), 1);
        assert!(!warnings.is_empty());
    }

    #[test]
    fn pack_max_chapters_respected() {
        let mut cfg = default_cfg();
        cfg.max_chapters = 2;
        let files: Vec<MdFile> = (0..5).map(|i| make_md(&format!("{i}.md"), 100)).collect();
        let (groups, _) = pack_into_groups(&files, &cfg);
        for g in &groups {
            assert!(g.chapter_count() <= 2);
        }
    }

    proptest! {
        #[test]
        fn prop_pack_invariants(
            sizes in proptest::collection::vec(1u64..(1024 * 1024), 0..30),
            max_mb in 1u32..5u32,
        ) {
            let exclude = GlobSetBuilder::new().build().unwrap();
            let cfg = VaultConfig {
                max_bytes: (max_mb as u64) * 1024 * 1024,
                chapter_separator: "\n\n---\n\n".to_string(),
                overhead_per_chapter: DEFAULT_OVERHEAD_PER_CHAPTER,
                read_chunk: DEFAULT_READ_CHUNK,
                write_buffer: DEFAULT_WRITE_BUFFER,
                sort_by: SortBy::Name,
                exclude,
                max_chapters: 0,
                output_prefix: "vault_group_".to_string(),
            };

            let files: Vec<MdFile> = sizes
                .iter()
                .enumerate()
                .map(|(i, s)| make_md(&format!("{i}.md"), *s))
                .collect();

            let (groups, _warnings) = pack_into_groups(&files, &cfg);

            let mut seen = std::collections::HashSet::new();
            for g in &groups {
                for f in &g.files {
                    let key = f.rel.to_string_lossy().into_owned();
                    prop_assert!(seen.insert(key.clone()), "duplicate file {key}");
                }
            }
            prop_assert_eq!(seen.len(), files.len());

            for g in &groups {
                if g.chapter_count() == 1 && g.files[0].size >= cfg.max_bytes {
                    continue;
                }
                prop_assert!(g.estimated_bytes < cfg.max_bytes);
            }
        }
    }

    #[test]
    fn integration_group_round_trip() -> Result<()> {
        let vault = TempDir::new()?;
        let out = TempDir::new()?;

        let structure = [
            ("Alpha/n1.md", "# A1\nContent A1"),
            ("Alpha/n2.md", "# A2\nContent A2"),
            ("Beta/n1.md", "# B1\nContent B1"),
        ];

        for (rel, body) in &structure {
            let full = vault.path().join(rel);
            fs::create_dir_all(full.parent().unwrap())?;
            fs::write(&full, body)?;
        }

        let cfg = VaultConfig {
            max_bytes: 1024 * 1024,
            output_prefix: "vault_group_".to_string(),
            ..default_cfg()
        };

        let (md_files, warnings) = discover_md_files(vault.path(), Some(out.path()), &cfg, &None);
        assert!(warnings.is_empty());
        assert_eq!(md_files.len(), 3);

        let (groups, _) = pack_into_groups(&md_files, &cfg);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].chapter_count(), 3);

        let mut checksums = HashMap::new();
        let mut write_warnings = Vec::new();
        let wr = write_group(
            &groups[0],
            out.path(),
            &cfg,
            &mut checksums,
            &mut write_warnings,
            &None,
            false,
        )?;
        assert!(!wr.skipped);
        assert!(wr.bytes_written > 0);
        assert!(write_warnings.is_empty());

        let content = fs::read_to_string(&wr.path)?;
        for (rel, body) in &structure {
            let rel_posix = rel.replace('\\', "/");
            assert!(content.contains(&format!("**source:** {rel_posix}")));
            assert!(content.contains(body));
        }

        for (rel, _) in &structure {
            let rel_posix = rel.replace('\\', "/");
            let occ = content.matches(&format!("**source:** {rel_posix}")).count();
            assert_eq!(occ, 1);
        }

        assert_eq!(checksums.len(), 3);
        for (rel, _) in &structure {
            let rel_posix = rel.replace('\\', "/");
            assert_eq!(checksums[&rel_posix].len(), 64);
        }

        Ok(())
    }
}
