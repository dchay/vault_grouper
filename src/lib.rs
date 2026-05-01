//! obsidian_vault_grouper v0.2.1
//! Refactored for SDET-verified reliability.

#![forbid(unsafe_code)]
#![warn(clippy::pedantic)]

use std::{
    collections::HashMap,
    ffi::OsStr,
    fs::{self, File},
    io::{BufWriter, Read, Write},
    path::{Path, PathBuf},
    time::SystemTime,
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

pub const MAX_RECURSION_DEPTH: usize = 500;
pub const DEFAULT_MAX_MB: f64 = 20.0;
pub const CHAPTER_OVERHEAD_ESTIMATE: u64 = 1024; // Increased for path safety
pub const DEFAULT_READ_CHUNK: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SortBy { Name, Mtime }

#[derive(Debug, Parser)]
pub struct Cli {
    pub vault_root: PathBuf,
    pub output_dir: Option<PathBuf>,

    #[arg(long, default_value_t = DEFAULT_MAX_MB)]
    pub max_mb: f64,

    #[arg(long, default_value_t = 0)]
    pub max_chapters: usize,

    #[arg(long, value_enum, default_value_t = SortBy::Name)]
    pub sort_by: SortBy,

    #[arg(long, action = ArgAction::SetTrue)]
    pub force: bool,

    #[arg(long, action = ArgAction::SetTrue)]
    pub resume: bool,

    #[arg(long, action = ArgAction::SetTrue)]
    pub dry_run: bool,

    #[arg(long = "exclude")]
    pub exclude_patterns: Vec<String>,

    #[arg(long = "output-prefix", default_value = "folder_pack_")]
    pub output_prefix: String,

    #[arg(long = "no-manifest", action = ArgAction::SetTrue)]
    pub no_manifest: bool,
}

pub struct VaultConfig {
    pub max_bytes: u64,
    pub read_chunk: usize,
    pub exclude: GlobSet,
    pub max_chapters: usize,
    pub output_prefix: String,
    pub force: bool,
    pub resume: bool,
}

#[derive(Debug, Clone)]
pub struct MdFile {
    pub abs: PathBuf,
    pub rel: PathBuf,
    pub size: u64,
}

pub struct GroupPlan {
    pub files: Vec<MdFile>,
    pub estimated_bytes: u64,
}

impl GroupPlan {
    pub fn new() -> Self {
        Self { files: Vec::new(), estimated_bytes: 0 }
    }

    pub fn can_fit(&self, md: &MdFile, cfg: &VaultConfig) -> bool {
        if cfg.max_chapters > 0 && self.files.len() >= cfg.max_chapters { return false; }
        let overhead = if self.files.is_empty() { 0 } else { CHAPTER_OVERHEAD_ESTIMATE };
        self.estimated_bytes + md.size + overhead < cfg.max_bytes
    }

    pub fn add(&mut self, md: MdFile) {
        let overhead = if self.files.is_empty() { 0 } else { CHAPTER_OVERHEAD_ESTIMATE };
        self.estimated_bytes += md.size + overhead;
        self.files.push(md);
    }
}

// --- Discovery with O(n) Tree Construction ---

pub fn discover_and_map(root: &Path, output_dir: &Path, cfg: &VaultConfig) -> (HashMap<PathBuf, Vec<MdFile>>, Vec<String>) {
    let mut tree: HashMap<PathBuf, Vec<MdFile>> = HashMap::new();
    let mut warnings = Vec::new();

    for entry in WalkDir::new(root).min_depth(1).follow_links(false) {
        let entry = match entry { Ok(e) => e, Err(e) => { warnings.push(e.to_string()); continue; }};
        let path = entry.path();
        
        // 1. Isolation: Exclude output_dir even if outside root
        if path.starts_with(output_dir) { continue; }

        let rel = path.strip_prefix(root).unwrap_or(path);
        
        // 2. Exclusion logic: Re-implemented
        if cfg.exclude.is_match(rel) { continue; }
        
        if path.extension() == Some(OsStr::new("md")) && entry.file_type().is_file() {
            let parent = rel.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
            let md = MdFile {
                abs: path.to_path_buf(),
                rel: rel.to_path_buf(),
                size: entry.metadata().map(|m| m.len()).unwrap_or(0),
            };
            tree.entry(parent).or_default().push(md);
        }
    }
    (tree, warnings)
}

// --- Recursive Merging (Bottom-Up) ---

fn pack_node(
    current_path: &Path,
    tree: &HashMap<PathBuf, Vec<MdFile>>,
    cfg: &VaultConfig,
    depth: usize,
) -> (Vec<GroupPlan>, Vec<MdFile>) {
    if depth > MAX_RECURSION_DEPTH { return (vec![], vec![]); }

    let mut finished_groups = Vec::new();
    let mut pending_files = Vec::new();

    // Recurse into children
    for (path, _) in tree {
        if path.parent().unwrap_or_else(|| Path::new("")) == current_path {
            let (mut child_groups, mut child_rem) = pack_node(path, tree, cfg, depth + 1);
            finished_groups.append(&mut child_groups);
            pending_files.append(&mut child_rem);
        }
    }

    // Add local files
    if let Some(locals) = tree.get(current_path) {
        pending_files.extend(locals.clone());
    }

    // Pack collected files for this sub-tree
    let mut current_group = GroupPlan::new();
    let mut remaining = Vec::new();

    for md in pending_files {
        if md.size >= cfg.max_bytes {
            let mut solo = GroupPlan::new();
            solo.add(md);
            finished_groups.push(solo);
            continue;
        }

        if current_group.can_fit(&md, cfg) {
            current_group.add(md);
        } else {
            finished_groups.push(current_group);
            current_group = GroupPlan::new();
            current_group.add(md);
        }
    }

    remaining.extend(current_group.files);
    (finished_groups, remaining)
}

// --- Execution & IO ---

pub fn run(cli: Cli) -> Result<()> {
    let output_dir = cli.output_dir.clone().unwrap_or_else(|| cli.vault_root.join("_grouped"));
    fs::create_dir_all(&output_dir)?;

    let mut builder = GlobSetBuilder::new();
    for p in &cli.exclude_patterns { builder.add(Glob::new(p)?); }
    let cfg = VaultConfig {
        max_bytes: (cli.max_mb * 1_048_576.0) as u64,
        read_chunk: DEFAULT_READ_CHUNK,
        exclude: builder.build()?,
        max_chapters: cli.max_chapters,
        output_prefix: cli.output_prefix,
        force: cli.force,
        resume: cli.resume,
    };

    let (tree, mut warnings) = discover_and_map(&cli.vault_root, &output_dir, &cfg);
    let (mut groups, final_rem) = pack_node(Path::new(""), &tree, &cfg, 0);
    
    if !final_rem.is_empty() {
        let mut g = GroupPlan::new();
        for f in final_rem { g.add(f); }
        groups.push(g);
    }

    if cli.dry_run {
        println!("Dry Run: {} groups planned.", groups.len());
        return Ok(());
    }

    let mut io_buffer = vec![0u8; cfg.read_chunk];
    for (i, group) in groups.iter().enumerate() {
        let index = i + 1;
        let dest = output_dir.join(format!("{}{:04}.md", cfg.output_prefix, index));

        if cfg.resume && dest.exists() { continue; }
        if cfg.force && dest.exists() { fs::remove_file(&dest)?; }

        let mut tmp = NamedTempFile::new_in(&output_dir)?;
        {
            let mut writer = BufWriter::new(tmp.as_file_mut());
            for md in &group.files {
                let mut f = File::open(&md.abs)?;
                // Re-verify size
                if f.metadata()?.len() > cfg.max_bytes {
                    warnings.push(format!("File {} grew beyond limit!", md.rel.display()));
                }
                std::io::copy(&mut f, &mut writer)?;
            }
        }
        tmp.persist(dest)?;
    }

    Ok(())
}