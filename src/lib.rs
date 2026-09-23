//! # obsidian_vault_grouper v2.2.1
//! **The Aegis Multimodal Edition.**

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
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

const YAML_BASE: u64 = 128; 
const PER_FILE_OVERHEAD: u64 = 250; 
const SAFETY_MARGIN: u64 = 8 * 1024;
const MAX_RECURSION_DEPTH: u32 = 500;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SortBy { Name, Mtime, Recent }

#[derive(Parser)]
#[command(name = "grouper", version = "2.2.1")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    Pack {
        vault_root: PathBuf,
        output_dir: Option<PathBuf>,
        #[arg(long, default_value = "StarFluke Project")]
        vault_name: String,
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

#[derive(Clone, Debug)]
pub struct MdFile {
    pub abs: PathBuf,
    pub rel_unix: String,
    pub size: u64,
    pub mtime: u64,
    pub is_code: bool,
    pub lang: String,
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

// --- Apparatus: Core Logic Restored ---

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
    let root_canonical = dunce::canonicalize(root)
        .with_context(|| format!("Failed to resolve root: {}", root.display()))?;
    let out_canonical = dunce::canonicalize(out).ok();

    subfolders.insert(PathBuf::from(""), HashSet::new());

    let walker = WalkDir::new(&root_canonical).follow_links(false).sort(true);

    for entry in walker.into_iter().filter_map(|e| e.ok()) {
        let walked_path = entry.path();
        let path = dunce::canonicalize(&walked_path)
            .with_context(|| format!("Failed to canonicalize walked path: {}", walked_path.display()))?;
        if let Some(ref o) = out_canonical { if path.starts_with(o) { continue; } }

        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
        let is_md = ext == "md";
        let is_rs = ext == "rs";
        let is_cs = ext == "cs";
        let is_txt = ext == "txt";

        if (is_md || is_rs || is_cs || is_txt) && entry.file_type().is_file() {
            let rel = path.strip_prefix(&root_canonical)
                .with_context(|| format!(
                    "Walked file is not under root. file={}, root={}",
                    path.display(),
                    root_canonical.display()
                ))?;
            let rel_unix = rel.to_string_lossy().replace('\\', "/");
            if excludes.is_match(&rel_unix) { continue; }

            let parent = rel.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
            let meta = entry.metadata()
                .with_context(|| format!("Failed to read metadata for {}", path.display()))?;
            let mtime = meta.modified()?
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();

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
                abs: path.clone(),
                rel_unix,
                size: meta.len(),
                mtime,
                is_code: is_rs || is_cs,
                lang: if is_rs { "rust".into() } else { "csharp".into() },
            }));
        }
    }
    Ok(BrainState { files_by_folder, subfolders, folder_timestamps })
}

/// Restored recursive packing logic from lib_8.rs
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
        Commands::Pack { vault_root, output_dir, vault_name, max_mb, max_chapters, sort_by, exclude, resume, force, manifest, dry_run, quiet, no_progress } => {
            let out = output_dir.unwrap_or_else(|| vault_root.join("_grouped"));
            if !dry_run {
                fs::create_dir_all(&out)
                    .with_context(|| format!("Failed to create output dir: {}", out.display()))?;
            }

            let excludes_set = build_globset(&exclude)?;
            let state = scan_vault(&vault_root, &out, &excludes_set, sort_by)?; 

            let capacity = (max_mb * 1024.0 * 1024.0) as u64;
            let available = capacity.saturating_sub(SAFETY_MARGIN);

            let (mut packs, last): (Vec<GroupPlan>, Vec<Arc<MdFile>>) = 
                pack_node(Path::new(""), &state, available, max_chapters, sort_by, 0)?;
            
            if !last.is_empty() { 
                let sz: u64 = last.iter().map(|f| f.size + PER_FILE_OVERHEAD).sum();
                packs.push(GroupPlan { files: last, est_size: sz }); 
            }

            if packs.is_empty() { 
                println!("No files found in {:?} matching .md, .rs, or .cs", vault_root);
                return Ok(()); 
            }

            let mut file_to_pack: HashMap<String, String> = HashMap::new();

            for (i, p) in packs.iter().enumerate() {
                // UPDATED: Prepend the vault_name to the filename itself
                // We sanitize the vault_name to ensure it's a valid filename
                let sanitized_name = vault_name.replace(|c: char| !c.is_alphanumeric() && c != '-' && c != '_', "");
                let pack_name = format!("{}-pack_{:04}.md", sanitized_name, i + 1);

                let dest = out.join(&pack_name);

                for f in &p.files {
                    file_to_pack.insert(f.rel_unix.clone(), pack_name.clone());
                }

                let tmp = NamedTempFile::new_in(&out)
                    .with_context(|| format!("Failed to create temp file in {}", out.display()))?;
                {
                    let mut writer = BufWriter::new(tmp.as_file());
                    // This still keeps the header inside the file for NotebookLM
                    writeln!(writer, "# {}\n## PACK ID: {}\n---\n", vault_name, i + 1)?;

                    for f in &p.files {
                        writeln!(writer, "\n--- SOURCE: {} ---", f.rel_unix)?;
                        if f.is_code {
                            writeln!(writer, "```{}\n", f.lang)?;
                        }
                        let mut src = BufReader::new(
                            File::open(&f.abs)
                                .with_context(|| format!("Failed to open source file: {}", f.abs.display()))?
                        );
                        std::io::copy(&mut src, &mut writer)
                            .with_context(|| format!("Failed while copying source file: {}", f.abs.display()))?;
                        if f.is_code {
                            writeln!(writer, "\n```")?;
                        }
                    }
                }
                tmp.persist(&dest)
                    .with_context(|| format!("Failed to persist temp file to {}", dest.display()))?;
            }

            if manifest {
                let sanitized_name = vault_name.replace(|c: char| !c.is_alphanumeric() && c != '-' && c != '_', "");
                // UPDATED: Prepend the vault_name to the manifest filename
                let m_path = out.join(format!("{}-manifest.md", sanitized_name));

                let mut m_writer = BufWriter::new(
                    File::create(&m_path)
                        .with_context(|| format!("Failed to create manifest: {}", m_path.display()))?
                );
                writeln!(m_writer, "# {} - Master Manifest\n", vault_name)?;
                writeln!(m_writer, "| Path | Pack |\n| --- | --- |")?;
                let mut keys: Vec<&String> = file_to_pack.keys().collect();
                keys.sort();
                for k in keys {
                    writeln!(m_writer, "| {} | {} |", k, file_to_pack[k])?;
                }
            }
            
            println!("Success: Generated {} packs for project '{}'", packs.len(), vault_name);
        }
    }
    Ok(())
}
