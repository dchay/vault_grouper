//! # obsidian_vault_grouper v1.9.0
//! **The Aegis Edition: Micro-Packer.**
//! 
//! Features: 2MB ultra-stable chunks, Markdown-only manifest, 
//! and Windows 11 I/O optimizations.

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

// --- Brain: Constants ---
const YAML_BASE: u64 = 64;
const PER_FILE_OVERHEAD: u64 = 150; 
const SAFETY_MARGIN: u64 = 8 * 1024;
const MAX_RECURSION_DEPTH: u32 = 500;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SortBy { 
    Name, 
    Mtime, 
    Recent 
}

#[derive(Parser)]
#[command(name = "grouper", version = "1.9.0")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    Pack {
        vault_root: PathBuf,
        output_dir: Option<PathBuf>,
        #[arg(long, default_value_t = 2.0)]
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

struct ManifestData {
    pub sort_order: String,
    pub packs: HashMap<String, PackInfo>,
    pub file_to_pack_map: HashMap<String, String>,
}

struct PackInfo {
    pub checksum: String,
    pub file_count: usize,
}

// --- Apparatus ---

fn is_reserved_name(name: &str) -> bool {
    let n = name.to_uppercase();
    ["CON", "PRN", "AUX", "NUL", "CLOCK$"].contains(&n.as_str())
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
    let mut folder_timestamps = HashMap::new();
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
            let meta = entry.metadata()?;
            let mtime = meta.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_secs();

            let mut curr = parent.as_path();
            loop {
                let p_buf = curr.to_path_buf();
                let timestamp = folder_timestamps.entry(p_buf.clone()).or_insert(mtime);
                match sort_by {
                    SortBy::Recent => if mtime > *timestamp { *timestamp = mtime; },
                    SortBy::Mtime => if mtime < *timestamp { *timestamp = mtime; },
                    _ => {}
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

fn pack_node(
    path: &Path,
    state: &BrainState,
    available_bytes: u64,
    max_ch: usize,
    sort_by: SortBy,
    depth: u32,
) -> Result<(Vec<GroupPlan>, Vec<Arc<MdFile>>)> {
    if depth > MAX_RECURSION_DEPTH { bail!("Recursion limit reached."); }
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
    let mut current_pack = GroupPlan { files: Vec::new(), est_size: YAML_BASE };

    for f in current_pool {
        let total_f_size = f.size + PER_FILE_OVERHEAD;
        let fits = (current_pack.est_size + total_f_size < available_bytes) &&
            (max_ch == 0 || current_pack.files.len() < max_ch);

        if fits {
            current_pack.est_size += total_f_size;
            current_pack.files.push(f);
        } else {
            if !current_pack.files.is_empty() { completed_packs.push(current_pack); }
            current_pack = GroupPlan { files: vec![f], est_size: YAML_BASE + total_f_size };
        }
    }
    remainder.extend(current_pack.files);
    Ok((completed_packs, remainder))
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Pack { vault_root, output_dir, max_mb, max_chapters, sort_by, exclude, resume, force, manifest, dry_run, quiet, no_progress } => {
            let out = output_dir.unwrap_or_else(|| vault_root.join("_grouped"));
            if !dry_run { fs::create_dir_all(&out)?; }

            let excludes_set = build_globset(&exclude)?;
            let state = scan_vault(&vault_root, &out, &excludes_set, sort_by)?; 

            let capacity = (max_mb * 1024.0 * 1024.0) as u64;
            let available = capacity.saturating_sub(SAFETY_MARGIN);

            let (mut packs, last) = pack_node(Path::new(""), &state, available, max_chapters, sort_by, 0)?;
            if !last.is_empty() { 
                let sz = last.iter().map(|f| f.size + PER_FILE_OVERHEAD).sum::<u64>() + YAML_BASE;
                packs.push(GroupPlan { files: last, est_size: sz });
            }

            if packs.is_empty() { return Ok(()); }

            let mut manifest_data = ManifestData { 
                sort_order: format!("{:?}", sort_by),
                packs: HashMap::new(),
                file_to_pack_map: HashMap::new(),
            };

            let pack_pb = if !quiet && !no_progress {
                let pb = ProgressBar::new(packs.len() as u64);
                pb.set_style(ProgressStyle::default_bar().template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len}")?);
                Some(pb)
            } else { None };

            for (i, p) in packs.iter().enumerate() {
                let pack_name = format!("pack_{:04}.md", i + 1);
                for f in &p.files {
                    manifest_data.file_to_pack_map.insert(f.rel_unix.clone(), pack_name.clone());
                }

                let dest = out.join(&pack_name);
                if dest.exists() && resume && !force { continue; }

                let tmp = NamedTempFile::new_in(&out)?;
                let mut hasher = Sha256::new();
                {
                    let mut writer = BufWriter::new(tmp.as_file());
                    let header = format!("---\npack_id: {}\n---\n\n", i + 1);
                    writer.write_all(header.as_bytes())?;
                    hasher.update(header.as_bytes());

                    for f in &p.files {
                        let sep = format!("\n\n--- SOURCE: {} ---\n\n", f.rel_unix);
                        writer.write_all(sep.as_bytes())?;
                        hasher.update(sep.as_bytes());

                        if let Ok(mut src) = File::open(&f.abs).map(BufReader::new) {
                            let mut buf = [0u8; 8192];
                            while let Ok(n) = src.read(&mut buf) {
                                if n == 0 { break; }
                                writer.write_all(&buf[..n])?;
                                hasher.update(&buf[..n]);
                            }
                        }
                    }
                    writer.flush()?;
                }

                manifest_data.packs.insert(pack_name, PackInfo {
                    checksum: format!("{:x}", hasher.finalize()),
                    file_count: p.files.len(),
                });

                tmp.persist(&dest)?;
                if let Some(ref pb) = pack_pb { pb.inc(1); }
            }

            if manifest {
                let md_manifest_path = out.join("vault_manifest.md");
                let tmp_md = NamedTempFile::new_in(&out)?;
                {
                    let mut writer = BufWriter::new(tmp_md.as_file());
                    writeln!(writer, "# Vault Master Index\n")?;
                    writeln!(writer, "## Metadata\n- **Order**: {}\n- **Pack Target**: {} MB\n", manifest_data.sort_order, max_mb)?;
                    
                    writeln!(writer, "## Global File Registry\n")?;
                    writeln!(writer, "| Original Vault Path | Container Pack |")?;
                    writeln!(writer, "| --- | --- |")?;
                    let mut entries: Vec<_> = manifest_data.file_to_pack_map.iter().collect();
                    entries.sort_by_key(|(k, _)| *k);
                    for (file, pack) in entries {
                        writeln!(writer, "| {} | {} |", file, pack)?;
                    }
                }
                tmp_md.persist(md_manifest_path)?;
            }
        }
    }
    Ok(())
}