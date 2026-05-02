//! # obsidian_vault_grouper v1.7.0
//! **The Aegis Edition: Fully Indexed.**
//! 
//! Features: Direct file-to-pack mapping, global sorted ledger, 
//! and production-hardened Windows 11 I/O.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufWriter, Write, BufReader, Read},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use dunce;
use globset::{Glob, GlobSet, GlobSetBuilder};
use indicatif::{ProgressBar, ProgressStyle};
use jwalk::WalkDir;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use uuid::Uuid;

// --- Brain: Constants & Data Structures ---
const YAML_BASE: u64 = 64;
const TOC_HEAD: u64 = 40;
const PER_FILE_OVERHEAD: u64 = 25;
const SAFETY_MARGIN: u64 = 16 * 1024;
const MAX_RECURSION_DEPTH: u32 = 500;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SortBy { 
    Name, 
    /// Chronological: Oldest to Newest.
    Mtime, 
    /// Reverse-Chronological: Newest to Oldest (Default).
    Recent 
}

#[derive(Parser)]
#[command(name = "grouper", version = "1.7.0")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    Pack {
        vault_root: PathBuf,
        output_dir: Option<PathBuf>,
        #[arg(long, default_value_t = 20.0)]
        max_mb: f64,
        #[arg(long, default_value_t = 0)]
        max_chapters: usize,
        #[arg(long, value_enum, default_value_t = SortBy::Recent)]
        sort_by: SortBy,
        #[arg(long)]
        exclude: Vec<String>,
        #[arg(long)]
        resume: bool,
        #[arg(long)]
        force: bool,
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        manifest: bool,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        quiet: bool,
        #[arg(long)]
        no_progress: bool,
    },
    Verify {
        #[arg(default_value = "_grouped/vault_manifest.json")]
        manifest_path: PathBuf,
        #[arg(long)]
        quiet: bool,
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
    pub folder_timestamps: HashMap<PathBuf, u64>,
}

#[derive(Debug)]
pub struct GroupPlan {
    pub files: Vec<Arc<MdFile>>,
    pub est_size: u64,
}

#[derive(Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub sort_order: String,
    /// Detailed pack metadata.
    pub packs: HashMap<String, PackInfo>,
    /// Global registry: Maps "vault_path/file.md" -> "pack_0001.md"
    pub file_to_pack_map: HashMap<String, String>,
    /// Sorted list of all files across all packs.
    pub global_file_order: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct PackInfo {
    pub checksum: String,
    pub file_count: usize,
}

// --- Apparatus: Optimized Discovery ---

fn is_reserved_name(name: &str) -> bool {
    let n = name.to_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL", "CLOCK$"];
    if reserved.contains(&n.as_str()) { return true; }
    if n.len() == 4 && (n.starts_with("COM") || n.starts_with("LPT")) {
        if let Some(c) = n.chars().nth(3) {
            if c.is_ascii_digit() { return true; }
        }
    }
    false
}

fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        builder.add(Glob::new(&pat.replace('\\', "/"))?);
    }
    Ok(builder.build()?)
}

pub fn scan_vault(root: &Path, out: &Path, excludes: &GlobSet, sort_by: SortBy) -> Result<BrainState> {
    let mut files_by_folder = HashMap::new();
    let mut subfolders = HashMap::new();
    let mut folder_timestamps: HashMap<PathBuf, u64> = HashMap::new();
    let root_canonical = dunce::canonicalize(root).context("Failed to resolve root")?;
    let out_canonical = dunce::canonicalize(out).ok();

    subfolders.insert(PathBuf::from(""), HashSet::new());

    let walker = WalkDir::new(root).follow_links(false).sort(true);

    for entry in walker.into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if is_reserved_name(path.file_stem().and_then(|s| s.to_str()).unwrap_or("")) { continue; }
        if let Some(ref o) = out_canonical { if path.starts_with(o) { continue; } }

        if path.extension().map_or(false, |ext| ext == "md") && entry.file_type.is_file() {
            let rel = path.strip_prefix(&root_canonical).unwrap_or(&path);
            let rel_unix = rel.to_string_lossy().replace('\\', "/");
            if excludes.is_match(&rel_unix) { continue; }

            let parent = rel.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
            let meta = entry.metadata().context("Metadata error")?;
            let mtime = meta.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_secs();

            let mut curr = parent.as_path();
            loop {
                let p_buf = curr.to_path_buf();
                let timestamp = folder_timestamps.entry(p_buf.clone()).or_insert(mtime);
                match sort_by {
                    SortBy::Recent => if mtime > *timestamp { *timestamp = mtime; },
                    SortBy::Mtime => if mtime < *timestamp { *timestamp = mtime; },
                    SortBy::Name => {} 
                }
                if let Some(up) = curr.parent() {
                    subfolders.entry(up.to_path_buf()).or_insert_with(HashSet::new).insert(p_buf);
                    curr = up;
                } else { break; }
            }

            files_by_folder.entry(parent).or_insert_with(Vec::new).push(Arc::new(MdFile {
                abs: path.to_path_buf(),
                rel_unix,
                size: meta.len(),
                mtime,
            }));
        }
    }
    Ok(BrainState { files_by_folder, subfolders, folder_timestamps })
}

// --- Brain: Recursive Planning ---

fn pack_node(
    path: &Path,
    state: &BrainState,
    available_bytes: u64,
    max_ch: usize,
    sort_by: SortBy,
    depth: u32,
) -> Result<(Vec<GroupPlan>, Vec<Arc<MdFile>>)> {
    if depth > MAX_RECURSION_DEPTH { bail!("Recursion safety limit exceeded."); }

    let mut completed_packs = Vec::new();
    let mut current_pool = Vec::new();

    if let Some(subs) = state.subfolders.get(path) {
        let mut sorted_subs: Vec<_> = subs.iter().collect();
        match sort_by {
            SortBy::Name => sorted_subs.sort(),
            SortBy::Mtime => sorted_subs.sort_by_key(|p| state.folder_timestamps.get(*p).unwrap_or(&u64::MAX)),
            SortBy::Recent => sorted_subs.sort_by_key(|p| std::cmp::Reverse(state.folder_timestamps.get(*p).unwrap_or(&0))),
        }
        for sub in sorted_subs {
            let (mut child_packs, child_rem) = pack_node(sub, state, available_bytes, max_ch, sort_by, depth + 1)?;
            completed_packs.append(&mut child_packs);
            current_pool.extend(child_rem);
        }
    }

    if let Some(locals) = state.files_by_folder.get(path) {
        let mut sorted = locals.clone();
        match sort_by {
            SortBy::Name => sorted.sort_by(|a, b| a.rel_unix.cmp(&b.rel_unix)),
            SortBy::Mtime => sorted.sort_by(|a, b| a.mtime.cmp(&b.mtime)), 
            SortBy::Recent => sorted.sort_by(|a, b| b.mtime.cmp(&a.mtime)), 
        }
        current_pool.extend(sorted);
    }

    let mut remainder = Vec::new();
    let mut current_pack = GroupPlan { files: Vec::new(), est_size: YAML_BASE + TOC_HEAD };

    for f in current_pool {
        let overhead = PER_FILE_OVERHEAD + (f.rel_unix.len() as u64 * 2);
        let total_f_size = f.size + overhead;

        if total_f_size + YAML_BASE + TOC_HEAD > available_bytes + SAFETY_MARGIN {
            completed_packs.push(GroupPlan { files: vec![f.clone()], est_size: total_f_size + YAML_BASE + TOC_HEAD });
            continue;
        }

        let fits = (current_pack.est_size + total_f_size < available_bytes) &&
            (max_ch == 0 || current_pack.files.len() < max_ch);

        if fits {
            current_pack.est_size += total_f_size;
            current_pack.files.push(f);
        } else {
            if !current_pack.files.is_empty() { completed_packs.push(current_pack); }
            current_pack = GroupPlan { files: vec![f], est_size: YAML_BASE + TOC_HEAD + total_f_size };
        }
    }

    remainder.extend(current_pack.files);
    Ok((completed_packs, remainder))
}

// --- Mass: Presentation & Run ---

fn is_system_dir(path: &Path) -> bool {
    let canonical = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let p = canonical.to_string_lossy().to_lowercase();
    ["c:\\windows", "c:\\program files", "/etc", "/usr"].iter().any(|&r| p.starts_with(r))
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Pack { vault_root, output_dir, max_mb, max_chapters, sort_by, exclude, resume, force, manifest, dry_run, quiet, no_progress } => {
            let out = output_dir.unwrap_or_else(|| vault_root.join("_grouped"));
            if is_system_dir(&out) { bail!("System directory guard triggered."); }
            if !dry_run { fs::create_dir_all(&out)?; }

            let excludes_set = build_globset(&exclude)?;
            let state = scan_vault(&vault_root, &out, &excludes_set, sort_by)?; 

            let capacity = (max_mb * 1024.0 * 1024.0) as u64;
            let available = if capacity > SAFETY_MARGIN { capacity - SAFETY_MARGIN } else { capacity };

            let (mut packs, last) = pack_node(Path::new(""), &state, available, max_chapters, sort_by, 0)?;
            if !last.is_empty() { 
                let last_size = last.iter().map(|f| f.size + PER_FILE_OVERHEAD).sum::<u64>() + YAML_BASE + TOC_HEAD;
                packs.push(GroupPlan { files: last, est_size: last_size });
            }

            if packs.is_empty() { 
                if !quiet { println!("No markdown files found to pack."); }
                return Ok(()); 
            }

            if dry_run {
                println!("--- Dry Run: {} packs planned ---", packs.len());
                for (i, p) in packs.iter().enumerate() {
                    println!("Pack {:04}: {} files (~{:.2} MB)", i+1, p.files.len(), p.est_size as f64 / 1024.0 / 1024.0);
                }
                return Ok(());
            }

            let mut manifest_data = Manifest { 
                version: "1.7.0".into(), 
                sort_order: format!("{:?}", sort_by),
                packs: HashMap::new(),
                file_to_pack_map: HashMap::new(),
                global_file_order: Vec::new(),
            };

            let pack_pb = if !quiet && !no_progress { 
                let pb = ProgressBar::new(packs.len() as u64);
                pb.set_style(ProgressStyle::default_bar()
                    .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})")?
                    .progress_chars("#>-"));
                Some(pb)
            } else { None };

            for (i, p) in packs.iter().enumerate() {
                let pack_name = format!("pack_{:04}.md", i + 1);
                
                // Track global order and reverse mapping
                for f in &p.files {
                    manifest_data.global_file_order.push(f.rel_unix.clone());
                    manifest_data.file_to_pack_map.insert(f.rel_unix.clone(), pack_name.clone());
                }

                let dest = out.join(&pack_name);
                if dest.exists() && resume && !force { 
                    if let Some(ref pb) = pack_pb { pb.inc(1); }
                    continue; 
                }

                let tmp = NamedTempFile::new_in(&out)?;
                let mut hasher = Sha256::new();
                {
                    let mut writer = BufWriter::new(tmp.as_file());
                    let header = format!("---\nvault_group: {}\n---\n\n# Table of Contents\n", i + 1);
                    writer.write_all(header.as_bytes())?;
                    hasher.update(header.as_bytes());

                    for (idx, f) in p.files.iter().enumerate() {
                        let line = format!("{}. {}\n", idx + 1, f.rel_unix);
                        writer.write_all(line.as_bytes())?;
                        hasher.update(line.as_bytes());
                    }

                    for (idx, f) in p.files.iter().enumerate() {
                        let chapter = format!("\n---\n# Chapter {}: {}\n\n", idx + 1, f.rel_unix);
                        writer.write_all(chapter.as_bytes())?;
                        hasher.update(chapter.as_bytes());
                        
                        let mut src = BufReader::new(File::open(&f.abs)?);
                        let mut buffer = [0u8; 8192];
                        loop {
                            let n = src.read(&mut buffer)?;
                            if n == 0 { break; }
                            writer.write_all(&buffer[..n])?;
                            hasher.update(&buffer[..n]);
                        }
                    }
                    writer.flush()?;
                }

                manifest_data.packs.insert(pack_name, PackInfo {
                    checksum: format!("{:x}", hasher.finalize()),
                    file_count: p.files.len(),
                });

                if dest.exists() {
                    let backup = dest.with_extension(format!("bak_{}", &Uuid::new_v4().simple().to_string()[..8]));
                    fs::rename(&dest, &backup)?;
                    tmp.persist(&dest)?;
                    let _ = fs::remove_file(backup);
                } else { tmp.persist(&dest)?; }
                if let Some(ref pb) = pack_pb { pb.inc(1); }
            }

            if let Some(pb) = pack_pb { pb.finish_with_message("Packing Complete"); }

            if manifest {
                let manifest_path = out.join("vault_manifest.json");
                let tmp_manifest = NamedTempFile::new_in(&out)?;
                serde_json::to_writer_pretty(tmp_manifest.as_file(), &manifest_data)?;
                tmp_manifest.persist(manifest_path)?;
            }
        }
        Commands::Verify { manifest_path, quiet } => {
            let data = fs::read_to_string(&manifest_path)?;
            let manifest: Manifest = serde_json::from_str(&data)?;
            let base = manifest_path.parent().unwrap_or_else(|| Path::new("."));
            manifest.packs.par_iter().for_each(|(name, info)| {
                let p_path = base.join(name);
                let mut hasher = Sha256::new();
                if let Ok(mut f) = File::open(&p_path) {
                    let _ = std::io::copy(&mut f, &mut hasher);
                    if format!("{:x}", hasher.finalize()) != info.checksum {
                        if !quiet { eprintln!("[FAIL] {}", name); }
                    } else if !quiet { println!("[OK] {}", name); }
                }
            });
        }
    }
    Ok(())
}