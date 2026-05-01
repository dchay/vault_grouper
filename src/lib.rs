//! obsidian_vault_grouper v1.1.0
//! Production-grade FLECS/Apparatus/Mass hybrid architecture.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use globset::{Glob, GlobSet, GlobSetBuilder};
use indicatif::{ProgressBar, ProgressStyle};
use serde::Serialize;
use tempfile::NamedTempFile;

// --- FLECS: The Brain (Single Source of Truth) ---

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

#[derive(Serialize)]
pub struct Manifest {
    pub generated_at: u64,
    pub total_groups: usize,
    pub total_files: usize,
}

// --- Apparatus: The Nervous System (Sensory Input/Discovery) ---

pub fn build_tree(root: &Path, output_dir: &Path, exclude: &GlobSet) -> Result<VaultTree> {
    // Explicit type annotations to resolve E0282
    let mut files_by_folder: HashMap<PathBuf, Vec<MdFile>> = HashMap::new();
    let mut subfolders: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();

    // Fix: Ensure root "" is present to trigger recursive traversal
    subfolders.insert(PathBuf::from(""), HashSet::new());

    let abs_root = root.canonicalize().context("Invalid vault root")?;
    let abs_output = output_dir.canonicalize().ok();

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

            // Build DAG hierarchy
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

// --- Mass: The Presentation Layer (Packing & Writing) ---

fn pack_node(
    path: &Path,
    tree: &VaultTree,
    max_bytes: u64,
    max_chapters: usize,
    sort_by: SortBy,
) -> (Vec<GroupPlan>, GroupPlan) {
    let mut finished = Vec::new();
    let mut bubbling = Vec::new();

    // 1. Recurse into subfolders
    if let Some(subs) = tree.subfolders.get(path) {
        let mut sorted_subs: Vec<_> = subs.iter().collect();
        sorted_subs.sort();
        for sub in sorted_subs {
            let (mut child_groups, child_rem) = pack_node(sub, tree, max_bytes, max_chapters, sort_by);
            finished.append(&mut child_groups);
            bubbling.extend(child_rem.files);
        }
    }

    // 2. Add local files at this node
    if let Some(locals) = tree.files_by_folder.get(path) {
        let mut sorted = locals.clone();
        match sort_by {
            SortBy::Name => sorted.sort_by(|a, b| a.rel.cmp(&b.rel)),
            SortBy::Mtime => sorted.sort_by(|a, b| a.mtime_ns.cmp(&b.mtime_ns)),
        }
        bubbling.extend(sorted);
    }

    // 3. Batching Logic (Resolved E0382 Move Error)
    let mut current = GroupPlan { files: Vec::new(), estimated_bytes: 64 }; // Initial YAML/TOC Overhead
    for f in bubbling {
        let path_len = f.rel.to_string_lossy().len() as u64;
        let overhead = 24 + (path_len * 2);
        let f_size = f.size;

        // Behemoth Check
        if f_size + overhead > max_bytes {
            if !current.files.is_empty() { finished.push(current); }
            finished.push(GroupPlan { estimated_bytes: f_size + overhead, files: vec![f] });
            current = GroupPlan { files: Vec::new(), estimated_bytes: 64 };
            continue;
        }

        let fits_size = current.estimated_bytes + f_size + overhead < max_bytes;
        let fits_chapters = max_chapters == 0 || current.files.len() < max_chapters;

        if fits_size && fits_chapters {
            current.estimated_bytes += f_size + overhead;
            current.files.push(f);
        } else {
            if !current.files.is_empty() { finished.push(current); }
            current = GroupPlan { files: vec![f], estimated_bytes: f_size + overhead + 64 };
        }
    }

    (finished, current)
}

pub fn run(cli: Cli) -> Result<()> {
    if cli.max_mb <= 0.0 { bail!("max_mb must be > 0"); }

    let out = cli.output_dir.clone().unwrap_or_else(|| cli.vault_root.join("_grouped"));
    fs::create_dir_all(&out)?;

    let mut gb = GlobSetBuilder::new();
    for p in cli.exclude { gb.add(Glob::new(&p)?); }
    let tree = build_tree(&cli.vault_root, &out, &gb.build()?)?;

    let max_b = (cli.max_mb * 1024.0 * 1024.0) as u64;
    let (mut groups, last) = pack_node(Path::new(""), &tree, max_b, cli.max_chapters, cli.sort_by);
    if !last.files.is_empty() { groups.push(last); }

    let total_files: usize = groups.iter().map(|g| g.files.len()).sum();
    let pb = ProgressBar::new(total_files as u64);
    pb.set_style(ProgressStyle::default_bar().template("[{bar:40}] {pos}/{len} {msg}")?);

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

    if cli.manifest {
        let m = Manifest {
            generated_at: std::time::SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
            total_groups: groups.len(),
            total_files,
        };
        fs::write(out.join("manifest.json"), serde_json::to_string_pretty(&m)?)?;
    }

    pb.finish_with_message("Done.");
    Ok(())
}

pub fn run_cli() -> Result<()> { run(Cli::parse()) }