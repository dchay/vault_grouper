use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;

use obsidian_vault_grouper::{
    build_glob_set, clean_existing_pack_outputs, execute_pack_write, plan_packs,
    read_already_packed_rel_paths, scan_vault_flat, PackOptions,
};

#[derive(Parser, Debug)]
#[command(
    name = "obsidian_vault_grouper",
    version = "2.5.1",
    about = "Data-Oriented Obsidian Vault Pack Grouper with Windows 11 retry persistence and chronological packing."
)]
struct Args {
    #[arg(short = 'i', long, help = "Path to input Obsidian vault root")]
    vault: PathBuf,

    #[arg(short = 'o', long, help = "Output directory for packs and manifest")]
    out_dir: PathBuf,

    #[arg(
        short = 'n',
        long,
        default_value = "vault",
        help = "Prefix name for output packs"
    )]
    name: String,

    #[arg(
        short = 's',
        long,
        default_value_t = 1.0,
        help = "Target pack size in megabytes (accepts decimals, e.g., 0.5, 12.5; default 1.0 MB)"
    )]
    pack_size: f64,

    #[arg(
        short = 'e',
        long = "exclude",
        help = "Glob patterns to exclude (can be repeated)"
    )]
    excludes: Vec<String>,

    #[arg(long, help = "Include hidden files and dot-directories (e.g., .obsidian)")]
    include_hidden: bool,

    #[arg(long, help = "Resume packing without overwriting existing pack files")]
    resume: bool,

    #[arg(
        short = 'm',
        long = "manifest",
        default_value_t = true,
        action = clap::ArgAction::Set,
        help = "Generate manifest.md table (default: true)"
    )]
    manifest: bool,

    #[arg(long, help = "Simulate plan without writing output files")]
    dry_run: bool,

    #[arg(short = 'q', long, help = "Suppress progress spinners and warnings")]
    quiet: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.pack_size <= 0.0 || !args.pack_size.is_finite() {
        anyhow::bail!("--pack-size (-s) must be a positive decimal number (e.g., 1.0, 12.5)");
    }

    // Convert decimal megabytes (1 MiB = 1,048,576 bytes) to u64 bytes
    let pack_size_bytes = (args.pack_size * 1024.0 * 1024.0) as u64;

    if !args.quiet {
        println!("Scanning vault: {}", args.vault.display());
    }

    let exclude_globs = build_glob_set(&args.excludes)?;
    let mut files = scan_vault_flat(
        &args.vault,
        exclude_globs.as_ref(),
        args.include_hidden,
        args.quiet,
    )?;

    if files.is_empty() {
        if !args.quiet {
            println!("No packable files found in target vault.");
        }
        return Ok(());
    }

    let manifest_name = format!("{}_manifest.md", args.name);
    let pack_prefix = format!("{}_pack_", args.name);

    if args.resume {
        let already_packed = read_already_packed_rel_paths(&args.out_dir, &pack_prefix)?;
        if !already_packed.is_empty() {
            let before_count = files.len();
            files.retain(|f| !already_packed.contains(&f.rel_unix));
            if !args.quiet {
                println!(
                    "Resuming run: skipped {} already-packed files. {} active files remaining.",
                    before_count - files.len(),
                    files.len()
                );
            }
        } else if !args.quiet {
            println!("--resume specified, but no existing manifest or packs were found. Proceeding with full scan.");
        }

        if files.is_empty() {
            if !args.quiet {
                println!("All files in vault are already packed. Nothing to do.");
            }
            return Ok(());
        }
    } else if !args.dry_run {
        clean_existing_pack_outputs(&args.out_dir, &pack_prefix, &manifest_name, args.quiet)
            .context("Failed to clean prior pack outputs")?;
    }

    if !args.quiet {
        println!(
            "Found {} active files sorted by last modified timestamp (descending). Planning packs (target <= {} bytes / {:.2} MB)...",
            files.len(),
            pack_size_bytes,
            args.pack_size
        );
    }

    let plan = plan_packs(&files, pack_size_bytes, args.quiet);

    if args.dry_run {
        println!("\n--- Dry Run Execution Summary ---");
        println!("Total active packable files: {}", files.len());
        println!("Generated packs count: {}", plan.boundaries.len());
        for (i, &(start, end)) in plan.boundaries.iter().enumerate() {
            let slice = &files[start..end];
            let raw_size: u64 = slice.iter().map(|f| f.size).sum();
            let newest = slice.first().map(|f| f.mtime).unwrap_or(0);
            let oldest = slice.last().map(|f| f.mtime).unwrap_or(0);

            println!(
                "  Pack {:04}: {} files, {} raw content bytes, mtime span [{} to {}]",
                i + 1,
                end - start,
                raw_size,
                obsidian_vault_grouper::format_unix_timestamp(oldest),
                obsidian_vault_grouper::format_unix_timestamp(newest)
            );
        }
        return Ok(());
    }

    let opts = PackOptions {
        vault_name: &args.name,
        out_dir: &args.out_dir,
        resume: args.resume,
        write_manifest: args.manifest,
        available_bytes: pack_size_bytes,
        quiet: args.quiet,
    };

    let manifest_entries = execute_pack_write(&files, &plan, &opts)?;

    if !args.quiet {
        println!(
            "Successfully packaged {} files into output directory: {}",
            manifest_entries.len(),
            args.out_dir.display()
        );
    }

    Ok(())
}