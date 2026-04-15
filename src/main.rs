use obsidian_vault_grouper::run_cli;

fn main() {
    if let Err(e) = run_cli() {
        eprintln!("[ERROR] {e:#}");
        std::process::exit(1);
    }
}
