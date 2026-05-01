//! obsidian_vault_grouper v0.4.0
//! Hardened DAG-traversal with recursive-bubbling, sub-second sorting, and atomic IO.

#![forbid(unsafe_code)]
#![warn(clippy::pedantic)]

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufWriter, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use globset::{Glob, GlobSet, GlobSetBuilder};
use indicatif::{ProgressBar, ProgressStyle};
use serde::Serialize;
use tempfile::NamedTempFile;

// --- Configuration & Constants ---

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SortBy { Name, Mtime }

#[derive(Parser)]
#[command(version, about = "Group Obsidian vaults into AI-friendly markdown chapters.")]
pub struct Cli {
    pub vault_root: PathBuf,
    pub output_dir: Option<PathBuf>,
    #[arg(long, default_value_t = 20.0)]
    pub max_mb: f64,
    #[arg(long, default_value_t = 0)]
    pub max_chapters: usize,
    #[arg(long, value_enum, default_value_t = SortBy::Name)]
    pub sort_by: SortBy,
    #[arg(long)]
    pub exclude: Vec<String>,
    #[arg(long)]
    pub resume: bool,
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Clone, Debug)]
pub struct MdFile {
    pub abs: PathBuf,
    pub rel: PathBuf,
    pub size: u64,
    pub mtime: Duration, // Sub-second precision
}

pub struct VaultTree {
    pub files_by_folder: HashMap<PathBuf, Vec<MdFile>>,
    pub subfolders: HashMap<PathBuf, HashSet<PathBuf>>,
}

pub struct GroupPlan {
    pub files: Vec<MdFile>,
    pub estimated_bytes: u64,
}

// --- Discovery Phase ---

pub fn build_hardened_tree(root: &Path, output_dir: &Path, exclude: &GlobSet) -> Result<VaultTree> {
    let mut files_by_folder: HashMap<PathBuf, Vec<MdFile>> = HashMap::new();
    let mut subfolders: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();

    // Fix 2: Symlink protection
    for entry in walkdir::WalkDir::new(root)
        .min_depth(1)
        .follow_links(false) 
    {
        let e = entry?;
        let path = e.path();
        
        // Fix 11: Match against relative path
        let rel = path.strip_prefix(root).unwrap_or(path);
        if path.starts_with(output_dir) || exclude.is_match(rel) {
            continue;
        }

        if path.extension().map_or(false, |ext| ext == "md") && e.file_type().is_file() {
            let parent = rel.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
            
            // Fix 2: Explicitly link root to its first-level children
            let mut curr = parent.as_path();
            while let Some(p) = curr.parent() {
                subfolders.entry(p.to_path_buf()).or_default().insert(curr.to_path_buf());
                curr = p;
            }
            // Ensure root explicitly tracks its direct children if they are folders
            if let Some(p_of_parent) = parent.parent() {
                 subfolders.entry(p_of_parent.to_path_buf()).or_default().insert(parent.clone());
            }

            let meta = e.metadata()?;
            files_by_folder.entry(parent).or_default().push(MdFile {
                abs: path.to_path_buf(),
                rel: rel.to_path_buf(),
                size: meta.len(),
                mtime: meta.modified()?.duration_since(UNIX_EPOCH)?,
            });
        }
    }
    Ok(VaultTree { files_by_folder, subfolders })
}

// --- Packing Logic ---

fn pack_node(
    path: &Path,
    tree: &VaultTree,
    max_bytes: u64,
    max_chapters: usize,
    sort_by: SortBy,
) -> (Vec<GroupPlan>, Vec<MdFile>) {
    let mut finished = Vec::new();
    let mut bubbling = Vec::new();

    // Depth-first recursion
    if let Some(subs) = tree.subfolders.get(path) {
        let mut sorted_subs: Vec<_> = subs.iter().collect();
        sorted_subs.sort(); // Deterministic folder processing
        for sub in sorted_subs {
            let (mut child_groups, mut child_rem) = pack_node(sub, tree, max_bytes, max_chapters, sort_by);
            finished.append(&mut child_groups);
            bubbling.append(&mut child_rem);
        }
    }

    // Local files
    if let Some(locals) = tree.files_by_folder.get(path) {
        let mut sorted = locals.clone();
        match sort_by {
            SortBy::Name => sorted.sort_by(|a, b| a.rel.cmp(&b.rel)),
            SortBy::Mtime => sorted.sort_by(|a, b| a.mtime.cmp(&b.mtime)),
        }
        bubbling.extend(sorted);
    }

    let mut current = GroupPlan { files: Vec::new(), estimated_bytes: 0 };
    for f in bubbling {
        // Fix 4: Dynamic overhead calculation (Header + Path + TOC line)
        let overhead = 256 + (f.rel.to_string_lossy().len() as u64); 
        
        let fits_size = current.estimated_bytes + f.size + overhead < max_bytes;
        let fits_chapters = max_chapters == 0 || current.files.len() < max_chapters;

        if fits_size && fits_chapters {
            current.estimated_bytes += f.size + overhead;
            current.files.push(f);
        } else {
            if !current.files.is_empty() {
                finished.push(current);
            }
            current = GroupPlan { 
                estimated_bytes: f.size + overhead, 
                files: vec![f] 
            };
        }
    }

    (finished, current.files) // Fix 6: Correct bubbling of last group
}

// --- Output Generation ---

pub fn write_group_atomic(group: &GroupPlan, idx: usize, dest: &Path) -> Result<()> {
    let parent = dest.parent().context("Output file must have a parent directory")?; // Fix 9
    let mut tmp = NamedTempFile::new_in(parent)?;
    {
        let mut w = BufWriter::new(tmp.as_file_mut());
        writeln!(w, "---\nvault_group: {}\n---", idx)?;
        writeln!(w, "\n# Table of Contents")?;
        for (i, f) in group.files.iter().enumerate() {
            writeln!(w, "{}. {}", i + 1, f.rel.display())?;
        }

        for (i, f) in group.files.iter().enumerate() {
            // Fix 12: Size re-verification
            let mut source = File::open(&f.abs)?;
            let current_size = source.metadata()?.len();
            if current_size > f.size * 2 {
                eprintln!("Warning: File {} has doubled in size!", f.rel.display());
            }

            writeln!(w, "\n---\n# Chapter {}: {}\n", i + 1, f.rel.display())?;
            std::io::copy(&mut source, &mut w)?;
        }
    }
    tmp.persist(dest)?;
    Ok(())
}

pub fn run(cli: Cli) -> Result<()> {
    let out = cli.output_dir.clone().unwrap_or_else(|| cli.vault_root.join("_grouped"));
    fs::create_dir_all(&out)?;

    let mut gb = GlobSetBuilder::new();
    for p in cli.exclude { gb.add(Glob::new(&p)?); }
    let tree = build_hardened_tree(&cli.vault_root, &out, &gb.build()?)?;

    let (mut groups, last_rem) = pack_node(
        Path::new(""), 
        &tree, 
        (cli.max_mb * 1024.0 * 1024.0) as u64, 
        cli.max_chapters,
        cli.sort_by
    );
    if !last_rem.is_empty() {
        groups.push(GroupPlan { estimated_bytes: 0, files: last_rem });
    }

    if cli.dry_run {
        println!("Dry run: Created {} group plans.", groups.len());
        return Ok(());
    }

    let pb = ProgressBar::new(groups.len() as u64);
    pb.set_style(ProgressStyle::default_bar().template("[{bar:40}] {pos}/{len} ({percent}%) {msg}")?);

    for (i, g) in groups.iter().enumerate() {
        let dest = out.join(format!("pack_{:04}.md", i + 1));
        if cli.resume && dest.exists() { continue; }
        write_group_atomic(g, i + 1, &dest)?;
        pb.inc(1);
    }
    pb.finish_with_message("Vault grouping complete.");
    Ok(())
}

pub fn run_cli() -> Result<()> { run(Cli::parse()) }