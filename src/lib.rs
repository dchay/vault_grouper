//! obsidian_vault_grouper v1.6.0
//! Deterministic Edition: Fixed Order, Accurate Math, and Dry Run.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufWriter, Write, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use walkdir::WalkDir;
use globset::{Glob, GlobSet, GlobSetBuilder};
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Serialize, Deserialize};
use sha2::{Sha256, Digest};
use tempfile::NamedTempFile;

// --- Brain: Constants & Calculus ---
const YAML_BASE: u64 = 64;
const TOC_HEAD: u64 = 40;
const CHAPTER_OVERHEAD_BASE: u64 = 25; // Optimized overhead per file
const SAFETY_MARGIN: u64 = 10 * 1024; // 10KB buffer

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SortBy { Name, Mtime }

#[derive(Parser)]
#[command(name = "grouper", version = "1.6.0", about = "Deterministic Obsidian vault grouping.")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Pack the vault into grouped Markdown files
    Pack {
        vault_root: PathBuf,
        output_dir: Option<PathBuf>,
        #[arg(long, default_value_t = 20.0)]
        max_mb: f64,
        #[arg(long, default_value_t = 0)]
        max_chapters: usize,
        #[arg(long, value_enum, default_value_t = SortBy::Name)]
        sort_by: SortBy,
        #[arg(long)]
        exclude: Vec<String>,
        #[arg(long)]
        resume: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        manifest: bool,
        #[arg(long)]
        dry_run: bool,
    },
    /// Verify the integrity of existing packs
    Verify {
        #[arg(default_value = "_grouped/manifest.json")]
        manifest_path: PathBuf,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MdFile {
    pub abs: PathBuf,
    pub rel_unix: String,
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

#[derive(Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub packs: HashMap<String, PackInfo>,
}

#[derive(Serialize, Deserialize)]
pub struct PackInfo {
    pub checksum: String,
    pub files: Vec<String>,
}

// --- Apparatus: Sensory Discovery (Deterministic) ---

fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns { builder.add(Glob::new(pat)?); }
    Ok(builder.build()?)
}

pub fn scan_vault(root: &Path, out: &Path, excludes: &GlobSet) -> Result<BrainState> {
    let mut files_by_folder = HashMap::new();
    let mut subfolders = HashMap::new();
    let root_canonical = root.canonicalize()?;

    // Explicitly initialize the root key
    subfolders.insert(PathBuf::from(""), HashSet::new());

    // walkdir is deterministic if we sort after collection or use it sequentially
    for entry in WalkDir::new(root).follow_links(false).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.starts_with(out) { continue; }

        if path.extension().map_or(false, |ext| ext == "md") && entry.file_type().is_file() {
            let rel = path.strip_prefix(&root_canonical)?;
            let rel_unix = rel.to_string_lossy().replace('\\', "/");

            if excludes.is_match(&rel_unix) { continue; }

            let parent = rel.parent().unwrap_or_else(|| Path::new("")).to_path_buf();

            // Build hierarchy upward
            let mut curr = parent.as_path();
            while let Some(p) = curr.parent() {
                subfolders.entry(p.to_path_buf()).or_insert_with(HashSet::new).insert(curr.to_path_buf());
                curr = p;
            }

            let meta = entry.metadata()?;
            files_by_folder.entry(parent).or_insert_with(Vec::new).push(MdFile {
                abs: path.to_path_buf(),
                rel_unix,
                size: meta.len(),
                mtime: meta.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_secs(),
            });
        }
    }
    Ok(BrainState { files_by_folder, subfolders })
}

// --- Brain: Recursive Packing Engine ---

fn pack_node(
    path: &Path,
    state: &BrainState,
    available_bytes: u64,
    max_ch: usize,
    sort_by: SortBy,
    depth: u32,
) -> Result<(Vec<GroupPlan>, Vec<MdFile>)> {
    if depth > 500 { bail!("Recursion safety limit reached."); }

    let mut all_completed_packs = Vec::new();
    let mut current_pool = Vec::new();

    // 1. Process Child Nodes Deterministically
    if let Some(subs) = state.subfolders.get(path) {
        let mut sorted_subs: Vec<_> = subs.iter().collect();
        sorted_subs.sort(); // ENSURE ORDER
        for sub in sorted_subs {
            let (mut child_packs, child_rem) = pack_node(sub, state, available_bytes, max_ch, sort_by, depth + 1)?;
            all_completed_packs.append(&mut child_packs);
            current_pool.extend(child_rem);
        }
    }

    // 2. Add Local Files
    if let Some(locals) = state.files_by_folder.get(path) {
        let mut sorted = locals.clone();
        match sort_by {
            SortBy::Name => sorted.sort_by(|a, b| a.rel_unix.cmp(&b.rel_unix)),
            SortBy::Mtime => sorted.sort_by(|a, b| a.mtime.cmp(&b.mtime)),
        }
        current_pool.extend(sorted);
    }

    // 3. Sequential Grouping
    let mut final_remainder = Vec::new();
    let mut current_pack = GroupPlan { files: Vec::new(), est_size: YAML_BASE + TOC_HEAD };

    for f in current_pool {
        let file_overhead = CHAPTER_OVERHEAD_BASE + (f.rel_unix.len() as u64 * 2);

        // Behemoth Check (Calculated against absolute limit, not available_bytes)
        if f.size + file_overhead + YAML_BASE + TOC_HEAD > available_bytes + SAFETY_MARGIN {
            let size = f.size;
            all_completed_packs.push(GroupPlan {
                files: vec![f],
                est_size: size + file_overhead + YAML_BASE + TOC_HEAD
            });
            continue;
        }

        let fits = (current_pack.est_size + f.size + file_overhead < available_bytes) &&
            (max_ch == 0 || current_pack.files.len() < max_ch);

        if fits {
            current_pack.est_size += f.size + file_overhead;
            current_pack.files.push(f);
        } else {
            if !current_pack.files.is_empty() {
                all_completed_packs.push(current_pack);
            }
            let size = f.size;
            current_pack = GroupPlan {
                files: vec![f],
                est_size: YAML_BASE + TOC_HEAD + size + file_overhead
            };
        }
    }

    final_remainder.extend(current_pack.files);
    Ok((all_completed_packs, final_remainder))
}

// --- Mass: Presentation & Output ---

pub fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Pack { vault_root, output_dir, max_mb, max_chapters, sort_by, exclude, resume, force, manifest, dry_run } => {
            let out = output_dir.unwrap_or_else(|| vault_root.join("_grouped"));
            if !dry_run { fs::create_dir_all(&out)?; }

            let excludes_set = build_globset(&exclude)?;
            let state = scan_vault(&vault_root, &out, &excludes_set)?;

            // MATH: Estimation happens against capacity minus safety margin
            let capacity = (max_mb * 1024.0 * 1024.0) as u64;
            let available = if capacity > SAFETY_MARGIN { capacity - SAFETY_MARGIN } else { capacity };

            let (mut packs, last) = pack_node(Path::new(""), &state, available, max_chapters, sort_by, 0)?;
            if !last.is_empty() {
                packs.push(GroupPlan { files: last, est_size: 0 });
            }

            if dry_run {
                println!("Dry Run: Would create {} packs for {} total files.", packs.len(), packs.iter().map(|p| p.files.len()).sum::<usize>());
                return Ok(());
            }

            let pb = ProgressBar::new(packs.len() as u64);
            pb.set_style(ProgressStyle::default_bar().template("[{bar:40}] Pack {pos}/{len} - {msg}")?);

            let mut manifest_data = Manifest { version: "1.6.0".into(), packs: HashMap::new() };

            for (i, p) in packs.iter().enumerate() {
                let pack_name = format!("pack_{:04}.md", i + 1);
                let dest = out.join(&pack_name);

                if resume && !force && dest.exists() { pb.inc(1); continue; }

                let tmp = NamedTempFile::new_in(&out)?;
                let mut hasher = Sha256::new();
                let mut written: u64 = 0;

                {
                    let mut writer = BufWriter::new(tmp.as_file());
                    let head = format!("---\nvault_group: {}\n---\n\n# Table of Contents\n", i + 1);
                    writer.write_all(head.as_bytes())?;
                    hasher.update(head.as_bytes());
                    written += head.len() as u64;

                    for (idx, f) in p.files.iter().enumerate() {
                        let line = format!("{}. {}\n", idx + 1, f.rel_unix);
                        writer.write_all(line.as_bytes())?;
                        hasher.update(line.as_bytes());
                        written += line.len() as u64;
                    }

                    for (idx, f) in p.files.iter().enumerate() {
                        let header = format!("\n---\n# Chapter {}: {}\n\n", idx + 1, f.rel_unix);
                        writer.write_all(header.as_bytes())?;
                        hasher.update(header.as_bytes());
                        written += header.len() as u64;

                        let mut src = File::open(&f.abs)?;
                        let mut buf = [0u8; 8192];
                        while let Ok(n) = src.read(&mut buf) {
                            if n == 0 { break; }
                            if written + (n as u64) > capacity {
                                bail!("Pack {} overflowed capacity. Vault content changed during execution.", pack_name);
                            }
                            writer.write_all(&buf[..n])?;
                            hasher.update(&buf[..n]);
                            written += n as u64;
                        }
                    }
                }

                manifest_data.packs.insert(pack_name, PackInfo {
                    checksum: format!("{:x}", hasher.finalize()),
                    files: p.files.iter().map(|f| f.rel_unix.clone()).collect(),
                });

                tmp.persist(&dest)?;
                pb.inc(1);
            }

            if manifest {
                fs::write(out.join("manifest.json"), serde_json::to_string_pretty(&manifest_data)?)?;
            }
            pb.finish_with_message("Done.");
        }
        Commands::Verify { manifest_path } => {
            let m_content = fs::read_to_string(&manifest_path).context("Read manifest failed")?;
            let manifest: Manifest = serde_json::from_str(&m_content)?;
            let base_dir = manifest_path.parent().unwrap_or_else(|| Path::new("."));

            for (name, info) in manifest.packs {
                let p_path = base_dir.join(&name);
                if !p_path.exists() { println!("[MISSING] {}", name); continue; }

                let mut hasher = Sha256::new();
                let mut f = File::open(p_path)?;
                let mut buf = [0u8; 8192];
                while let Ok(n) = f.read(&mut buf) {
                    if n == 0 { break; }
                    hasher.update(&buf[..n]);
                }

                if format!("{:x}", hasher.finalize()) == info.checksum {
                    println!("[OK] {}", name);
                } else {
                    println!("[CORRUPT] {}", name);
                }
            }
        }
    }
    Ok(())
}