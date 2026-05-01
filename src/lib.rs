//! obsidian_vault_grouper v1.0.0
//! Production-grade FLECS/Apparatus/Mass hybrid architecture.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufWriter, Write, Read},
    path::{Path, PathBuf},
    sync::Arc,
    time::UNIX_EPOCH,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use globset::{Glob, GlobSet, GlobSetBuilder};
use indicatif::{ProgressBar, ProgressStyle};
use serde::Serialize;
use tempfile::NamedTempFile;

// --- Models (FLECS State) ---

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
    pub manifest: bool,
    #[arg(long)]
    pub verbose: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct MdFile {
    pub abs: PathBuf,
    pub rel: PathBuf,
    pub size: u64,
    pub mtime_ns: u128,
}

pub struct VaultTree {
    pub files_by_folder: HashMap<PathBuf, Vec<MdFile>>,
    pub subfolders: HashMap<PathBuf, HashSet<PathBuf>>,
}

#[derive(Serialize)]
pub struct GroupPlan {
    pub files: Vec<MdFile>,
    pub estimated_bytes: u64,
}

// --- Discovery (Apparatus / Nervous System) ---

pub fn build_tree(root: &Path, output_dir: &Path, exclude: &GlobSet) -> Result<VaultTree> {
    let mut files_by_folder: HashMap<PathBuf, Vec<MdFile>> = HashMap::new();
    let mut subfolders: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();

    // Fix: Ensure the root exists in the subfolder map for recursion
    subfolders.insert(PathBuf::from(""), HashSet::new());

    let abs_root = root.canonicalize().context("Failed to canonicalize vault root")?;
    let abs_output = if output_dir.exists() {
        output_dir.canonicalize().ok()
    } else {
        None
    };

    for entry in walkdir::WalkDir::new(&abs_root).min_depth(1).follow_links(false) {
        let e = match entry {
            Ok(val) => val,
            Err(err) => {
                eprintln!("Warning: Skipping unreadable entry: {}", err);
                continue;
            }
        };

        let path = e.path();
        if let Some(ref out) = abs_output {
            if path.starts_with(out) { continue; }
        }

        let rel = path.strip_prefix(&abs_root).unwrap_or(path).to_path_buf();
        if exclude.is_match(&rel) { continue; }

        if path.extension().map_or(false, |ext| ext == "md") && e.file_type().is_file() {
            let parent = rel.parent().unwrap_or_else(|| Path::new("")).to_path_buf();

            // Build hierarchy pointers
            let mut curr = rel.as_path();
            while let Some(p) = curr.parent() {
                subfolders.entry(p.to_path_buf()).or_default().insert(curr.to_path_buf());
                curr = p;
            }

            let meta = e.metadata()?;
            files_by_folder.entry(parent).or_default().push(MdFile {
                abs: path.to_path_buf(),
                rel: rel.clone(),
                size: meta.len(),
                mtime_ns: meta.modified()?.duration_since(UNIX_EPOCH)?.as_nanos(),
            });
        }
    }
    Ok(VaultTree { files_by_folder, subfolders })
}

// --- Packing (Mass / Presentation Layer) ---


fn pack_node(
    path: &Path,
    tree: &VaultTree,
    max_bytes: u64,
    max_chapters: usize,
    sort_by: SortBy,
) -> (Vec<GroupPlan>, GroupPlan) {
    let mut finished = Vec::new();
    let mut bubbling = Vec::new();

    // 1. Recurse children
    if let Some(subs) = tree.subfolders.get(path) {
        let mut sorted_subs: Vec<_> = subs.iter().collect();
        sorted_subs.sort();
        for sub in sorted_subs {
            let (mut child_groups, child_rem) = pack_node(sub, tree, max_bytes, max_chapters, sort_by);
            finished.append(&mut child_groups);
            bubbling.extend(child_rem.files);
        }
    }

    // 2. Add local files
    if let Some(locals) = tree.files_by_folder.get(path) {
        let mut sorted = locals.clone();
        match sort_by {
            SortBy::Name => sorted.sort_by(|a, b| a.rel.cmp(&b.rel)),
            SortBy::Mtime => sorted.sort_by(|a, b| a.mtime_ns.cmp(&b.mtime_ns)),
        }
        bubbling.extend(sorted);
    }

    // 3. Batching (Fixing the Move error)
    let mut current = GroupPlan { files: Vec::new(), estimated_bytes: 32 };
    for f in bubbling {
        let overhead = 24 + (f.rel.to_string_lossy().len() as u64 * 2);
        let f_size = f.size; // Copy size to avoid needing 'f' after move

        // Check for Behemoths
        if f_size + overhead > max_bytes {
            eprintln!("Warning: File {} ({} bytes) exceeds limit.", f.rel.display(), f_size);
            // If we have a current group, close it first
            if !current.files.is_empty() {
                finished.push(current);
                current = GroupPlan { files: Vec::new(), estimated_bytes: 32 };
            }
            // Move 'f' into its own isolated group
            finished.push(GroupPlan { estimated_bytes: f_size + overhead, files: vec![f] });
            continue; // 'f' is moved, jump to next iteration
        }

        let fits_size = current.estimated_bytes + f_size + overhead < max_bytes;
        let fits_chapters = max_chapters == 0 || current.files.len() < max_chapters;

        if fits_size && fits_chapters {
            current.estimated_bytes += f_size + overhead;
            current.files.push(f); // Move happens here
        } else {
            if !current.files.is_empty() {
                finished.push(current);
            }
            // Move 'f' into a fresh current group
            current = GroupPlan { files: vec![f], estimated_bytes: f_size + overhead + 32 };
        }
    }

    (finished, current)
}

pub fn run(cli: Cli) -> Result<()> {
    if cli.max_mb <= 0.0 { bail!("max_mb must be greater than zero"); }

    let out = cli.output_dir.clone().unwrap_or_else(|| cli.vault_root.join("_grouped"));
    fs::create_dir_all(&out)?;

    let mut gb = GlobSetBuilder::new();
    for p in cli.exclude { gb.add(Glob::new(&p)?); }
    let tree = build_tree(&cli.vault_root, &out, &gb.build()?)?;

    let max_b = (cli.max_mb * 1024.0 * 1024.0) as u64;
    let (mut groups, last) = pack_node(Path::new(""), &tree, max_b, cli.max_chapters, cli.sort_by);
    if !last.files.is_empty() { groups.push(last); }

    let pb = ProgressBar::new(groups.iter().map(|g| g.files.len()).sum::<usize>() as u64);
    pb.set_style(ProgressStyle::default_bar().template("[{bar:40}] {pos}/{len} ({percent}%) {msg}")?);

    for (i, g) in groups.iter().enumerate() {
        let dest = out.join(format!("pack_{:04}.md", i + 1));
        if cli.resume && dest.exists() {
            pb.inc(g.files.len() as u64);
            continue;
        }

        let mut tmp = NamedTempFile::new_in(&out)?;
        {
            let mut w = BufWriter::new(tmp.as_file_mut());
            writeln!(w, "---\nvault_group: {}\n---", i + 1)?;
            writeln!(w, "\n# Table of Contents")?;
            for (idx, f) in g.files.iter().enumerate() {
                writeln!(w, "{}. {}", idx + 1, f.rel.display())?;
            }

            for (idx, f) in g.files.iter().enumerate() {
                let mut src = File::open(&f.abs)?;
                writeln!(w, "\n---\n# Chapter {}: {}\n", idx + 1, f.rel.display())?;
                std::io::copy(&mut src, &mut w)?;
                pb.inc(1);
            }
        }
        tmp.persist(dest)?;
    }

    pb.finish_with_message("Vault processing complete.");
    Ok(())
}

pub fn run_cli() -> Result<()> { run(Cli::parse()) }