//! obsidian_vault_grouper v1.6.1
//! Final Production Edition: Deterministic, Parallel, and Memory-Efficient.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufWriter, Write, Read, BufReader},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use walkdir::WalkDir;
use globset::{Glob, GlobSet, GlobSetBuilder};
use indicatif::{ProgressBar, ProgressStyle, MultiProgress};
use serde::{Serialize, Deserialize};
use sha2::{Sha256, Digest};
use tempfile::NamedTempFile;
use rayon::prelude::*;

// --- Brain: Constants & Calculus ---
const YAML_BASE: u64 = 64;
const TOC_HEAD: u64 = 40;
const CHAPTER_OVERHEAD_BASE: u64 = 25;
const SAFETY_MARGIN: u64 = 15 * 1024; // Increased to 15KB for safety
const BUFFER_SIZE: usize = 16384;    // 16KB IO buffer

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SortBy { Name, Mtime }

#[derive(Parser)]
#[command(name = "grouper", version = "1.6.1", about = "Production-grade Obsidian vault grouping.")]
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
    /// Verify the integrity of existing packs against manifest.json
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
    pub files_by_folder: HashMap<PathBuf, Vec<Arc<MdFile>>>,
    pub subfolders: HashMap<PathBuf, HashSet<PathBuf>>,
}

pub struct GroupPlan {
    pub files: Vec<Arc<MdFile>>,
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

// --- Apparatus: Deterministic Discovery ---

fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns { builder.add(Glob::new(pat)?); }
    Ok(builder.build()?)
}

pub fn scan_vault(root: &Path, out: &Path, excludes: &GlobSet) -> Result<BrainState> {
    let mut files_by_folder = HashMap::new();
    let mut subfolders = HashMap::new();
    let root_canonical = root.canonicalize().context("Failed to canonicalize vault root")?;
    let out_canonical = if out.exists() {
        out.canonicalize().ok()
    } else {
        None
    };

    subfolders.insert(PathBuf::from(""), HashSet::new());

    // 1. Deterministic Discovery: Collect and Sort by path
    let mut entries: Vec<_> = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .collect();

    entries.sort_by(|a, b| a.path().cmp(b.path()));

    // 2. Hierarchy Building
    for entry in entries {
        let path = entry.path();

        // Skip output directory explicitly
        if let Some(ref o) = out_canonical {
            if path.starts_with(o) { continue; }
        }

        if path.extension().map_or(false, |ext| ext == "md") && entry.file_type().is_file() {
            let rel = path.strip_prefix(&root_canonical)?;
            let rel_unix = rel.to_string_lossy().replace('\\', "/");

            if excludes.is_match(&rel_unix) { continue; }

            let parent = rel.parent().unwrap_or_else(|| Path::new("")).to_path_buf();

            let mut curr = parent.as_path();
            while let Some(p) = curr.parent() {
                subfolders.entry(p.to_path_buf()).or_insert_with(HashSet::new).insert(curr.to_path_buf());
                curr = p;
            }

            let meta = entry.metadata()?;
            files_by_folder.entry(parent).or_insert_with(Vec::new).push(Arc::new(MdFile {
                abs: path.to_path_buf(),
                rel_unix,
                size: meta.len(),
                mtime: meta.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_secs(),
            }));
        }
    }
    Ok(BrainState { files_by_folder, subfolders })
}

// --- Brain: Optimized Packing Engine ---

fn pack_node(
    path: &Path,
    state: &BrainState,
    available_bytes: u64,
    max_ch: usize,
    sort_by: SortBy,
    depth: u32,
) -> Result<(Vec<GroupPlan>, Vec<Arc<MdFile>>)> {
    if depth > 500 { bail!("Recursion depth guard triggered."); }

    let mut completed_packs = Vec::new();
    let mut current_pool = Vec::new();

    // 1. Recurse into subfolders
    if let Some(subs) = state.subfolders.get(path) {
        let mut sorted_subs: Vec<_> = subs.iter().collect();
        sorted_subs.sort();
        for sub in sorted_subs {
            let (mut child_packs, child_rem) = pack_node(sub, state, available_bytes, max_ch, sort_by, depth + 1)?;
            completed_packs.append(&mut child_packs);
            current_pool.extend(child_rem);
        }
    }

    // 2. Collect local files
    if let Some(locals) = state.files_by_folder.get(path) {
        let mut sorted = locals.clone();
        match sort_by {
            SortBy::Name => sorted.sort_by(|a, b| a.rel_unix.cmp(&b.rel_unix)),
            SortBy::Mtime => sorted.sort_by(|a, b| a.mtime.cmp(&b.mtime)),
        }
        current_pool.extend(sorted);
    }

    // 3. Bin Packing logic
    let mut remainder = Vec::new();
    let mut current_pack = GroupPlan { files: Vec::new(), est_size: YAML_BASE + TOC_HEAD };

    for f in current_pool {
        let overhead = CHAPTER_OVERHEAD_BASE + (f.rel_unix.len() as u64 * 2);

        // Behemoth Check (against absolute capacity)
        if f.size + overhead + YAML_BASE + TOC_HEAD > available_bytes + SAFETY_MARGIN {
            completed_packs.push(GroupPlan { files: vec![f.clone()], est_size: f.size + overhead + YAML_BASE + TOC_HEAD });
            continue;
        }

        let fits = (current_pack.est_size + f.size + overhead < available_bytes) &&
            (max_ch == 0 || current_pack.files.len() < max_ch);

        if fits {
            current_pack.est_size += f.size + overhead;
            current_pack.files.push(f);
        } else {
            if !current_pack.files.is_empty() {
                completed_packs.push(current_pack);
            }
            let size = f.size;
            current_pack = GroupPlan { files: vec![f], est_size: YAML_BASE + TOC_HEAD + overhead + size };
        }
    }

    remainder.extend(current_pack.files);
    Ok((completed_packs, remainder))
}

// --- Mass: Presentation & Verification ---

pub fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Pack { vault_root, output_dir, max_mb, max_chapters, sort_by, exclude, resume, force, manifest, dry_run } => {
            let out = output_dir.unwrap_or_else(|| vault_root.join("_grouped"));
            if !dry_run { fs::create_dir_all(&out)?; }

            let excludes_set = build_globset(&exclude)?;
            let state = scan_vault(&vault_root, &out, &excludes_set)?;

            let capacity = (max_mb * 1024.0 * 1024.0) as u64;
            let available = if capacity > SAFETY_MARGIN { capacity - SAFETY_MARGIN } else { capacity };

            let (mut packs, last) = pack_node(Path::new(""), &state, available, max_chapters, sort_by, 0)?;
            if !last.is_empty() { packs.push(GroupPlan { files: last, est_size: 0 }); }

            if packs.is_empty() {
                println!("No markdown files found to pack.");
                return Ok(());
            }

            if dry_run {
                println!("Dry Run: Total Packs: {}, Total Files: {}", packs.len(), packs.iter().map(|p| p.files.len()).sum::<usize>());
                return Ok(());
            }

            // Multi-Progress Setup
            let mp = MultiProgress::new();
            let pack_pb = mp.add(ProgressBar::new(packs.len() as u64));
            pack_pb.set_style(ProgressStyle::default_bar().template("{spinner:.green} [Pack {pos}/{len}] {bar:40.cyan/blue} {msg}")?);

            let mut manifest_data = Manifest { version: "1.6.1".into(), packs: HashMap::new() };

            for (i, p) in packs.iter().enumerate() {
                let pack_name = format!("pack_{:04}.md", i + 1);
                let dest = out.join(&pack_name);

                if dest.exists() {
                    if force { fs::remove_file(&dest)?; }
                    else if resume { pack_pb.inc(1); continue; }
                    else { bail!("File {} exists. Use --force or --resume.", pack_name); }
                }

                let tmp = NamedTempFile::new_in(&out)?;
                let mut hasher = Sha256::new();
                let mut written_bytes: u64 = 0;

                {
                    let mut writer = BufWriter::new(tmp.as_file());
                    let header = format!("---\nvault_group: {}\n---\n\n# Table of Contents\n", i + 1);
                    writer.write_all(header.as_bytes())?;
                    hasher.update(header.as_bytes());
                    written_bytes += header.len() as u64;

                    for (idx, f) in p.files.iter().enumerate() {
                        let line = format!("{}. {}\n", idx + 1, f.rel_unix);
                        writer.write_all(line.as_bytes())?;
                        hasher.update(line.as_bytes());
                        written_bytes += line.len() as u64;
                    }

                    for (idx, f) in p.files.iter().enumerate() {
                        let chap = format!("\n---\n# Chapter {}: {}\n\n", idx + 1, f.rel_unix);
                        writer.write_all(chap.as_bytes())?;
                        hasher.update(chap.as_bytes());
                        written_bytes += chap.len() as u64;

                        let mut src = BufReader::new(File::open(&f.abs)?);
                        let mut buf = [0u8; BUFFER_SIZE];
                        while let Ok(n) = src.read(&mut buf) {
                            if n == 0 { break; }
                            if written_bytes + (n as u64) > capacity + SAFETY_MARGIN {
                                bail!("Overflow in pack {}. Source data modified during run.", pack_name);
                            }
                            writer.write_all(&buf[..n])?;
                            hasher.update(&buf[..n]);
                            written_bytes += n as u64;
                        }
                    }
                }

                let checksum = format!("{:x}", hasher.finalize());
                manifest_data.packs.insert(pack_name, PackInfo {
                    checksum,
                    files: p.files.iter().map(|f| f.rel_unix.clone()).collect(),
                });

                tmp.persist(&dest).context("Failed to save final pack file")?;
                pack_pb.inc(1);
            }

            if manifest {
                fs::write(out.join("manifest.json"), serde_json::to_string_pretty(&manifest_data)?)?;
            }
            pack_pb.finish_with_message("Packing Complete.");
        }
        Commands::Verify { manifest_path } => {
            let content = fs::read_to_string(&manifest_path).context("Could not read manifest")?;
            let manifest: Manifest = serde_json::from_str(&content)?;
            let base = manifest_path.parent().unwrap_or_else(|| Path::new("."));

            // Parallel verification with Rayon
            manifest.packs.par_iter().for_each(|(name, info)| {
                let p_path = base.join(name);
                if !p_path.exists() {
                    println!("[MISSING] {}", name);
                    return;
                }

                let mut hasher = Sha256::new();
                if let Ok(mut f) = File::open(&p_path) {
                    let mut buf = [0u8; BUFFER_SIZE];
                    while let Ok(n) = f.read(&mut buf) {
                        if n == 0 { break; }
                        hasher.update(&buf[..n]);
                    }
                    if format!("{:x}", hasher.finalize()) == info.checksum {
                        println!("[OK] {} ({} files)", name, info.files.len());
                    } else {
                        println!("[CORRUPT] {}", name);
                    }
                }
            });
        }
    }
    Ok(())
}
