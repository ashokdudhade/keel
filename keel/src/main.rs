//! `keel` binary entry point. Uses `anyhow` for context-rich top-level errors.

use anyhow::{Context, Result};
use clap::Parser;
use keel::api::{DependencyDto, ImplDto, ReferenceDto, SymbolDto};
use keel::cli::{commands, Cli, Commands, KnowledgeCommands, WikiCommands};
use std::path::Path;

/// Record one CLI query for the Insights portal (best-effort, local only).
fn log_cli_query<T>(
    tool: &str,
    target: &str,
    module: Option<&str>,
    qr: &keel::QueryResult<T>,
    start: std::time::Instant,
) {
    let summary = keel::usage::QuerySummary::from_query_result(qr);
    keel::usage::log_query_at(
        &keel::usage::log_path_for_db(&commands::db_path()),
        keel::usage::Surface::Cli,
        tool,
        target,
        module,
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
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "indexed": stats.indexed,
                        "skipped": stats.skipped,
                        "removed": stats.removed,
                        "errors": stats.errors,
                        "syntax_errors": stats.syntax_errors,
                    })
                );
            } else {
                commands::print_index_stats(&stats);
            }
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
            if json {
                println!(
                    "{}",
                    commands::status_json(Path::new(".")).context("daemon status")?
                );
            } else {
                commands::run_status().context("daemon status")?;
            }
        }
        Commands::DaemonStop => {
            commands::run_daemon_stop().context("stopping keel daemon")?;
        }
        Commands::Doctor { path } => {
            if json {
                println!(
                    "{}",
                    commands::doctor_checks_json(&path)
                        .with_context(|| format!("diagnosing {}", path.display()))?
                );
            } else {
                commands::run_doctor(&path)
                    .with_context(|| format!("diagnosing {}", path.display()))?;
            }
        }
        Commands::Init { path } => {
            commands::run_init(&path)
                .with_context(|| format!("initializing {}", path.display()))?;
        }
        Commands::Definition {
            name,
            module,
            limit,
            preview,
        } => {
            let start = std::time::Instant::now();
            let qr = commands::run_definition_meta(&name, module.as_deref(), auto_index)
                .context("querying definition")?
                .truncated(keel::graph::query_result::resolve_limit(limit));
            log_cli_query("definition", &name, module.as_deref(), &qr, start);
            let mut previews = commands::preview_cache(preview);
            if json {
                let out = match previews.as_mut() {
                    Some(cache) => qr.map_results(|s| {
                        let mut d = SymbolDto::from(&s);
                        d.preview = cache.line(&s.file, s.start_line);
                        d
                    }),
                    None => qr.map_results(|s| SymbolDto::from(&s)),
                };
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else if qr.results.is_empty() {
                eprintln!("No definition found for {name}");
                for n in commands::extra_miss_notes(&qr.notes) {
                    eprintln!("{n}");
                }
            } else {
                for s in qr.results {
                    commands::print_symbol_hit(&s, &mut previews);
                }
                for n in &qr.notes {
                    eprintln!("{n}");
                }
            }
        }
        Commands::References {
            name,
            module,
            limit,
            preview,
        } => {
            let start = std::time::Instant::now();
            let qr = commands::run_references_meta(&name, module.as_deref(), limit, auto_index)
                .context("querying references")?;
            log_cli_query("references", &name, module.as_deref(), &qr, start);
            let mut previews = commands::preview_cache(preview);
            if json {
                let out = match previews.as_mut() {
                    Some(cache) => qr.map_results(|r| {
                        let mut d = ReferenceDto::from(&r);
                        d.preview = cache.line(&r.file, r.start_line);
                        d
                    }),
                    None => qr.map_results(|r| ReferenceDto::from(&r)),
                };
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else if qr.results.is_empty() {
                eprintln!("No references found for {name}");
                for n in commands::extra_miss_notes(&qr.notes) {
                    eprintln!("{n}");
                }
            } else {
                for r in qr.results {
                    commands::print_reference_hit(&r, &mut previews);
                }
                for n in &qr.notes {
                    eprintln!("{n}");
                }
            }
        }
        Commands::Callers {
            name,
            module,
            limit,
            preview,
        } => {
            let start = std::time::Instant::now();
            let qr = commands::run_callers_meta(&name, module.as_deref(), limit, auto_index)
                .context("querying callers")?;
            log_cli_query("callers", &name, module.as_deref(), &qr, start);
            let mut previews = commands::preview_cache(preview);
            if json {
                let out = match previews.as_mut() {
                    Some(cache) => qr.map_results(|r| {
                        let mut d = ReferenceDto::from(&r);
                        d.preview = cache.line(&r.file, r.start_line);
                        d
                    }),
                    None => qr.map_results(|r| ReferenceDto::from(&r)),
                };
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else if qr.results.is_empty() {
                eprintln!("No callers found for {name}");
                for n in commands::extra_miss_notes(&qr.notes) {
                    eprintln!("{n}");
                }
            } else {
                for r in qr.results {
                    commands::print_reference_hit(&r, &mut previews);
                }
                for n in &qr.notes {
                    eprintln!("{n}");
                }
            }
        }
        Commands::Implementations {
            name,
            module,
            limit,
            preview,
        } => {
            let start = std::time::Instant::now();
            let qr = commands::run_implementations_meta(&name, module.as_deref(), auto_index)
                .context("querying implementations")?
                .truncated(keel::graph::query_result::resolve_limit(limit));
            log_cli_query("implementations", &name, module.as_deref(), &qr, start);
            let mut previews = commands::preview_cache(preview);
            if json {
                let out = match previews.as_mut() {
                    Some(cache) => qr.map_results(|i| {
                        let mut d = ImplDto::from(&i);
                        d.preview = cache.line(&i.file, i.start_line);
                        d
                    }),
                    None => qr.map_results(|i| ImplDto::from(&i)),
                };
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else if qr.results.is_empty() {
                eprintln!("No implementations found for {name}");
                for n in commands::extra_miss_notes(&qr.notes) {
                    eprintln!("{n}");
                }
            } else {
                for i in qr.results {
                    commands::print_impl_hit(&i, &mut previews);
                }
                for n in &qr.notes {
                    eprintln!("{n}");
                }
            }
        }
        Commands::Dependencies { name, limit } => {
            let start = std::time::Instant::now();
            let qr = commands::run_dependencies_meta(&name, auto_index)
                .context("querying dependencies")?
                .truncated(keel::graph::query_result::resolve_limit(limit));
            log_cli_query("dependencies", &name, None, &qr, start);
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
                for n in &qr.notes {
                    eprintln!("{n}");
                }
            }
        }
        Commands::Dependents { name, limit } => {
            let start = std::time::Instant::now();
            let qr = commands::run_dependents_meta(&name, auto_index)
                .context("querying dependents")?
                .truncated(keel::graph::query_result::resolve_limit(limit));
            log_cli_query("dependents", &name, None, &qr, start);
            if json {
                let out = qr.map_results(|d| DependencyDto::from(&d));
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else if qr.results.is_empty() {
                eprintln!("No dependents found for {name}");
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
                for n in &qr.notes {
                    eprintln!("{n}");
                }
            }
        }
        Commands::Impact {
            name,
            module,
            limit,
            preview,
        } => {
            let start = std::time::Instant::now();
            let qr = commands::run_impact_meta(&name, module.as_deref(), limit, auto_index)
                .context("querying impact")?;
            log_cli_query("impact", &name, module.as_deref(), &qr, start);
            let mut previews = commands::preview_cache(preview);
            if json {
                let out = match previews.as_mut() {
                    Some(cache) => qr.map_results(|s| {
                        let mut d = SymbolDto::from(&s);
                        d.preview = cache.line(&s.file, s.start_line);
                        d
                    }),
                    None => qr.map_results(|s| SymbolDto::from(&s)),
                };
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else if qr.results.is_empty() {
                if commands::has_file_scope_note(&qr.notes) {
                    eprintln!("No impacted symbols for {name}");
                } else {
                    eprintln!("No impact found for {name}");
                }
                for n in commands::extra_miss_notes(&qr.notes) {
                    eprintln!("{n}");
                }
            } else {
                for s in qr.results {
                    commands::print_symbol_hit(&s, &mut previews);
                }
                for n in &qr.notes {
                    eprintln!("{n}");
                }
            }
        }
        Commands::Search {
            pattern,
            limit,
            preview,
        } => {
            let start = std::time::Instant::now();
            let qr =
                commands::run_search_meta(&pattern, limit, auto_index).context("searching")?;
            log_cli_query("search", &pattern, None, &qr, start);
            let mut previews = commands::preview_cache(preview);
            if json {
                let out = match previews.as_mut() {
                    Some(cache) => qr.map_results(|s| {
                        let mut d = SymbolDto::from(&s);
                        d.preview = cache.line(&s.file, s.start_line);
                        d
                    }),
                    None => qr.map_results(|s| SymbolDto::from(&s)),
                };
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else if qr.results.is_empty() {
                eprintln!("No symbols matching `{pattern}`");
                for n in commands::extra_miss_notes(&qr.notes) {
                    eprintln!("{n}");
                }
            } else {
                for s in qr.results {
                    commands::print_symbol_hit(&s, &mut previews);
                }
                for n in &qr.notes {
                    eprintln!("{n}");
                }
            }
        }
        Commands::Outline {
            path,
            limit,
            preview,
        } => {
            let start = std::time::Instant::now();
            let file = path.to_string_lossy().into_owned();
            let qr = commands::run_outline_meta(&file, auto_index)
                .context("querying outline")?
                .truncated(keel::graph::query_result::resolve_limit(limit));
            log_cli_query("outline", &file, None, &qr, start);
            let mut previews = commands::preview_cache(preview);
            if json {
                let out = match previews.as_mut() {
                    Some(cache) => qr.map_results(|s| {
                        let mut d = SymbolDto::from(&s);
                        d.preview = cache.line(&s.file, s.start_line);
                        d
                    }),
                    None => qr.map_results(|s| SymbolDto::from(&s)),
                };
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else if qr.results.is_empty() {
                eprintln!("No symbols found in {file}");
                for n in commands::extra_miss_notes(&qr.notes) {
                    eprintln!("{n}");
                }
            } else {
                for s in qr.results {
                    commands::print_symbol_hit(&s, &mut previews);
                }
                for n in &qr.notes {
                    eprintln!("{n}");
                }
            }
        }
        Commands::Unused {
            path,
            limit,
            preview,
            transitive,
        } => {
            let start = std::time::Instant::now();
            let target = path.as_ref().map(|p| p.to_string_lossy().into_owned());
            let scope = target.as_deref().unwrap_or("project");
            let qr = commands::run_unused_meta_opts(target.as_deref(), transitive, auto_index)
                .context("querying unused")?
                .truncated(keel::graph::query_result::resolve_limit(limit));
            log_cli_query("unused", scope, None, &qr, start);
            let mut previews = commands::preview_cache(preview);
            if json {
                let out = match previews.as_mut() {
                    Some(cache) => qr.map_results(|s| {
                        let mut d = SymbolDto::from(&s);
                        d.preview = cache.line(&s.file, s.start_line);
                        d
                    }),
                    None => qr.map_results(|s| SymbolDto::from(&s)),
                };
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else if qr.results.is_empty() {
                eprintln!("No unreferenced functions found in {scope}");
                for n in commands::extra_miss_notes(&qr.notes) {
                    eprintln!("{n}");
                }
            } else {
                for s in qr.results {
                    commands::print_symbol_hit(&s, &mut previews);
                }
                for n in &qr.notes {
                    eprintln!("{n}");
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
        Commands::InsightsStop => {
            commands::run_insights_stop().context("stopping Insights server")?;
        }
        Commands::Mcp => {
            commands::run_mcp(auto_index).context("serving MCP")?;
        }
        Commands::Explain { name, file, module } => {
            // Exactly one target: NAME (+ optional --module filter), or
            // --file, or --module alone.
            let explanation = match (name, file, module) {
                (Some(_), Some(_), _) | (None, Some(_), Some(_)) => {
                    anyhow::bail!("pass exactly one of NAME, --file, or --module")
                }
                (None, None, None) => {
                    anyhow::bail!("pass exactly one of NAME, --file, or --module")
                }
                (Some(name), None, module) => {
                    commands::build_explanation_symbol(&name, module.as_deref(), auto_index)
                        .with_context(|| format!("explaining {name}"))?
                }
                (None, Some(file), None) => commands::build_explanation_file(&file, auto_index)
                    .with_context(|| format!("explaining {}", file.display()))?,
                (None, None, Some(module)) => {
                    commands::build_explanation_module(&module, auto_index)
                        .with_context(|| format!("explaining module {module}"))?
                }
            };
            if json {
                println!(
                    "{}",
                    commands::explanation_json(&explanation).context("rendering explanation")?
                );
            } else {
                commands::print_explanation(&explanation);
            }
        }
        Commands::Wiki { action } => match action {
            WikiCommands::Generate { dir } => {
                if json {
                    println!(
                        "{}",
                        commands::wiki_report_json(dir, auto_index, false)
                            .context("generating wiki")?
                    );
                } else {
                    commands::run_wiki_generate(dir, auto_index).context("generating wiki")?;
                }
            }
            WikiCommands::Update { dir } => {
                if json {
                    println!(
                        "{}",
                        commands::wiki_report_json(dir, auto_index, true)
                            .context("updating wiki")?
                    );
                } else {
                    commands::run_wiki_update(dir, auto_index).context("updating wiki")?;
                }
            }
            WikiCommands::Status { dir } => {
                if json {
                    println!(
                        "{}",
                        commands::wiki_status_json(dir, auto_index)
                            .context("checking wiki status")?
                    );
                } else {
                    commands::run_wiki_status(dir, auto_index).context("checking wiki status")?;
                }
            }
        },
        Commands::Context {
            task,
            budget,
            no_tests,
            no_knowledge,
        } => {
            if json {
                println!(
                    "{}",
                    commands::context_json(&task, budget, !no_tests, !no_knowledge, auto_index)
                        .context("building task context")?
                );
            } else {
                commands::run_context(&task, budget, !no_tests, !no_knowledge, auto_index)
                    .context("building task context")?;
            }
        }
        Commands::Knowledge { action } => match action {
            KnowledgeCommands::Status => {
                if json {
                    println!(
                        "{}",
                        commands::knowledge_status_json(auto_index)
                            .context("checking knowledge freshness")?
                    );
                } else {
                    commands::run_knowledge_status(auto_index)
                        .context("checking knowledge freshness")?;
                }
            }
            KnowledgeCommands::Validate { id } => {
                if json {
                    println!(
                        "{}",
                        commands::knowledge_validate_json(&id, auto_index)
                            .with_context(|| format!("validating knowledge {id}"))?
                    );
                } else {
                    commands::run_knowledge_validate(&id, auto_index)
                        .with_context(|| format!("validating knowledge {id}"))?;
                }
            }
        },
        Commands::Drift { path } => {
            // KnowledgeCI gate: findings exit 1 so CI fails. The report
            // prints first in both modes; JSON stays machine-readable.
            if json {
                let report =
                    commands::build_drift_report(&path, auto_index).context("checking drift")?;
                println!("{}", serde_json::to_string_pretty(&report)?);
                if !report.is_clean() {
                    std::process::exit(1);
                }
            } else {
                let report = commands::run_drift(&path, auto_index).context("checking drift")?;
                if !report.is_clean() {
                    std::process::exit(1);
                }
            }
        }
    }
    Ok(())
}
