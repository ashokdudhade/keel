//! `keel` binary entry point. Uses `anyhow` for context-rich top-level errors.

use anyhow::{Context, Result};
use clap::Parser;
use keel::cli::{commands, Cli, Commands};

fn main() -> Result<()> {
    let cli = Cli::parse();
    let auto_index = !cli.no_auto_index;
    match cli.command {
        Commands::Index { path } => {
            let stats = commands::run_index(&path)
                .with_context(|| format!("indexing {}", path.display()))?;
            println!(
                "Indexed {} file(s) (skipped {}, removed {}, errors {}).",
                stats.indexed, stats.skipped, stats.removed, stats.errors
            );
        }
        Commands::Watch { path } => {
            commands::run_watch(&path)
                .with_context(|| format!("watching {}", path.display()))?;
        }
        Commands::Daemon { port } => {
            commands::run_daemon(port).context("running keel daemon")?;
        }
        Commands::Start { path } => {
            commands::run_start(&path)
                .with_context(|| format!("registering project {}", path.display()))?;
        }
        Commands::Stop => {
            commands::run_stop().context("unregistering project")?;
        }
        Commands::Status => {
            commands::run_status().context("daemon status")?;
        }
        Commands::Definition { name } => {
            let defs =
                commands::run_definition(&name, auto_index).context("querying definition")?;
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
            let refs =
                commands::run_references(&name, auto_index).context("querying references")?;
            if refs.is_empty() {
                eprintln!("No references found for {name}");
            }
            for r in refs {
                println!("{}:{}:{}\t{}", r.file.display(), r.start_line, r.start_col, r.name);
            }
        }
        Commands::Callers { name } => {
            let refs = commands::run_callers(&name, auto_index).context("querying callers")?;
            if refs.is_empty() {
                eprintln!("No callers found for {name}");
            }
            for r in refs {
                println!("{}:{}:{}\t{}", r.file.display(), r.start_line, r.start_col, r.name);
            }
        }
        Commands::Implementations { name } => {
            let impls = commands::run_implementations(&name, auto_index)
                .context("querying implementations")?;
            if impls.is_empty() {
                eprintln!("No implementations found for {name}");
            }
            for i in impls {
                println!(
                    "{}:{}:{}\t{}",
                    i.file.display(),
                    i.start_line,
                    i.start_col,
                    i.type_name
                );
            }
        }
        Commands::Dependencies { name } => {
            let deps =
                commands::run_dependencies(&name, auto_index).context("querying dependencies")?;
            if deps.is_empty() {
                eprintln!("No dependencies found for {name}");
            }
            for d in deps {
                match &d.file {
                    Some(file) => println!("{}\t{}", d.module_path, file.display()),
                    None => println!("{}", d.module_path),
                }
            }
        }
        Commands::Impact { name } => {
            let impacted = commands::run_impact(&name, auto_index).context("querying impact")?;
            if impacted.is_empty() {
                eprintln!("No impact found for {name}");
            }
            for s in impacted {
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
        Commands::Serve { port } => {
            commands::run_serve(port, auto_index).context("serving JSON API")?;
        }
        Commands::Mcp => {
            commands::run_mcp(auto_index).context("serving MCP")?;
        }
    }
    Ok(())
}
