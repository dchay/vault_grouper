//! obsidian_vault_grouper v0.3.0
//! DAG-based folder grouping with deterministic sorting and AI-optimized output.

#![forbid(unsafe_code)]
#![warn(clippy::pedantic)]

use std::{
    collections::HashMap,
    fs::{self, File},
    io::{BufWriter, Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use globset::{Glob, GlobSet, GlobSetBuilder};
use indicatif::{ProgressBar, ProgressStyle};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

// --- Types ---

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SortBy { Name, Mtime }

#[derive(Parser)]
#[command(version, about)]
pub struct Cli {
    pub vault_root: PathBuf,
    pub output_dir: Option<PathBuf>,
    #[arg(long, default_value_t = 20.0)]
    pub max_mb: f64,
    #[arg(long, value_enum, default_value_t = SortBy::Name)]
    pub sort_by: SortBy,
    #[arg(long)]
    pub exclude: Vec<String>,
    #[arg(long)]
    pub resume: bool,
    #[arg(long)]
    pub force: bool,
}

pub struct VaultTree {
    pub files: HashMap<PathBuf, Vec<MdFile>>,
    pub children: HashMap<PathBuf, Vec<PathBuf>>,
}

#[derive(Clone)]
pub struct MdFile {
    pub abs: PathBuf,
    pub rel: PathBuf,
    pub size: u64,
    pub mtime: u64,
}

pub struct GroupPlan {
    pub files: Vec<MdFile>,
    pub bytes: u64,
}

// --- Logic ---

/// Builds the folder adjacency list and file map in O(n).
pub fn build_vault_tree(root: &Path, output_dir: &Path, exclude: &GlobSet) -> VaultTree {
    // Explicitly define the types for the compiler
    let mut files: HashMap<PathBuf, Vec<MdFile>> = HashMap::new();
    let mut children: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();

    for entry in walkdir::WalkDir::new(root).min_depth(1) {
        let Ok(e) = entry else { continue };
        let path = e.path();
        if path.starts_with(output_dir) || exclude.is_match(path) { continue; }

        if path.extension().map_or(false, |ext| ext == "md") && e.file_type().is_file() {
            let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
            let parent = rel.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
            
            // Build child adjacency
            let mut curr = parent.as_path();
            while let Some(p) = curr.parent() {
                children.entry(p.to_path_buf()).or_default().push(curr.to_path_buf());
                curr = p;
            }

            let meta = e.metadata().ok();
            files.entry(parent).or_default().push(MdFile {
                abs: path.to_path_buf(),
                rel,
                size: meta.as_ref().map_or(0, |m| m.len()),
                mtime: meta.and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs()),
            });
        }
    }

    // Dedup children lists
    for v in children.values_mut() {
        v.sort();
        v.dedup();
    }
    VaultTree { files, children }
}

/// Recursively packs folders using bottom-up affinity.
fn pack_folder(
    path: &Path,
    tree: &VaultTree,
    max_bytes: u64,
    sort_by: SortBy,
) -> (Vec<GroupPlan>, Vec<MdFile>) {
    let mut finished = Vec::new();
    let mut bubbling = Vec::new();

    // 1. Process children first (Depth-First)
    if let Some(subs) = tree.children.get(path) {
        for sub in subs {
            let (mut c_groups, mut c_rem) = pack_folder(sub, tree, max_bytes, sort_by);
            finished.append(&mut c_groups);
            bubbling.append(&mut c_rem);
        }
    }

    // 2. Add local files
    if let Some(locals) = tree.files.get(path) {
        let mut sorted = locals.clone();
        match sort_by {
            SortBy::Name => sorted.sort_by(|a, b| a.rel.cmp(&b.rel)),
            SortBy::Mtime => sorted.sort_by(|a, b| a.mtime.cmp(&b.mtime)),
        }
        bubbling.extend(sorted);
    }

    // 3. Pack everything bubbling at this level
    let mut current = GroupPlan { files: Vec::new(), bytes: 0 };
    let mut remainder = Vec::new();

    for f in bubbling {
        // Use a reference or clone here so 'f' isn't consumed prematurely
        if f.size + 1024 > max_bytes {
            finished.push(GroupPlan { files: vec![f.clone()], bytes: 0 });
            continue;
        }

        if current.bytes + f.size + 1024 < max_bytes {
            current.bytes += f.size + 1024;
            current.files.push(f); // 'f' is moved here
        } else {
            finished.push(current);
            // Since the previous 'current' was pushed, we start a new one
            // We must clone 'f' if we intend to move it into a new group after a move check
            current = GroupPlan {
                bytes: f.size + 1024,
                files: vec![f],
            };
        }
    }

    remainder.extend(current.files);
    (finished, remainder)
}

/// Restores the AI-optimized separators and TOC.
pub fn write_group(group: &GroupPlan, idx: usize, dest: &Path) -> Result<()> {
    let mut tmp = NamedTempFile::new_in(dest.parent().unwrap())?;
    {
        let mut w = BufWriter::new(tmp.as_file_mut());
        writeln!(w, "---\nvault_group: {}\n---", idx)?;
        writeln!(w, "\n# Table of Contents")?;
        for (i, f) in group.files.iter().enumerate() {
            writeln!(w, "{}. {}", i + 1, f.rel.display())?;
        }

        for (i, f) in group.files.iter().enumerate() {
            writeln!(w, "\n---\n# Chapter {}: {}\n", i + 1, f.rel.display())?;
            let mut source = File::open(&f.abs)?;
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
    let tree = build_vault_tree(&cli.vault_root, &out, &gb.build()?);

    let (mut groups, rem) = pack_folder(Path::new(""), &tree, (cli.max_mb * 1024.0 * 1024.0) as u64, cli.sort_by);
    if !rem.is_empty() { groups.push(GroupPlan { files: rem, bytes: 0 }); }

    let pb = ProgressBar::new(groups.len() as u64);
    pb.set_style(ProgressStyle::default_bar().template("[{bar:40}] {pos}/{len} {msg}")?);

    for (i, g) in groups.iter().enumerate() {
        let dest = out.join(format!("pack_{:04}.md", i + 1));
        if cli.resume && dest.exists() { continue; }
        write_group(g, i + 1, &dest)?;
        pb.inc(1);
    }
    pb.finish_with_message("Done");
    Ok(())
}

pub fn run_cli() -> Result<()> { run(Cli::parse()) }