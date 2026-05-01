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

pub const DEFAULT_MAX_MB: f64 = 20.0; // Optimized for Gemini's long context[cite: 2]
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
    about = "Groups Obsidian .md files by folder into Gemini-optimized context packs.",
    long_about = "Optimizes your Obsidian vault for AI tools like NotebookLM. It preserves folder-level \
                  affinity (e.g., keeping all 'Apparatus' or 'FLECS' logic together) while ensuring \
                  files stay within Gemini's preferred context thresholds."
)]
pub struct Cli {
    /// Root directory of your Obsidian vault.
    pub vault_root: PathBuf,

    /// Output directory for grouped files. (Default: <vault_root>/_grouped)
    pub output_dir: Option<PathBuf>,

    // --- TECHNICAL CONSTRAINTS ---
    /// Target maximum size for each group file in MB.
    /// Set to 20MB for optimal Gemini performance.
    #[arg(long, default_value_t = DEFAULT_MAX_MB, help_heading = "Technical Limits")]
    pub max_mb: f64,

    /// Maximum number of 'chapters' (original .md files) per group. 0 = unlimited.[cite: 2]
    #[arg(long, default_value_t = 0, help_heading = "Technical Limits")]
    pub max_chapters: usize,

    /// Sorting logic for files within a folder.[cite: 2]
    #[arg(long, value_enum, default_value_t = SortBy::Name, help_heading = "Technical Limits")]
    pub sort_by: SortBy,

    // --- WORKFLOW ---
    /// Overwrite existing files in the output directory.[cite: 2]
    #[arg(long, action = ArgAction::SetTrue, help_heading = "Workflow")]
    pub force: bool,

    /// Skip processing folders where the output file already exists.[cite: 2]
    #[arg(long, action = ArgAction::SetTrue, help_heading = "Workflow")]
    pub resume: bool,

    /// Print statistics and planned groups without writing any files.[cite: 2]
    #[arg(long, action = ArgAction::SetTrue, help_heading = "Workflow")]
    pub dry_run: bool,

    /// Print vault statistics only; do not write group-files.[cite: 2]
    #[arg(long, action = ArgAction::SetTrue, help_heading = "Workflow")]
    pub stats_only: bool,

    /// Glob patterns to skip (e.g., --exclude '**/templates/**').[cite: 2]
    #[arg(long = "exclude", help_heading = "Workflow")]
    pub exclude_patterns: Vec<String>,

    // --- OUTPUT STYLE ---
    /// Filename prefix for the resulting groups.[cite: 2]
    #[arg(long = "output-prefix", default_value = "folder_pack_", help_heading = "Output Style")]
    pub output_prefix: String,

    /// Disable the generation of vault_manifest.json.[cite: 2]
    #[arg(long = "no-manifest", action = ArgAction::SetTrue, help_heading = "Output Style")]
    pub no_manifest: bool,

    /// Indent the JSON manifest for human readability.[cite: 2]
    #[arg(long = "indent-manifest", action = ArgAction::SetTrue, help_heading = "Output Style")]
    pub indent_manifest: bool,

    // --- UI & LOGGING ---
    /// Extra diagnostic output on stderr.[cite: 2]
    #[arg(short, long, action = ArgAction::SetTrue)]
    pub verbose: bool,

    /// Suppress progress bars and summaries.[cite: 2]
    #[arg(short, long, action = ArgAction::SetTrue)]
    pub quiet: bool,

    /// Disable interactive progress bars for CI/CD environments.[cite: 2]
    #[arg(long = "no-progress", action = ArgAction::SetTrue)]
    pub no_progress: bool,

    // --- PLACEHOLDERS FOR YOUR INPUT ---
    // TODO: Add a flag to filter by project-specific frontmatter (e.g., status: "ready").[cite: 2]
    // TODO: Add a flag for 'Strict Folder Mode' vs 'Hybrid' (where small folders merge).[cite: 2]
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
    if cfg.exclude.is_empty() { return false; }
    let posix = path_to_posix(rel);
    if cfg.exclude.is_match(&posix) { return true; }
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

    let cmp_fn: Box<dyn Fn(&walkdir::DirEntry, &walkdir::DirEntry) -> std::cmp::Ordering + Send + Sync> = match cfg.sort_by {
        SortBy::Name => Box::new(|a, b| a.file_name().to_string_lossy().to_lowercase().cmp(&b.file_name().to_string_lossy().to_lowercase())),
        SortBy::Mtime => Box::new(|a, b| {
            let ta = a.metadata().ok().and_then(|m| m.modified().ok()).unwrap_or(SystemTime::UNIX_EPOCH);
            let tb = b.metadata().ok().and_then(|m| m.modified().ok()).unwrap_or(SystemTime::UNIX_EPOCH);
            ta.cmp(&tb).then_with(|| a.file_name().to_string_lossy().cmp(&b.file_name().to_string_lossy()))
        }),
    };

    let walker = WalkDir::new(root).min_depth(1).follow_links(false).sort_by(cmp_fn);

    for entry_r in walker {
        let entry = match entry_r { Ok(e) => e, Err(e) => { warnings.push(format!("Walk error: {e}")); continue; }};
        let path = entry.path();
        let rel = match path.strip_prefix(root) { Ok(r) => r, Err(_) => continue };
        if let Some(ref out_rel) = rel_out { if rel.starts_with(out_rel) { continue; } }
        if is_excluded(rel, cfg) { continue; }
        if entry.file_type().is_dir() { continue; }
        if entry.file_type().is_file() && path.extension() == Some(OsStr::new("md")) {
            let meta = match entry.metadata() { Ok(m) => m, Err(e) => { warnings.push(format!("Cannot stat {}: {e}", path.display())); continue; }};
            results.push(MdFile { abs: path.to_path_buf(), rel: rel.to_path_buf(), size: meta.len(), mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH) });
            count += 1;
            if count % SCAN_TICK_BATCH == 0 { if let Some(pb) = scan_pb { pb.set_message(format!("Scanning… ({count} files found)")); pb.tick(); } }
        }
    }
    if let Some(pb) = scan_pb { pb.finish_and_clear(); }
    (results, warnings)
}

/// Rewritten core logic: Recursive "Deep Pack" consolidation.
/// This bubbles up files from the deepest subdirectories and groups them
/// with parents until the size limit is hit.
pub fn pack_into_groups(md_files: &[MdFile], cfg: &VaultConfig) -> (Vec<GroupPlan>, Vec<String>) {
    // Explicitly type the Vec to resolve E0282
    let mut final_groups: Vec<GroupPlan> = Vec::new();
    let warnings: Vec<String> = Vec::new();

    // 1. Build the tree-like structure
    let mut tree: HashMap<PathBuf, Vec<MdFile>> = HashMap::new();
    let mut folders: Vec<PathBuf> = Vec::new();

    for md in md_files {
        let parent = md.rel.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
        if !tree.contains_key(&parent) {
            folders.push(parent.clone());
        }
        tree.entry(parent).or_default().push(md.clone());
    }

    // Sort folders by depth (longest paths first)[cite: 2]
    folders.sort_by(|a, b| b.components().count().cmp(&a.components().count()));

    let mut current_pack = GroupPlan::new(1);

    // 2. Process folders from the leaves up to the root[cite: 2]
    for folder in folders {
        if let Some(files) = tree.get(&folder) {
            for md in files {
                // If a single file is a behemoth, isolate it[cite: 2]
                if md.size >= cfg.max_bytes {
                    if !current_pack.files.is_empty() {
                        final_groups.push(current_pack);
                        current_pack = GroupPlan::new(final_groups.len() + 1);
                    }
                    current_pack.add(md.clone(), cfg, true);
                    final_groups.push(current_pack);
                    current_pack = GroupPlan::new(final_groups.len() + 1);
                    continue;
                }

                // Consolidation check[cite: 2]
                if !current_pack.can_fit(md, cfg) {
                    final_groups.push(current_pack);
                    current_pack = GroupPlan::new(final_groups.len() + 1);
                }
                current_pack.add(md.clone(), cfg, current_pack.files.is_empty());
            }
        }
    }

    if !current_pack.files.is_empty() {
        final_groups.push(current_pack);
    }

    (final_groups, warnings)
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

    let mut tmp = NamedTempFile::new_in(output_dir)?;
    let mut w = BufWriter::with_capacity(cfg.write_buffer, tmp.as_file_mut());

    writeln!(w, "---\nvault_group: {}\nchapters: {}\n---\n", group.index, total)?;
    writeln!(w, "# Vault Group {}\n\n## Table of Contents\n", group.index)?;
    for (i, md) in group.files.iter().enumerate() {
        writeln!(w, "{}. `{}`", i + 1, path_to_posix(&md.rel))?;
    }
    writeln!(w)?;

    for (i, md) in group.files.iter().enumerate() {
        if i > 0 { w.write_all(cfg.chapter_separator.as_bytes())?; }
        let rel_posix = path_to_posix(&md.rel);
        writeln!(w, "# Chapter {} of {total}\n\n**source:** {}\n", i + 1, rel_posix)?;

        let mut file = File::open(&md.abs)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0_u8; cfg.read_chunk];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 { break; }
            let chunk = &buf[..n];
            hasher.update(chunk);
            w.write_all(chunk)?;
        }
        w.write_all(b"\n")?;
        checksums.insert(rel_posix, format!("{:x}", hasher.finalize()));
        if let Some(pb) = write_pb { pb.inc(1); }
    }
    w.flush()?;
    drop(w);
    tmp.persist(&final_path)?;
    Ok(WriteResult { group_index: group.index, path: final_path, bytes_written: fs::metadata(&output_dir.join(&filename))?.len(), chapter_count: total, skipped: false })
}

pub fn write_manifest(root: &Path, output_dir: &Path, cfg: &VaultConfig, groups: &[GroupPlan], write_results: &[WriteResult], checksums: &HashMap<String, String>, indent: bool) -> Result<PathBuf> {
    let result_map: HashMap<usize, &WriteResult> = write_results.iter().map(|wr| (wr.group_index, wr)).collect();
    let manifest_groups: Vec<ManifestGroup> = groups.iter().map(|g| {
        let fname = make_group_filename(&cfg.output_prefix, g.index);
        let out_path = output_dir.join(&fname);
        let (bytes_written, skipped) = if let Some(wr) = result_map.get(&g.index) { (Some(wr.bytes_written), wr.skipped) } else { (fs::metadata(&out_path).map(|m| m.len()).ok(), true) };
        let chapters = g.files.iter().enumerate().map(|(i, md)| {
            let rel_posix = path_to_posix(&md.rel);
            ManifestChapter { chapter: i + 1, source: rel_posix.clone(), bytes: md.size, mtime: md.mtime.duration_since(SystemTime::UNIX_EPOCH).unwrap_or(Duration::ZERO).as_secs_f64(), sha256: checksums.get(&rel_posix).cloned().unwrap_or_default() }
        }).collect();
        ManifestGroup { index: g.index, filename: fname, bytes_written, skipped, chapters }
    }).collect();

    let manifest = ManifestRoot {
        version: env!("CARGO_PKG_VERSION").to_string(),
        generated_at: OffsetDateTime::now_utc().format(&Rfc3339).unwrap(),
        vault_root_posix: path_to_posix(root),
        vault_root_native: root.to_string_lossy().into_owned(),
        max_mb: (cfg.max_bytes as f64 / 1_048_576.0),
        groups: manifest_groups,
    };
    let manifest_path = output_dir.join("vault_manifest.json");
    let file = File::create(&manifest_path)?;
    if indent { serde_json::to_writer_pretty(BufWriter::new(file), &manifest)?; } else { serde_json::to_writer(BufWriter::new(file), &manifest)?; }
    Ok(manifest_path)
}

pub fn make_scan_pb(mp: &MultiProgress, enabled: bool) -> Option<ProgressBar> {
    if !enabled { return None; }
    let pb = mp.add(ProgressBar::new_spinner());
    pb.set_style(ProgressStyle::with_template("{spinner} {msg}").unwrap().tick_chars("/-\\| "));
    Some(pb)
}

pub fn make_write_pb(mp: &MultiProgress, total: u64, enabled: bool) -> Option<ProgressBar> {
    if !enabled || total == 0 { return None; }
    let pb = mp.add(ProgressBar::new(total));
    pb.set_style(ProgressStyle::with_template("{spinner} Writing [{bar:40.cyan/blue}] {pos}/{len}").unwrap().progress_chars("█▉▊▋▌▍▎▏ "));
    Some(pb)
}

pub fn run(cli: Cli) -> Result<()> {
    let cfg = VaultConfig::from_cli(&cli)?;
    let output_dir = cli.output_dir.clone().unwrap_or_else(|| cli.vault_root.join("_grouped"));
    if !cli.dry_run && !cli.stats_only { fs::create_dir_all(&output_dir)?; }

    let mp = MultiProgress::new();
    let scan_pb = make_scan_pb(&mp, !cli.no_progress && !cli.quiet);
    let (md_files, mut warnings) = discover_md_files(&cli.vault_root, Some(&output_dir), &cfg, &scan_pb);

    let (groups, pack_warnings) = pack_into_groups(&md_files, &cfg);
    warnings.extend(pack_warnings);

    if cli.dry_run {
        for g in &groups { println!("Group {:04}: {} files", g.index, g.chapter_count()); }
        return Ok(());
    }

    let mut checksums = HashMap::new();
    let mut write_results = Vec::new();
    let write_pb = make_write_pb(&mp, groups.iter().map(|g| g.chapter_count() as u64).sum(), !cli.no_progress && !cli.quiet);

    for g in &groups {
        write_results.push(write_group(g, &output_dir, &cfg, &mut checksums, &mut warnings, &write_pb, cli.verbose)?);
    }

    if !cli.no_manifest { write_manifest(&cli.vault_root, &output_dir, &cfg, &groups, &write_results, &checksums, cli.indent_manifest)?; }
    Ok(())
}

pub fn run_cli() -> Result<()> {
    run(Cli::parse())
}