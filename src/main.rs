use anyhow::{bail, Context, Result};
use clap::{ArgAction, Parser};
use std::path::PathBuf;

use obsidian_vault_grouper::{
    clean_existing_pack_outputs, execute_pack_write, plan_packs, scan_vault_flat, sort_files,
    PackOptions, SortBy,
};

#[derive(Parser, Debug)]
#[command(
    name = "obsidian_vault_grouper",
    version = "2.4.3",
    about = "Data-oriented Vault Pack Grouper for Obsidian Vaults"
)]
struct Args {
    /// Target vault directory path
    #[arg(short, long)]
    vault: PathBuf,

    /// Output directory for packs and manifest
    #[arg(short, long)]
    out: PathBuf,

    /// Maximum pack size in Megabytes (MB)
    #[arg(short, long, default_value_t = 25)]
    max_mb: u64,

    /// Safety margin percentage subtracted from target capacity (0 to 50%)
    #[arg(long, default_value_t = 5.0)]
    safety_margin: f64,

    /// Force clean existing pack files in output directory before execution
    #[arg(long, action = ArgAction::SetTrue)]
    force: bool,

    /// Resume packing by skipping files already listed in existing manifest
    #[arg(long, action = ArgAction::SetTrue)]
    resume: bool,

    /// Explicitly disable manifest generation (manifest is enabled by default)
    #[arg(long, action = ArgAction::SetTrue)]
    no_manifest: bool,

    /// Glob exclude patterns (can be specified multiple times)
    #[arg(short, long)]
    exclude: Vec<String>,

    /// Sort files prior to packing [options: recent, oldest, path, size]
    #[arg(long, default_value = "recent")]
    sort: String,

    /// Perform a dry run without writing pack files to disk
    #[arg(long, action = ArgAction::SetTrue)]
    dry_run: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    if !args.vault.exists() || !args.vault.is_dir() {
        bail!("Vault directory does not exist or is not a directory: {}", args.vault.display());
    }

    if args.max_mb == 0 {
        bail!("--max-mb must be greater than zero.");
    }

    if !(0.0..=50.0).contains(&args.safety_margin) {
        bail!(
            "--safety-margin must be between 0 and 50 percent; got {}",
            args.safety_margin
        );
    }

    let raw_name = args
        .vault
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("vault");

    let sanitized_name: String = raw_name
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect();

    let vault_name = if sanitized_name.is_empty() {
        "vault".to_string()
    } else {
        sanitized_name
    };

    let write_manifest = !args.no_manifest;

    if args.force && args.resume {
        bail!("Cannot specify both --force and --resume. Choose one strategy.");
    }

    if args.force && !args.dry_run {
        let manifest_name = format!("{vault_name}-manifest.md");
        clean_existing_pack_outputs(&args.out, &vault_name, &manifest_name)?;
    }

    let sort_by = match args.sort.to_lowercase().as_str() {
        "recent" => SortBy::Recent,
        "oldest" => SortBy::Oldest,
        "path" => SortBy::Path,
        "size" => SortBy::Size,
        other => bail!("Invalid sort option '{other}'. Valid options: recent, oldest, path, size."),
    };

    println!("Scanning vault flat at '{}'...", args.vault.display());
    let mut files = scan_vault_flat(&args.vault, &args.out, &args.exclude)
        .context("Failed during vault scanning")?;

    if files.is_empty() {
        println!("No matching files found in vault.");
        return Ok(());
    }

    println!("Scanned {} files. Sorting by '{:?}'...", files.len(), sort_by);
    sort_files(&mut files, sort_by);

    let max_bytes = args
        .max_mb
        .checked_mul(1024 * 1024)
        .ok_or_else(|| anyhow::anyhow!("--max-mb {} overflows byte capacity", args.max_mb))?;

    let margin_bytes = ((max_bytes as f64) * (args.safety_margin / 100.0)) as u64;
    let available_bytes = max_bytes.saturating_sub(margin_bytes);

    if available_bytes == 0 {
        bail!("Safety margin resulted in 0 available pack bytes. Lower safety margin or increase --max-mb.");
    }

    println!(
        "Planning packs: target max {} MB ({} bytes available after {}% safety margin)...",
        args.max_mb, available_bytes, args.safety_margin
    );
    let plan = plan_packs(&files, available_bytes);

    if args.dry_run {
        println!("\n=== Dry Run Plan Summary ===");
        println!("Vault Name:         {vault_name}");
        println!("Total Files:        {}", files.len());
        println!("Planned Packs:      {}", plan.boundaries.len());
        println!("Available Capacity: {available_bytes} bytes per pack");
        for (idx, &(s, e)) in plan.boundaries.iter().enumerate() {
            let slice = &files[s..e];
            let size_sum: u64 = slice.iter().map(|f| f.size).sum();
            println!(
                " Pack {:04}: {} files, ~{} bytes total ('{}' to '{}')",
                idx + 1,
                slice.len(),
                size_sum,
                slice.first().map(|f| f.rel_unix.as_str()).unwrap_or(""),
                slice.last().map(|f| f.rel_unix.as_str()).unwrap_or("")
            );
        }
        println!("Dry run complete. No files written.\n");
        return Ok(());
    }

    println!(
        "Writing {} planned pack(s) to '{}'...",
        plan.boundaries.len(),
        args.out.display()
    );
    let pack_opts = PackOptions {
        vault_name: &vault_name,
        out_dir: &args.out,
        resume: args.resume,
        write_manifest,
        available_bytes,
    };

    execute_pack_write(&files, &plan, &pack_opts)?;

    println!("Vault grouping completed successfully.");
    Ok(())
}