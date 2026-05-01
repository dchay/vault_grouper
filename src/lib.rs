//! obsidian_vault_grouper v0.2.0
//! Library crate: core logic + CLI runner.
//! Optimized for Gemini/NotebookLM context windows using recursive folder grouping.

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

// --- Constants & Config ---

pub const DEFAULT_MAX_MB: f64 = 20.0; // Optimized for Gemini "Needle in a Haystack" performance
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
    #[arg(long, default_value_t = DEFAULT_MAX_MB, help_heading = "Technical Limits")]
    pub max_mb: f64,

    /// Maximum number of 'chapters' per group. 0 = unlimited.
    #[arg(long, default_value_t = 0, help_heading = "Technical Limits")]
    pub max_chapters: usize,

    /// Sorting logic for files within a folder.
    #[arg(long, value_enum, default_value_t = SortBy::Name, help_heading = "Technical Limits")]
    pub sort_by: SortBy,

    // --- WORKFLOW ---
    /// Overwrite existing files in the output directory.
    #[arg(long, action = ArgAction::SetTrue, help_heading = "Workflow")]
    pub force: bool,

    /// Skip processing if output file already exists.
    #[arg(long, action = ArgAction::SetTrue, help_heading = "Workflow")]
    pub resume: bool,

    /// Print statistics and planned groups without writing any files.
    #[arg(long, action = ArgAction::SetTrue, help_heading = "Workflow")]
    pub dry_run: bool,

    /// Print vault statistics only.
    #[arg(long, action = ArgAction::SetTrue, help_heading = "Workflow")]
    pub stats_only: bool,

    /// Glob patterns to skip (e.g., --exclude '**/templates/**').
    #[arg(long = "exclude", help_heading = "Workflow")]
    pub exclude_patterns: Vec<String>,

    // --- OUTPUT STYLE ---
    /// Filename prefix for the resulting groups.
    #[arg(long = "output-prefix", default_value = "folder_pack_", help_heading = "Output Style")]
    pub output_prefix: String,

    /// Disable the generation of vault_manifest.json.
    #[arg(long = "no-manifest", action = ArgAction::SetTrue, help_heading = "Output Style")]
    pub no_manifest: bool,

    /// Indent the JSON manifest for human readability.
    #[arg(long = "indent-manifest", action = ArgAction::SetTrue, help_heading = "Output Style")]
    pub indent_manifest: bool,

    // --- UI & LOGGING ---
    #[arg(short, long, action = ArgAction::SetTrue)]
    pub verbose: bool,

    #[arg(short, long, action = ArgAction::SetTrue)]
    pub quiet: bool,

    #[arg(long = "no-progress", action = ArgAction::SetTrue)]
    pub no_progress: bool,

    // TODO: Add a flag for "Depth Limit" to stop consolidation at a specific level.
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
    pub force: bool,
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
            force: cli.force,
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

    pub fn can_fit(&self, md: &MdFile, cfg: &VaultConfig) -> bool {
        if cfg.max_chapters > 0 && self.files.len() >= cfg.max_chapters {
            return false;
        }
        let overhead = if self.files.is_empty() { 0 } else { cfg.overhead_per_chapter };
        
        self.estimated_bytes
            .checked_add(md.size)
            .and_then(|s| s.checked_add(overhead))
            .map_or(false, |total| total < cfg.max_bytes)
    }

    pub fn add(&mut self, md: MdFile, cfg: &VaultConfig, is_solo: bool) {
        let overhead = if is_solo { 0 } else { cfg.overhead_per_chapter };
        self.estimated_bytes = self.estimated_bytes
            .checked_add(md.size)
            .and_then(|s| s.checked_add(overhead))
            .expect("Vault size calculation overflowed u64 limit");
        self.files.push(md);
    }
}

// --- Internal State Management ---

struct PackingState {
    io_buffer: Vec<u8>,
    next_group_index: usize,
    warnings: Vec<String>,
}

impl PackingState {
    fn new(buffer_size: usize) -> Self {
        Self {
            io_buffer: vec![0u8; buffer_size],
            next_group_index: 1,
            warnings: Vec::new(),
        }
    }

    fn get_and_inc_index(&mut self) -> usize {
        let idx = self.next_group_index;
        self.next_group_index += 1;
        idx
    }
}

// --- Logic Implementation ---

pub fn pack_into_groups(md_files: &[MdFile], cfg: &VaultConfig) -> (Vec<GroupPlan>, Vec<String>) {
    let mut state = PackingState::new(cfg.read_chunk);
    
    let mut sorted_files = md_files.to_vec();
    sorted_files.sort_by(|a, b| a.rel.cmp(&b.rel));

    let (mut groups, remainder) = pack_directory_recursive(
        Path::new(""), 
        cfg, 
        &sorted_files, 
        &mut state
    );

    if !remainder.is_empty() {
        let mut final_group = GroupPlan::new(state.get_and_inc_index());
        for md in remainder {
            final_group.add(md, cfg, final_group.files.is_empty());
        }
        groups.push(final_group);
    }

    (groups, state.warnings)
}

fn pack_directory_recursive(
    current_rel_path: &Path,
    cfg: &VaultConfig,
    all_files: &[MdFile],
    state: &mut PackingState,
) -> (Vec<GroupPlan>, Vec<MdFile>) {
    let mut finished_groups = Vec::new();
    let mut pending_from_children = Vec::new();

    // 1. Identify immediate subdirectories
    let mut subdirs: Vec<PathBuf> = all_files.iter()
        .filter(|f| f.rel.starts_with(current_rel_path) && f.rel.parent().unwrap_or(Path::new("")) != current_rel_path)
        .filter_map(|f| {
            f.rel.strip_prefix(current_rel_path).ok()
                .and_then(|p| p.components().next())
                .map(|c| current_rel_path.join(c))
        })
        .collect();
    subdirs.sort();
    subdirs.dedup();

    // 2. Recurse Bottom-up
    for subdir in subdirs {
        let (mut child_groups, mut child_rem) = 
            pack_directory_recursive(&subdir, cfg, all_files, state);
        finished_groups.append(&mut child_groups);
        pending_from_children.append(&mut child_rem);
    }

    // 3. Local file collection
    let local_files: Vec<MdFile> = all_files.iter()
        .filter(|f| f.rel.parent().unwrap_or(Path::new("")) == current_rel_path)
        .cloned()
        .collect();
    
    let mut to_pack = pending_from_children;
    to_pack.extend(local_files);

    // 4. Grouping with Behemoth handling
    let mut current_group = GroupPlan::new(state.next_group_index);
    let mut remainder = Vec::new();

    for md in to_pack {
        if md.size >= cfg.max_bytes {
            if !current_group.files.is_empty() {
                state.get_and_inc_index();
                finished_groups.push(current_group);
            }
            let mut solo_group = GroupPlan::new(state.get_and_inc_index());
            solo_group.add(md, cfg, true);
            finished_groups.push(solo_group);
            current_group = GroupPlan::new(state.next_group_index);
            continue;
        }

        if current_group.can_fit(&md, cfg) {
            current_group.add(md, cfg, current_group.files.is_empty());
        } else {
            if !current_group.files.is_empty() {
                state.get_and_inc_index();
                finished_groups.push(current_group);
            }
            current_group = GroupPlan::new(state.next_group_index);
            current_group.add(md, cfg, true);
        }
    }

    remainder.extend(current_group.files);
    (finished_groups, remainder)
}

pub fn write_group(
    group: &GroupPlan,
    output_dir: &Path,
    cfg: &VaultConfig,
    state: &mut PackingState,
    checksums: &mut HashMap<String, String>,
    write_pb: &Option<ProgressBar>,
) -> Result<u64> {
    let filename = format!("{}{:04}.md", cfg.output_prefix, group.index);
    let final_path = output_dir.join(&filename);

    if cfg.force && final_path.exists() {
        fs::remove_file(&final_path).context("Failed to remove existing file for --force overwrite")?;
    }

    let mut tmp = NamedTempFile::new_in(output_dir)?;
    {
        let mut w = BufWriter::with_capacity(cfg.write_buffer, tmp.as_file_mut());
        
        writeln!(w, "---\nvault_group: {}\nchapters: {}\n---\n", group.index, group.files.len())?;
        writeln!(w, "# Group {}\n\n## Table of Contents\n", group.index)?;
        for (i, f) in group.files.iter().enumerate() {
            writeln!(w, "{}. `{}`", i + 1, f.rel.display())?;
        }

        for (i, md) in group.files.iter().enumerate() {
            if i > 0 { w.write_all(cfg.chapter_separator.as_bytes())?; }
            
            // TOCTOU re-verification
            let mut f = File::open(&md.abs).context("Failed to open source file during writing phase")?;
            let live_size = f.metadata()?.len();
            if live_size > md.size * 2 {
                state.warnings.push(format!("Warning: File {} size doubled since discovery.", md.rel.display()));
            }

            writeln!(w, "\n# Chapter {}\n**Source:** `{}`\n", i + 1, md.rel.display())?;
            
            let mut hasher = Sha256::new();
            loop {
                let n = f.read(&mut state.io_buffer)?;
                if n == 0 { break; }
                let chunk = &state.io_buffer[..n];
                hasher.update(chunk);
                w.write_all(chunk)?;
            }
            checksums.insert(md.rel.to_string_lossy().into_owned(), format!("{:x}", hasher.finalize()));
            if let Some(pb) = write_pb { pb.inc(1); }
        }
        w.flush()?;
    }
    
    tmp.persist(&final_path).context("Failed to persist group file to disk")?;
    Ok(fs::metadata(final_path)?.len())
}

// --- Discovery ---

pub fn discover_md_files(
    root: &Path,
    exclude_out_dir: Option<&Path>,
    cfg: &VaultConfig,
    scan_pb: &Option<ProgressBar>,
) -> (Vec<MdFile>, Vec<String>) {
    let mut results = Vec::new();
    let mut warnings = Vec::new();
    let mut count = 0usize;

    let rel_out = exclude_out_dir.and_then(|p| p.strip_prefix(root).ok());

    let walker = WalkDir::new(root).min_depth(1).follow_links(false).sort_by(|a, b| {
        a.file_name().cmp(b.file_name())
    });

    for entry_r in walker {
        let entry = match entry_r { Ok(e) => e, Err(e) => { warnings.push(format!("Walk error: {e}")); continue; }};
        let path = entry.path();
        let rel = match path.strip_prefix(root) { Ok(r) => r, Err(_) => continue };
        
        if let Some(out_rel) = rel_out { if rel.starts_with(out_rel) { continue; } }
        if entry.file_type().is_dir() { continue; }
        if path.extension() == Some(OsStr::new("md")) {
            if let Ok(meta) = entry.metadata() {
                results.push(MdFile {
                    abs: path.to_path_buf(),
                    rel: rel.to_path_buf(),
                    size: meta.len(),
                    mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                });
                count += 1;
                if count % SCAN_TICK_BATCH == 0 {
                    if let Some(pb) = scan_pb { pb.set_message(format!("Found {count} files...")); }
                }
            }
        }
    }
    (results, warnings)
}

// --- Manifest ---

#[derive(Debug, Serialize)]
struct Manifest {
    generated_at: String,
    files_packed: usize,
    groups: usize,
    checksums: HashMap<String, String>,
}

pub fn write_manifest(output_dir: &Path, files_count: usize, groups_count: usize, checksums: HashMap<String, String>, indent: bool) -> Result<()> {
    let manifest = Manifest {
        generated_at: OffsetDateTime::now_utc().format(&Rfc3339).unwrap(),
        files_packed: files_count,
        groups: groups_count,
        checksums,
    };
    let path = output_dir.join("vault_manifest.json");
    let file = File::create(path)?;
    if indent {
        serde_json::to_writer_pretty(file, &manifest)?;
    } else {
        serde_json::to_writer(file, &manifest)?;
    }
    Ok(())
}

// --- Runner ---

pub fn run(cli: Cli) -> Result<()> {
    let cfg = VaultConfig::from_cli(&cli)?;
    let output_dir = cli.output_dir.clone().unwrap_or_else(|| cli.vault_root.join("_grouped"));
    fs::create_dir_all(&output_dir)?;

    let mp = MultiProgress::new();
    let scan_pb = if cli.quiet || cli.no_progress { None } else {
        let pb = mp.add(ProgressBar::new_spinner());
        pb.set_style(ProgressStyle::default_spinner());
        Some(pb)
    };

    let (md_files, mut warnings) = discover_md_files(&cli.vault_root, Some(&output_dir), &cfg, &scan_pb);
    if let Some(pb) = scan_pb { pb.finish_and_clear(); }

    let (groups, pack_warnings) = pack_into_groups(&md_files, &cfg);
    warnings.extend(pack_warnings);

    if cli.dry_run || cli.stats_only {
        println!("Dry run: Found {} groups across {} files.", groups.len(), md_files.len());
        return Ok(());
    }

    let mut state = PackingState::new(cfg.read_chunk);
    let mut checksums = HashMap::new();
    let write_pb = if cli.quiet || cli.no_progress { None } else {
        let pb = mp.add(ProgressBar::new(md_files.len() as u64));
        pb.set_style(ProgressStyle::default_bar().template("{bar:40} {pos}/{len} {msg}").unwrap());
        Some(pb)
    };

    for g in &groups {
        write_group(g, &output_dir, &cfg, &mut state, &mut checksums, &write_pb)?;
    }
    
    if let Some(pb) = write_pb { pb.finish_with_message("Done!"); }

    if !cli.no_manifest {
        write_manifest(&output_dir, md_files.len(), groups.len(), checksums, cli.indent_manifest)?;
    }

    for w in warnings { eprintln!("Warning: {w}"); }
    for w in state.warnings { eprintln!("Warning: {w}"); }

    Ok(())
}

pub fn run_cli() -> Result<()> {
    run(Cli::parse())
}