//! `keel` binary entry point. Uses `anyhow` for context-rich top-level errors.

use anyhow::{Context, Result};
use clap::Parser;
use keel::api::{DependencyDto, ImplDto, ReferenceDto, SymbolDto};
use keel::cli::{commands, Cli, Commands};

fn main() -> Result<()> {
    let cli = Cli::parse();
    let auto_index = !cli.no_auto_index;
    let json = cli.json;
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
            let qr =
                commands::run_definition_meta(&name, auto_index).context("querying definition")?;
            if json {
                let out = qr.map_results(|s| SymbolDto::from(&s));
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                if qr.results.is_empty() {
                    eprintln!("No definition found for {name}");
                }
                for s in qr.results {
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
        }
        Commands::References { name } => {
            let qr =
                commands::run_references_meta(&name, auto_index).context("querying references")?;
            if json {
                let out = qr.map_results(|r| ReferenceDto::from(&r));
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                if qr.results.is_empty() {
                    eprintln!("No references found for {name}");
                }
                for r in qr.results {
                    println!("{}:{}:{}\t{}", r.file.display(), r.start_line, r.start_col, r.name);
                }
            }
        }
        Commands::Callers { name } => {
            let qr = commands::run_callers_meta(&name, auto_index).context("querying callers")?;
            if json {
                let out = qr.map_results(|r| ReferenceDto::from(&r));
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else if qr.results.is_empty() {
                eprintln!("No callers found for {name}");
            } else {
                for r in qr.results {
                    println!("{}:{}:{}\t{}", r.file.display(), r.start_line, r.start_col, r.name);
                }
            }
        }
        Commands::Implementations { name } => {
            let qr = commands::run_implementations_meta(&name, auto_index)
                .context("querying implementations")?;
            if json {
                let out = qr.map_results(|i| ImplDto::from(&i));
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else if qr.results.is_empty() {
                eprintln!("No implementations found for {name}");
            } else {
                for i in qr.results {
                    println!(
                        "{}:{}:{}\t{}",
                        i.file.display(),
                        i.start_line,
                        i.start_col,
                        i.type_name
                    );
                }
            }
        }
        Commands::Dependencies { name } => {
            let qr = commands::run_dependencies_meta(&name, auto_index)
                .context("querying dependencies")?;
            if json {
                let out = qr.map_results(|d| DependencyDto::from(&d));
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else if qr.results.is_empty() {
                eprintln!("No dependencies found for {name}");
            } else {
                for d in qr.results {
                    match &d.file {
                        Some(file) => println!("{}\t{}", d.module_path, file.display()),
                        None => println!("{}", d.module_path),
                    }
                }
            }
        }
        Commands::Impact { name } => {
            let qr = commands::run_impact_meta(&name, auto_index).context("querying impact")?;
            if json {
                let out = qr.map_results(|s| SymbolDto::from(&s));
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else if qr.results.is_empty() {
                eprintln!("No impact found for {name}");
            } else {
                for s in qr.results {
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
