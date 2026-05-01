//! obsidian_vault_grouper v1.3.0
//! Hardened Production Release

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufWriter, Write, Read},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use globset::{Glob, GlobSet, GlobSetBuilder};
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Serialize, Deserialize};
use tempfile::NamedTempFile;

// --- Constants & Magic Numbers ---
const YAML_FRONTMATTER_SIZE: u64 = 30;
const TOC_HEADER_SIZE: u64 = 25;
const CHAPTER_HEADER_BASE: u64 = 25; // "\n---\n# Chapter X: "
const NEWLINE_SIZE: u64 = 1;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SortBy { Name, Mtime }

#[derive(Parser)]
#[command(version, about = "Group Obsidian vaults into AI-friendly packs.")]
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
    pub force: bool,
    #[arg(long)]
    pub manifest: bool,
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MdFile {
    pub abs: PathBuf,
    pub rel: PathBuf,
    pub size: u64,
    pub mtime_ns: u128,
}

pub struct VaultTree {
    pub files_by_folder: HashMap<PathBuf, Vec<MdFile>>,
    pub subfolders: HashMap<PathBuf, Vec<PathBuf>>,
}

#[derive(Serialize)]
pub struct GroupPlan {
    pub files: Vec<MdFile>,
    pub estimated_bytes: u64,
}

#[derive(Serialize)]
pub struct Manifest {
    pub generated_at: u64,
    pub total_files: usize,
    pub group_map: HashMap<String, Vec<PathBuf>>,
}

// --- Apparatus: Sensory Discovery ---

pub fn build_tree(root: &Path, output_dir: &Path, exclude: &GlobSet) -> Result<VaultTree> {
    let mut files_by_folder: HashMap<PathBuf, Vec<MdFile>> = HashMap::new();
    let mut subfolders_map: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();

    let abs_root = root.canonicalize().context("Invalid vault root")?;
    let abs_output = output_dir.canonicalize().ok();

    for entry in walkdir::WalkDir::new(&abs_root).min_depth(1).follow_links(false) {
        let e = entry.map_err(|err| { eprintln!("Warning: Skipping entry: {}", err); err }).ok();
        if e.is_none() { continue; }
        let e = e.unwrap();
        let path = e.path();

        if let Some(ref out) = abs_output { if path.starts_with(out) { continue; } }
        let rel = path.strip_prefix(&abs_root).unwrap_or(path).to_path_buf();
        if exclude.is_match(&rel) { continue; }

        if path.extension().map_or(false, |ext| ext == "md") && e.file_type().is_file() {
            let parent = rel.parent().unwrap_or_else(|| Path::new("")).to_path_buf();

            // Iteratively build the subfolder hierarchy
            let mut curr = parent.as_path();
            while let Some(p) = curr.parent() {
                subfolders_map.entry(p.to_path_buf()).or_default().insert(curr.to_path_buf());
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

    let subfolders = subfolders_map.into_iter()
        .map(|(k, v)| {
            let mut folders: Vec<_> = v.into_iter().collect();
            folders.sort();
            (k, folders)
        }).collect();

    Ok(VaultTree { files_by_folder, subfolders })
}

// --- Mass: Iterative Grouping Engine ---

fn pack_vault_iterative(tree: &VaultTree, max_bytes: u64, max_chapters: usize, sort_by: SortBy) -> Vec<GroupPlan> {
    let mut finished_groups = Vec::new();
    let mut bubbling_files = Vec::new();
    let mut stack = vec![(PathBuf::from(""), false)]; // (Path, VisitedChildren)

    // Depth-First Post-Order Traversal (Iterative)
    while let Some((path, children_done)) = stack.pop() {
        if !children_done {
            stack.push((path.clone(), true));
            if let Some(subs) = tree.subfolders.get(&path) {
                for sub in subs.iter().rev() { stack.push((sub.clone(), false)); }
            }
        } else {
            let mut local_pool = Vec::new();
            if let Some(locals) = tree.files_by_folder.get(&path) {
                let mut sorted = locals.clone();
                match sort_by {
                    SortBy::Name => sorted.sort_by(|a, b| a.rel.cmp(&b.rel)),
                    SortBy::Mtime => sorted.sort_by(|a, b| a.mtime_ns.cmp(&b.mtime_ns)),
                }
                local_pool.extend(sorted);
            }
            bubbling_files.extend(local_pool);
        }
    }

    // Linear Grouping Logic
    let mut current = GroupPlan { files: Vec::new(), estimated_bytes: YAML_FRONTMATTER_SIZE + TOC_HEADER_SIZE };
    for f in bubbling_files {
        let path_len = f.rel.to_string_lossy().len() as u64;
        let toc_line_overhead = 5 + path_len + NEWLINE_SIZE;
        let chapter_overhead = CHAPTER_HEADER_BASE + path_len + (NEWLINE_SIZE * 2);
        let total_f_overhead = toc_line_overhead + chapter_overhead;

        if f.size + total_f_overhead > max_bytes {
            if !current.files.is_empty() { finished_groups.push(current); }
            finished_groups.push(GroupPlan {
                estimated_bytes: f.size + total_f_overhead + YAML_FRONTMATTER_SIZE + TOC_HEADER_SIZE,
                files: vec![f]
            });
            current = GroupPlan { files: Vec::new(), estimated_bytes: YAML_FRONTMATTER_SIZE + TOC_HEADER_SIZE };
            continue;
        }

        let fits = (current.estimated_bytes + f.size + total_f_overhead < max_bytes) &&
            (max_chapters == 0 || current.files.len() < max_chapters);

        if fits {
            current.estimated_bytes += f.size + total_f_overhead;
            current.files.push(f);
        } else {
            finished_groups.push(current);
            current = GroupPlan {
                files: vec![f.clone()],
                estimated_bytes: YAML_FRONTMATTER_SIZE + TOC_HEADER_SIZE + f.size + total_f_overhead
            };
        }
    }
    if !current.files.is_empty() { finished_groups.push(current); }
    finished_groups
}

pub fn run(cli: Cli) -> Result<()> {
    let out = cli.output_dir.clone().unwrap_or_else(|| cli.vault_root.join("_grouped"));
    if !cli.dry_run { fs::create_dir_all(&out)?; }

    let mut gb = GlobSetBuilder::new();
    for p in cli.exclude { gb.add(Glob::new(&p)?); }
    let tree = build_tree(&cli.vault_root, &out, &gb.build()?)?;

    let max_b = (cli.max_mb * 1024.0 * 1024.0) as u64;
    let groups = pack_vault_iterative(&tree, max_b, cli.max_chapters, cli.sort_by);

    if groups.is_empty() { println!("Vault is empty or all files excluded."); return Ok(()); }
    if cli.dry_run { println!("Dry Run: {} groups planned.", groups.len()); return Ok(()); }

    let pb = ProgressBar::new(groups.len() as u64);
    pb.set_style(ProgressStyle::default_bar().template("[{bar:40}] {pos}/{len} {msg}")?);

    let mut group_map = HashMap::new();
    for (i, g) in groups.iter().enumerate() {
        let name = format!("pack_{:04}.md", i + 1);
        let dest = out.join(&name);
        group_map.insert(name.clone(), g.files.iter().map(|f| f.rel.clone()).collect());

        if cli.resume && !cli.force && dest.exists() { pb.inc(1); continue; }

        let tmp = NamedTempFile::new_in(&out)?;
        {
            let mut w = BufWriter::new(tmp.as_file());
            writeln!(w, "---\nvault_group: {}\n---", i + 1)?;
            writeln!(w, "\n# Table of Contents")?;
            for (idx, f) in g.files.iter().enumerate() { writeln!(w, "{}. {}", idx + 1, f.rel.display())?; }

            for (idx, f) in g.files.iter().enumerate() {
                let mut src = File::open(&f.abs)?;
                let current_meta = src.metadata()?;
                if current_meta.len() > f.size * 2 {
                    bail!("File {} grew significantly during processing. Aborting.", f.rel.display());
                }
                writeln!(w, "\n---\n# Chapter {}: {}\n", idx + 1, f.rel.display())?;
                std::io::copy(&mut src, &mut w)?;
            }
        }
        tmp.persist(&dest).context("Failed to persist pack file")?;
        pb.inc(1);
    }

    if cli.manifest {
        let m = Manifest {
            generated_at: std::time::SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
            total_files: groups.iter().map(|g| g.files.len()).sum(),
            group_map,
        };
        fs::write(out.join("manifest.json"), serde_json::to_string_pretty(&m)?)?;
    }

    pb.finish_with_message("Done.");
    Ok(())
}

pub fn run_cli() -> Result<()> { run(Cli::parse()) }