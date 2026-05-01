//! obsidian_vault_grouper v1.4.0
//! The "Daniel Chay" Production Edition: Corrected Folder Affinity & Multi-OS Support.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufWriter, Write, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use ignore::WalkBuilder;
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Serialize, Deserialize};
use sha2::{Sha256, Digest};
use tempfile::NamedTempFile;

// --- Brain: Logic & State Constants ---
const YAML_BASE: u64 = 40;
const TOC_HEAD: u64 = 30;
const CHAPTER_BASE: u64 = 40;
const SAFETY_MARGIN: u64 = 1024; // 1KB buffer for file growth/encoding

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SortBy { Name, Mtime }

#[derive(Parser)]
#[command(version, about = "Group Obsidian vaults with folder affinity and integrity checks.")]
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
    pub stats_only: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MdFile {
    pub abs: PathBuf,
    pub rel_unix: String, // Normalized for Cross-Platform
    pub size: u64,
    pub mtime: u64,
}

pub struct BrainState {
    pub files_by_folder: HashMap<PathBuf, Vec<MdFile>>,
    pub subfolders: HashMap<PathBuf, HashSet<PathBuf>>,
}

#[derive(Serialize)]
pub struct GroupPlan {
    pub files: Vec<MdFile>,
    pub est_size: u64,
}

#[derive(Serialize)]
pub struct Manifest {
    pub version: String,
    pub packs: HashMap<String, PackInfo>,
}

#[derive(Serialize)]
pub struct PackInfo {
    pub checksum: String,
    pub files: Vec<String>,
}

// --- Apparatus: Sensory Discovery ---

fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

pub fn scan_vault(root: &Path, exclude_patterns: &[String]) -> Result<BrainState> {
    let mut files_by_folder = HashMap::new();
    let mut subfolders = HashMap::new();

    let mut walker = WalkBuilder::new(root);
    walker.standard_filters(true).follow_links(false);
    for pat in exclude_patterns { walker.add_custom_ignore_filename(pat); }

    for entry in walker.build() {
        let e = entry?;
        let path = e.path();
        if path.extension().map_or(false, |ext| ext == "md") && e.file_type().map_or(false, |ft| ft.is_file()) {
            let rel = path.strip_prefix(root)?.to_path_buf();
            let parent = rel.parent().unwrap_or_else(|| Path::new("")).to_path_buf();

            // Build hierarchy
            let mut curr = parent.as_path();
            while let Some(p) = curr.parent() {
                subfolders.entry(p.to_path_buf()).or_insert_with(HashSet::new).insert(curr.to_path_buf());
                curr = p;
            }

            let meta = e.metadata()?;
            files_by_folder.entry(parent).or_insert_with(Vec::new).push(MdFile {
                abs: path.to_path_buf(),
                rel_unix: normalize_path(&rel),
                size: meta.len(),
                mtime: meta.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_secs(),
            });
        }
    }
    Ok(BrainState { files_by_folder, subfolders })
}

// --- Brain: Recursive Bubbling Engine ---

fn pack_node(
    path: &Path,
    state: &BrainState,
    max_bytes: u64,
    max_ch: usize,
    sort_by: SortBy,
    depth: u32,
) -> Result<(Vec<GroupPlan>, Vec<MdFile>)> {
    if depth > 500 { bail!("Vault nesting exceeds safety limit (500). Possible recursion loop."); }

    let mut finished = Vec::new();
    let mut bubbling = Vec::new();

    // 1. Process children first (Bottom-Up)
    if let Some(subs) = state.subfolders.get(path) {
        let mut sorted_subs: Vec<_> = subs.iter().collect();
        sorted_subs.sort();
        for sub in sorted_subs {
            let (mut child_groups, child_rem) = pack_node(sub, state, max_bytes, max_ch, sort_by, depth + 1)?;
            finished.append(&mut child_groups);
            bubbling.extend(child_rem);
        }
    }

    // 2. Add local files
    if let Some(locals) = state.files_by_folder.get(path) {
        let mut sorted = locals.clone();
        match sort_by {
            SortBy::Name => sorted.sort_by(|a, b| a.rel_unix.cmp(&b.rel_unix)),
            SortBy::Mtime => sorted.sort_by(|a, b| a.mtime.cmp(&b.mtime)),
        }
        bubbling.extend(sorted);
    }

    // 3. Group the bubbling pool
    let mut groups = Vec::new();
    let mut current = GroupPlan { files: Vec::new(), est_size: YAML_BASE + TOC_HEAD };

    for f in bubbling {
        let overhead = CHAPTER_BASE + (f.rel_unix.len() as u64 * 2) + 10; // TOC line + Header

        // Behemoth isolation
        if f.size + overhead + YAML_BASE + TOC_HEAD > max_bytes {
            finished.push(GroupPlan { files: vec![f.clone()], est_size: f.size + overhead + YAML_BASE + TOC_HEAD });
            continue;
        }

        let fits = (current.est_size + f.size + overhead < max_bytes) &&
            (max_ch == 0 || current.files.len() < max_ch);

        if fits {
            current.est_size += f.size + overhead;
            current.files.push(f);
        } else {
            if !current.files.is_empty() { groups.push(current); }
            current = GroupPlan { files: vec![f.clone()], est_size: YAML_BASE + TOC_HEAD + f.size + overhead };
        }
    }

    Ok((finished, current.files))
}

// --- Mass: Presentation & Writing ---

pub fn run(cli: Cli) -> Result<()> {
    let out = cli.output_dir.clone().unwrap_or_else(|| cli.vault_root.join("_grouped"));
    if !cli.stats_only { fs::create_dir_all(&out)?; }

    let state = scan_vault(&cli.vault_root, &cli.exclude)?;
    let max_b = (cli.max_mb * 1024.0 * 1024.0) as u64;

    let (mut groups, last) = pack_node(Path::new(""), &state, max_b, cli.max_chapters, cli.sort_by, 0)?;
    if !last.is_empty() { groups.push(GroupPlan { est_size: 0, files: last }); }

    if cli.stats_only {
        println!("Analysis Complete: {} packs planned for {} files.", groups.len(), state.files_by_folder.values().map(|v| v.len()).sum::<usize>());
        return Ok(());
    }

    let pb = ProgressBar::new(groups.len() as u64);
    pb.set_style(ProgressStyle::default_bar().template("[{bar:40}] {pos}/{len} - {msg}")?);

    let mut manifest_data = Manifest { version: "1.4.0".into(), packs: HashMap::new() };

    for (i, g) in groups.iter().enumerate() {
        let pack_name = format!("pack_{:04}.md", i + 1);
        let dest = out.join(&pack_name);

        if cli.resume && !cli.force && dest.exists() { pb.inc(1); continue; }
        if cli.force && dest.exists() { fs::remove_file(&dest)?; }

        let tmp = NamedTempFile::new_in(&out)?;
        let mut hasher = Sha256::new();
        {
            let mut writer = BufWriter::new(tmp.as_file());

            // Write Frontmatter & TOC
            let head = format!("---\nvault_group: {}\n---\n\n# Table of Contents\n", i + 1);
            writer.write_all(head.as_bytes())?;
            hasher.update(head.as_bytes());

            for (idx, f) in g.files.iter().enumerate() {
                let line = format!("{}. {}\n", idx + 1, f.rel_unix);
                writer.write_all(line.as_bytes())?;
                hasher.update(line.as_bytes());
            }

            // Write Content
            for (idx, f) in g.files.iter().enumerate() {
                let mut src = File::open(&f.abs)?;
                let header = format!("\n---\n# Chapter {}: {}\n\n", idx + 1, f.rel_unix);
                writer.write_all(header.as_bytes())?;
                hasher.update(header.as_bytes());

                let mut buffer = [0u8; 8192];
                loop {
                    let n = src.read(&mut buffer)?;
                    if n == 0 { break; }
                    writer.write_all(&buffer[..n])?;
                    hasher.update(&buffer[..n]);
                }
            }
        }

        let checksum = format!("{:x}", hasher.finalize());
        manifest_data.packs.insert(pack_name, PackInfo {
            checksum,
            files: g.files.iter().map(|f| f.rel_unix.clone()).collect(),
        });

        tmp.persist(&dest).context("Atomic write failed")?;
        pb.inc(1);
    }

    if cli.manifest {
        let m_path = out.join("manifest.json");
        fs::write(m_path, serde_json::to_string_pretty(&manifest_data)?)?;
    }

    pb.finish_with_message("Deployment Complete.");
    Ok(())
}

pub fn run_cli() -> Result<()> { run(Cli::parse()) }