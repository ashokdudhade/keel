//! `keel` binary entry point. Uses `anyhow` for context-rich top-level errors.

use anyhow::{Context, Result};
use clap::Parser;
use keel::api::{DependencyDto, ImplDto, ReferenceDto, SymbolDto};
use keel::cli::{commands, Cli, Commands};

/// Record one CLI query for the Insights portal (best-effort, local only).
fn log_cli_query<T>(
    tool: &str,
    target: &str,
    qr: &keel::QueryResult<T>,
    start: std::time::Instant,
) {
    let summary = keel::usage::QuerySummary::from_query_result(qr);
    keel::usage::log_query_at(
        &keel::usage::log_path_for_db(&commands::db_path()),
        keel::usage::Surface::Cli,
        tool,
        target,
        None,
        &summary,
        start.elapsed().as_millis() as u64,
    );
}

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
        Commands::DaemonStop => {
            commands::run_daemon_stop().context("stopping keel daemon")?;
        }
        Commands::Doctor { path } => {
            commands::run_doctor(&path)
                .with_context(|| format!("diagnosing {}", path.display()))?;
        }
        Commands::Init { path } => {
            commands::run_init(&path)
                .with_context(|| format!("initializing {}", path.display()))?;
        }
        Commands::Definition { name } => {
            let start = std::time::Instant::now();
            let qr =
                commands::run_definition_meta(&name, auto_index).context("querying definition")?;
            log_cli_query("definition", &name, &qr, start);
            if json {
                let out = qr.map_results(|s| SymbolDto::from(&s));
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                if qr.results.is_empty() {
                    eprintln!("No definition found for {name}");
                    for n in commands::extra_miss_notes(&qr.notes) {
                        eprintln!("{n}");
                    }
                }
                for s in qr.results {
                    println!("{}", commands::format_symbol_hit(&s));
                }
            }
        }
        Commands::References { name } => {
            let start = std::time::Instant::now();
            let qr =
                commands::run_references_meta(&name, auto_index).context("querying references")?;
            log_cli_query("references", &name, &qr, start);
            if json {
                let out = qr.map_results(|r| ReferenceDto::from(&r));
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                if qr.results.is_empty() {
                    eprintln!("No references found for {name}");
                    for n in commands::extra_miss_notes(&qr.notes) {
                        eprintln!("{n}");
                    }
                }
                for r in qr.results {
                    println!("{}", commands::format_reference_hit(&r));
                }
            }
        }
        Commands::Callers { name } => {
            let start = std::time::Instant::now();
            let qr = commands::run_callers_meta(&name, auto_index).context("querying callers")?;
            log_cli_query("callers", &name, &qr, start);
            if json {
                let out = qr.map_results(|r| ReferenceDto::from(&r));
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else if qr.results.is_empty() {
                eprintln!("No callers found for {name}");
                for n in commands::extra_miss_notes(&qr.notes) {
                    eprintln!("{n}");
                }
            } else {
                for r in qr.results {
                    println!("{}", commands::format_reference_hit(&r));
                }
            }
        }
        Commands::Implementations { name } => {
            let start = std::time::Instant::now();
            let qr = commands::run_implementations_meta(&name, auto_index)
                .context("querying implementations")?;
            log_cli_query("implementations", &name, &qr, start);
            if json {
                let out = qr.map_results(|i| ImplDto::from(&i));
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else if qr.results.is_empty() {
                eprintln!("No implementations found for {name}");
                for n in commands::extra_miss_notes(&qr.notes) {
                    eprintln!("{n}");
                }
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
            let start = std::time::Instant::now();
            let qr = commands::run_dependencies_meta(&name, auto_index)
                .context("querying dependencies")?;
            log_cli_query("dependencies", &name, &qr, start);
            if json {
                let out = qr.map_results(|d| DependencyDto::from(&d));
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else if qr.results.is_empty() {
                eprintln!("No dependencies found for {name}");
                for n in commands::extra_miss_notes(&qr.notes) {
                    eprintln!("{n}");
                }
            } else {
                for d in qr.results {
                    match &d.file {
                        Some(file) => println!("{}\t{}", d.module_path, file.display()),
                        None => println!("{}\texternal", d.module_path),
                    }
                }
            }
        }
        Commands::Impact { name } => {
            let start = std::time::Instant::now();
            let qr = commands::run_impact_meta(&name, auto_index).context("querying impact")?;
            log_cli_query("impact", &name, &qr, start);
            if json {
                let out = qr.map_results(|s| SymbolDto::from(&s));
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else if qr.results.is_empty() {
                eprintln!("No impact found for {name}");
                for n in commands::extra_miss_notes(&qr.notes) {
                    eprintln!("{n}");
                }
            } else {
                for s in qr.results {
                    println!("{}", commands::format_symbol_hit(&s));
                }
            }
        }
        Commands::Serve { port } => {
            commands::run_serve(port, auto_index).context("serving JSON API")?;
        }
        Commands::Insights { port } => {
            commands::run_insights(port, auto_index, json)
                .context("opening Insights dashboard")?;
        }
        Commands::Mcp => {
            commands::run_mcp(auto_index).context("serving MCP")?;
        }
    }
    Ok(())
}
