//! obsidian_vault_grouper v1.5.0
//! Production-Ready Edition: Fixed Globbing, Shadowing, and Integrity.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufWriter, Write, Read, BufReader},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use globset::{Glob, GlobSet, GlobSetBuilder};
use jwalk::WalkDir;
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Serialize, Deserialize};
use sha2::{Sha256, Digest};
use tempfile::NamedTempFile;

// --- Brain: Constants & Calculus ---
const YAML_BASE: u64 = 64;
const TOC_HEAD: u64 = 40;
const CHAPTER_OVERHEAD_BASE: u64 = 60; // Base overhead per file (TOC line + Header)
const SAFETY_MARGIN: u64 = 10 * 1024; // 10KB buffer

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SortBy { Name, Mtime }

#[derive(Parser)]
#[command(name = "grouper", version = "1.5.0", about = "High-performance Obsidian vault grouping.")]
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

// --- Apparatus: Sensory Discovery ---

fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        builder.add(Glob::new(pat)?);
    }
    Ok(builder.build()?)
}

pub fn scan_vault(root: &Path, out: &Path, excludes: &GlobSet) -> Result<BrainState> {
    let mut files_by_folder = HashMap::new();
    let mut subfolders = HashMap::new();
    let root_canonical = root.canonicalize()?;
    let out_canonical = if out.exists() { Some(out.canonicalize()?) } else { None };

    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        let path = entry.path();

        // Skip output directory to prevent infinite loops
        if let Some(ref o) = out_canonical {
            if path.starts_with(o) { continue; }
        }

        if path.extension().map_or(false, |ext| ext == "md") && entry.file_type().is_file() {
            let rel = path.strip_prefix(&root_canonical)?;
            let rel_unix = rel.to_string_lossy().replace('\\', "/");

            if excludes.is_match(&rel_unix) { continue; }

            let parent = rel.parent().unwrap_or_else(|| Path::new("")).to_path_buf();

            // Register hierarchy
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

// --- Brain: Recursive Bubbling Engine ---

fn pack_node(
    path: &Path,
    state: &BrainState,
    max_bytes: u64,
    max_ch: usize,
    sort_by: SortBy,
    depth: u32,
) -> Result<(Vec<GroupPlan>, Vec<MdFile>)> {
    if depth > 500 { bail!("Vault depth safety limit exceeded."); }

    let mut all_completed_packs = Vec::new();
    let mut current_pool = Vec::new();

    // 1. Process Children
    if let Some(subs) = state.subfolders.get(path) {
        let mut sorted_subs: Vec<_> = subs.iter().collect();
        sorted_subs.sort();
        for sub in sorted_subs {
            let (mut child_packs, child_rem) = pack_node(sub, state, max_bytes, max_ch, sort_by, depth + 1)?;
            all_completed_packs.append(&mut child_packs);
            current_pool.extend(child_rem);
        }
    }

    // 2. Add local files
    if let Some(locals) = state.files_by_folder.get(path) {
        let mut sorted = locals.clone();
        match sort_by {
            SortBy::Name => sorted.sort_by(|a, b| a.rel_unix.cmp(&b.rel_unix)),
            SortBy::Mtime => sorted.sort_by(|a, b| a.mtime.cmp(&b.mtime)),
        }
        current_pool.extend(sorted);
    }

    // 3. Grouping logic
    let mut final_remainder = Vec::new();
    let mut current_pack = GroupPlan { files: Vec::new(), est_size: YAML_BASE + TOC_HEAD };

    for f in current_pool {
        let file_overhead = CHAPTER_OVERHEAD_BASE + (f.rel_unix.len() as u64 * 2);

        // Handle Behemoths: If single file + mandatory overhead exceeds limit
        if f.size + file_overhead + YAML_BASE + TOC_HEAD > max_bytes {
            let size = f.size;
            all_completed_packs.push(GroupPlan {
                files: vec![f],
                est_size: size + file_overhead + YAML_BASE + TOC_HEAD
            });
            continue;
        }

        let fits = (current_pack.est_size + f.size + file_overhead < max_bytes) &&
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

// --- Mass: Presentation & Integrity ---

pub fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Pack { vault_root, output_dir, max_mb, max_chapters, sort_by, exclude, resume, force, manifest } => {
            let out = output_dir.unwrap_or_else(|| vault_root.join("_grouped"));
            fs::create_dir_all(&out)?;

            let excludes_set = build_globset(&exclude)?;
            let state = scan_vault(&vault_root, &out, &excludes_set)?;
            let max_b = (max_mb * 1024.0 * 1024.0) as u64;

            let (mut packs, last) = pack_node(Path::new(""), &state, max_b, max_chapters, sort_by, 0)?;
            if !last.is_empty() {
                packs.push(GroupPlan { files: last, est_size: 0 });
            }

            let pb = ProgressBar::new(packs.len() as u64);
            pb.set_style(ProgressStyle::default_bar().template("[{bar:40}] {pos}/{len} - {msg}")?);

            let mut manifest_data = Manifest { version: "1.5.0".into(), packs: HashMap::new() };

            for (i, p) in packs.iter().enumerate() {
                let pack_name = format!("pack_{:04}.md", i + 1);
                let dest = out.join(&pack_name);

                if resume && !force && dest.exists() { pb.inc(1); continue; }
                if force && dest.exists() { fs::remove_file(&dest)?; }

                let tmp = NamedTempFile::new_in(&out)?;
                let mut hasher = Sha256::new();
                let mut current_written_size: u64 = 0;

                {
                    let mut writer = BufWriter::new(tmp.as_file());
                    let head = format!("---\nvault_group: {}\n---\n\n# Table of Contents\n", i + 1);
                    writer.write_all(head.as_bytes())?;
                    hasher.update(head.as_bytes());
                    current_written_size += head.len() as u64;

                    for (idx, f) in p.files.iter().enumerate() {
                        let line = format!("{}. {}\n", idx + 1, f.rel_unix);
                        writer.write_all(line.as_bytes())?;
                        hasher.update(line.as_bytes());
                        current_written_size += line.len() as u64;
                    }

                    for (idx, f) in p.files.iter().enumerate() {
                        let header = format!("\n---\n# Chapter {}: {}\n\n", idx + 1, f.rel_unix);
                        writer.write_all(header.as_bytes())?;
                        hasher.update(header.as_bytes());
                        current_written_size += header.len() as u64;

                        // Size-guarded stream copy
                        let mut src = BufReader::new(File::open(&f.abs)?);
                        let mut buffer = [0u8; 8192];
                        while let Ok(n) = src.read(&mut buffer) {
                            if n == 0 { break; }
                            if current_written_size + (n as u64) > max_b + SAFETY_MARGIN {
                                bail!("Pack {} exceeded size limit during write. Source data may have changed.", pack_name);
                            }
                            writer.write_all(&buffer[..n])?;
                            hasher.update(&buffer[..n]);
                            current_written_size += n as u64;
                        }
                    }
                }

                let checksum = format!("{:x}", hasher.finalize());
                manifest_data.packs.insert(pack_name, PackInfo {
                    checksum,
                    files: p.files.iter().map(|f| f.rel_unix.clone()).collect(),
                });

                tmp.persist(&dest)?;
                pb.inc(1);
            }

            if manifest {
                fs::write(out.join("manifest.json"), serde_json::to_string_pretty(&manifest_data)?)?;
            }
            pb.finish_with_message("Vault packing complete.");
        }
        Commands::Verify { manifest_path } => {
            let m_content = fs::read_to_string(&manifest_path).context("Could not read manifest")?;
            let manifest: Manifest = serde_json::from_str(&m_content)?;
            let base_dir = manifest_path.parent().unwrap();

            for (name, info) in manifest.packs {
                let p_path = base_dir.join(&name);
                if !p_path.exists() { println!("[MISSING] {}", name); continue; }

                let mut hasher = Sha256::new();
                let mut f = File::open(p_path)?;
                let mut buffer = [0u8; 8192];
                while let Ok(n) = f.read(&mut buffer) {
                    if n == 0 { break; }
                    hasher.update(&buffer[..n]);
                }
                let hash = format!("{:x}", hasher.finalize());

                if hash == info.checksum {
                    println!("[OK] {}", name);
                } else {
                    println!("[CORRUPT] {}", name);
                }
            }
        }
    }
    Ok(())
}
