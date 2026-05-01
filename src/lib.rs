//! obsidian_vault_grouper v1.6.4
//! The Paladin Edition: Windows 11 Optimization, Atomic Persistence, and CI/CD Mastery.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    sync::{atomic::{AtomicUsize, Ordering}, Arc},
};

use anyhow::{bail, Context, Result};
use chrono::Local;
use clap::{Parser, Subcommand, ValueEnum};
use globset::{Glob, GlobSet, GlobSetBuilder};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use walkdir::WalkDir;
// For unique backups
use dunce;
// Windows-friendly path handling

// --- Brain: Constants & Logic ---
const YAML_BASE: u64 = 64;
const TOC_HEAD: u64 = 40;
const PER_FILE_OVERHEAD: u64 = 25; // Replaces CHAPTER_OVERHEAD_BASE
const SAFETY_MARGIN: u64 = 16 * 1024;
const BUFFER_SIZE: usize = 8192;
const MAX_RECURSION_DEPTH: u32 = 500;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SortBy { Name, Mtime }

#[derive(Parser)]
#[command(name = "grouper", version = "1.6.4", about = "Enterprise-grade Obsidian vault grouping.")]
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
        #[arg(long)]
        quiet: bool,
        #[arg(long)]
        no_progress: bool,
    },
    /// Verify the integrity of existing packs
    Verify {
        #[arg(default_value = "_grouped/manifest.json")]
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
}

#[derive(Debug)]
pub struct GroupPlan {
    pub files: Vec<Arc<MdFile>>,
    pub est_size: u64,
}

#[derive(Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub packs: HashMap<String, PackInfo>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct PackInfo {
    pub checksum: String,
    pub files: Vec<String>,
}

// --- Apparatus: Optimized Discovery ---

fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns { builder.add(Glob::new(pat)?); }
    Ok(builder.build()?)
}

pub fn scan_vault(root: &Path, out: &Path, excludes: &GlobSet) -> Result<BrainState> {
    let mut files_by_folder = HashMap::new();
    let mut subfolders = HashMap::new();
    let root_canonical = dunce::canonicalize(root).context("Failed to resolve vault root path")?;
    let out_canonical = dunce::canonicalize(out).ok();

    subfolders.insert(PathBuf::from(""), HashSet::new());

    let walker = WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name();

    for entry in walker.into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();

        if let Some(ref o) = out_canonical {
            if path.starts_with(o) { continue; }
        }

        if path.extension().map_or(false, |ext| ext == "md") && entry.file_type().is_file() {
            let rel = path.strip_prefix(&root_canonical).unwrap_or(path);
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

// --- Brain: Recursive Planning ---

fn pack_node(
    path: &Path,
    state: &BrainState,
    available_bytes: u64,
    max_ch: usize,
    sort_by: SortBy,
    depth: u32,
) -> Result<(Vec<GroupPlan>, Vec<Arc<MdFile>>)> {
    if depth > MAX_RECURSION_DEPTH { bail!("Recursion safety limit reached (500)."); }

    let mut completed_packs = Vec::new();
    let mut current_pool = Vec::new();

    if let Some(subs) = state.subfolders.get(path) {
        let mut sorted_subs: Vec<_> = subs.iter().collect();
        sorted_subs.sort();
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
            if !current_pack.files.is_empty() {
                completed_packs.push(current_pack);
            }
            current_pack = GroupPlan { files: vec![f], est_size: YAML_BASE + TOC_HEAD + total_f_size };
        }
    }

    remainder.extend(current_pack.files);
    Ok((completed_packs, remainder))
}

// --- Mass: Presentation & Safety ---

fn is_system_dir(path: &Path) -> bool {
    let canonical = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let p = canonical.to_string_lossy().to_lowercase();

    let restricted = [
        "/", "/etc", "/dev", "/bin", "/sbin", "/usr", "/boot",
        "c:\\", "c:\\windows", "c:\\program files", "c:\\program files (x86)",
        "d:\\", "e:\\", "f:\\"
    ];

    restricted.iter().any(|&r| p == r || p.starts_with(&(r.to_owned() + "\\")) || p.starts_with(&(r.to_owned() + "/")))
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Pack { vault_root, output_dir, max_mb, max_chapters, sort_by, exclude, resume, force, manifest, dry_run, quiet, no_progress } => {
            let out = output_dir.unwrap_or_else(|| vault_root.join("_grouped"));

            if out.as_os_str().is_empty() || out == Path::new(".") || out == Path::new("..") {
                bail!("Invalid output directory path.");
            }
            if is_system_dir(&out) { bail!("Refusing to write to protected system directory: {}", out.display()); }
            if !dry_run { fs::create_dir_all(&out)?; }

            let excludes_set = build_globset(&exclude)?;
            let state = scan_vault(&vault_root, &out, &excludes_set)?;

            let capacity = (max_mb * 1024.0 * 1024.0) as u64;
            let available = if capacity > SAFETY_MARGIN { capacity - SAFETY_MARGIN } else { capacity };

            let (mut packs, last) = pack_node(Path::new(""), &state, available, max_chapters, sort_by, 0)?;
            if !last.is_empty() {
                let actual_size = last.iter().map(|f| f.size + PER_FILE_OVERHEAD + (f.rel_unix.len() as u64 * 2)).sum::<u64>() + YAML_BASE + TOC_HEAD;
                packs.push(GroupPlan { files: last, est_size: actual_size });
            }

            if packs.is_empty() {
                if !quiet { println!("No files found to process."); }
                return Ok(());
            }

            if dry_run {
                println!("Dry Run: {} packs planned containing {} files.", packs.len(), packs.iter().map(|p| p.files.len()).sum::<usize>());
                return Ok(());
            }

            let show_progress = !quiet && !no_progress && atty::is(atty::Stream::Stdout);
            let mut pack_pb = None;
            if show_progress {
                let pb = ProgressBar::new(packs.len() as u64);
                pb.set_style(ProgressStyle::default_bar().template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}")?);
                pack_pb = Some(pb);
            }

            let mut manifest_data = Manifest { version: "1.6.4".into(), packs: HashMap::new() };

            for (i, p) in packs.iter().enumerate() {
                let pack_name = format!("pack_{:04}.md", i + 1);
                let dest = out.join(&pack_name);

                if dest.exists() && resume && !force {
                    if let Some(ref pb) = pack_pb { pb.inc(1); }
                    continue;
                }
                if dest.is_dir() { bail!("Cannot overwrite directory: {}", dest.display()); }

                let tmp = NamedTempFile::new_in(&out)?;
                let mut hasher = Sha256::new();
                let mut written: u64 = 0;

                {
                    let mut writer = BufWriter::new(tmp.as_file());
                    let header = format!("---\nvault_group: {}\n---\n\n# Table of Contents\n", i + 1);
                    writer.write_all(header.as_bytes())?;
                    hasher.update(header.as_bytes());
                    written += header.len() as u64;

                    for (idx, f) in p.files.iter().enumerate() {
                        let line = format!("{}. {}\n", idx + 1, f.rel_unix);
                        writer.write_all(line.as_bytes())?;
                        hasher.update(line.as_bytes());
                        written += line.len() as u64;
                    }

                    for (idx, f) in p.files.iter().enumerate() {
                        let chapter = format!("\n---\n# Chapter {}: {}\n\n", idx + 1, f.rel_unix);
                        writer.write_all(chapter.as_bytes())?;
                        hasher.update(chapter.as_bytes());
                        written += chapter.len() as u64;

                        let mut src = BufReader::new(File::open(&f.abs)?);
                        let mut buf = [0u8; BUFFER_SIZE];
                        while let Ok(n) = src.read(&mut buf) {
                            if n == 0 { break; }
                            if written + (n as u64) > capacity + SAFETY_MARGIN {
                                bail!("Size Limit Exceeded: Pack {} exceeds user-defined max_mb.", pack_name);
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

                // Atomic Swap with Collision-Proof Backups
                if dest.exists() {
                    let ts = Local::now().format("%Y%m%d_%H%M%S");
                    let backup = dest.with_extension(format!("bak_{}", ts));
                    fs::rename(&dest, &backup).context("Failed to create backup")?;

                    if let Err(e) = tmp.persist(&dest) {
                        fs::rename(&backup, &dest).ok(); // Attempt restore
                        return Err(e.into());
                    }
                    fs::remove_file(backup).ok(); // Cleanup backup
                } else {
                    tmp.persist(&dest).context("Failed to persist pack")?;
                }

                if let Some(ref pb) = pack_pb { pb.inc(1); }
            }

            if manifest {
                fs::write(out.join("manifest.json"), serde_json::to_string_pretty(&manifest_data)?)?;
            }
            if let Some(pb) = pack_pb { pb.finish_with_message("Done"); }
        }
        Commands::Verify { manifest_path, quiet } => {
            let data = fs::read_to_string(&manifest_path).context("Manifest not found")?;
            let manifest: Manifest = serde_json::from_str(&data)?;
            let base = manifest_path.parent().unwrap_or_else(|| Path::new("."));
            let errors = AtomicUsize::new(0);

            manifest.packs.par_iter().for_each(|(name, info)| {
                let p_path = base.join(name);
                let res: Result<()> = (|| {
                    let mut hasher = Sha256::new();
                    let mut f = File::open(&p_path).with_context(|| format!("Unreadable pack: {}", name))?;
                    let mut buf = [0u8; BUFFER_SIZE];
                    while let Ok(n) = f.read(&mut buf) {
                        if n == 0 { break; }
                        hasher.update(&buf[..n]);
                    }
                    if format!("{:x}", hasher.finalize()) != info.checksum {
                        bail!("Checksum mismatch: {}", name);
                    }
                    Ok(())
                })();

                match res {
                    Ok(_) => if !quiet { println!("[OK] {}", name); },
                    Err(e) => {
                        if !quiet { eprintln!("[FAIL] {}", e); }
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });

            let count = errors.load(Ordering::Relaxed);
            if count > 0 { bail!("Verification Failed: {} packs are invalid.", count); }
            else if !quiet { println!("All packs verified successfully."); }
        }
    }
    Ok(())
}
