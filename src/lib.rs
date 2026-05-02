//! # obsidian_vault_grouper v2.0.0
//! **The Aegis Edition: Code-Panel Scraper.**
//!
//! Optimized for scooping C# and Rust source trees into 2MB
//! Markdown code-fenced packs for NotebookLM.

use std::{
    fs::{self, File},
    io::{BufReader, BufWriter, Write},
    path::PathBuf
    ,
};

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use tempfile::NamedTempFile;
use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SortBy { Name, Mtime, Recent }

#[derive(Parser)]
#[command(name = "aegis-code", version = "2.0.0")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    Pack {
        src_root: PathBuf,
        output_dir: Option<PathBuf>,
        #[arg(long, default_value_t = 2.0)]
        max_mb: f64,
        #[arg(long, value_enum, default_value_t = SortBy::Name)]
        sort_by: SortBy,
    },
}

struct SourceFile {
    abs: PathBuf,
    rel: String,
    ext: String,
    size: u64,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Pack { src_root, output_dir, max_mb, sort_by } => {
            let out = output_dir.unwrap_or_else(|| PathBuf::from("./_code_packs"));
            fs::create_dir_all(&out)?;

            let mut files = Vec::new();
            for entry in WalkDir::new(&src_root).into_iter().filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.is_file() {
                    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
                    if ext == "rs" || ext == "cs" {
                        // Skip build artifacts
                        let p_str = path.to_string_lossy();
                        if p_str.contains("/target/") || p_str.contains("/bin/") || p_str.contains("/obj/") {
                            continue;
                        }

                        files.push(SourceFile {
                            abs: path.to_path_buf(),
                            rel: path.strip_prefix(&src_root)?.to_string_lossy().replace('\\', "/"),
                            ext: ext.to_string(),
                            size: entry.metadata()?.len(),
                        });
                    }
                }
            }

            // Sort logic
            match sort_by {
                SortBy::Name => files.sort_by(|a, b| a.rel.cmp(&b.rel)),
                _ => {} // Extend as needed for mtime
            }

            let capacity = (max_mb * 1024.0 * 1024.0) as u64;
            let mut packs = Vec::new();
            let mut current_pack = Vec::new();
            let mut current_size = 0u64;

            for f in files {
                if current_size + f.size > capacity && !current_pack.is_empty() {
                    packs.push(current_pack);
                    current_pack = Vec::new();
                    current_size = 0;
                }
                current_size += f.size + 200; // Overhead for fences
                current_pack.push(f);
            }
            if !current_pack.is_empty() { packs.push(current_pack); }

            let mut manifest = String::from("# Code Vault Manifest\n\n| File | Pack |\n| --- | --- |\n");

            for (i, p) in packs.iter().enumerate() {
                let pack_name = format!("code_pack_{:03}.md", i + 1);
                let dest = out.join(&pack_name);
                let tmp = NamedTempFile::new_in(&out)?;

                {
                    let mut writer = BufWriter::new(tmp.as_file());
                    writeln!(writer, "---\npack_id: {}\n---\n", i + 1)?;

                    for f in p {
                        manifest.push_str(&format!("| {} | {} |\n", f.rel, pack_name));

                        let lang = if f.ext == "rs" { "rust" } else { "csharp" };
                        writeln!(writer, "## FILE: {}\n```{}\n", f.rel, lang)?;

                        let mut src = BufReader::new(File::open(&f.abs)?);
                        std::io::copy(&mut src, &mut writer)?;

                        writeln!(writer, "\n```\n---")?;
                    }
                }
                tmp.persist(dest)?;
            }

            fs::write(out.join("code_manifest.md"), manifest)?;
            println!("Successfully packed {} code packs to {:?}", packs.len(), out);
        }
    }
    Ok(())
}