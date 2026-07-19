//! `sb` binary entry point. Uses `anyhow` for context-rich top-level errors.

use anyhow::{Context, Result};
use clap::Parser;
use second_brain::cli::{commands, Cli, Commands};

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Index { path } => {
            let stats = commands::run_index(&path)
                .with_context(|| format!("indexing {}", path.display()))?;
            println!(
                "Indexed {} file(s) (skipped {}, removed {}).",
                stats.indexed, stats.skipped, stats.removed
            );
        }
        Commands::Watch { path } => {
            commands::run_watch(&path)
                .with_context(|| format!("watching {}", path.display()))?;
        }
        Commands::Definition { name } => {
            let defs = commands::run_definition(&name).context("querying definition")?;
            if defs.is_empty() {
                eprintln!("No definition found for {name}");
            }
            for s in defs {
                println!(
                    "{}:{}:{}\t{}\t{}",
                    s.file.display(),
                    s.start_line,
                    s.start_col,
                    s.kind.as_db(),
                    s.name
                );
            }
        }
        Commands::References { name } => {
            let refs = commands::run_references(&name).context("querying references")?;
            if refs.is_empty() {
                eprintln!("No references found for {name}");
            }
            for r in refs {
                println!("{}:{}:{}\t{}", r.file.display(), r.start_line, r.start_col, r.name);
            }
        }
        Commands::Callers { name } => {
            let refs = commands::run_callers(&name).context("querying callers")?;
            if refs.is_empty() {
                eprintln!("No callers found for {name}");
            }
            for r in refs {
                println!("{}:{}:{}\t{}", r.file.display(), r.start_line, r.start_col, r.name);
            }
        }
    }
    Ok(())
}
