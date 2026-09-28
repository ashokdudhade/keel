//! Command-line interface definitions.

pub mod commands;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Top-level CLI parser for the `keel` binary.
#[derive(Parser)]
#[command(
    name = "keel",
    about = "Keel: deterministic code intelligence",
    version
)]
pub struct Cli {
    /// Skip the automatic incremental index that runs before queries.
    #[arg(long, global = true)]
    pub no_auto_index: bool,

    /// Print query results as JSON including confidence metadata.
    #[arg(long, global = true)]
    pub json: bool,

    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Commands,
}

/// Clap value parser rejecting empty query names: `keel definition ""` is a
/// typo, not a miss, so fail with exit 2 and usage instead of printing a
/// confusing `No definition found for ` header with a trailing space.
fn non_empty_name(value: &str) -> Result<String, String> {
    if value.is_empty() {
        Err("name must not be empty".to_string())
    } else {
        Ok(value.to_string())
    }
}

/// Available subcommands.
#[derive(Subcommand)]
pub enum Commands {
    /// Index a repository at PATH into `.keel/index.db`.
    Index {
        /// Path to the repository to index.
        path: PathBuf,
    },
    /// Print definition location(s) for a symbol name.
    Definition {
        /// Symbol name to look up.
        #[arg(value_parser = non_empty_name)]
        name: String,
        /// Only consider definitions in this module (e.g. crate::mcp).
        #[arg(long)]
        module: Option<String>,
        /// Maximum matches to show (1-100000, default 500).
        #[arg(long)]
        limit: Option<usize>,
        /// Show the source line of each hit (indented continuation row).
        #[arg(long)]
        preview: bool,
    },
    /// Print reference location(s) for a name.
    References {
        /// Name to find references for.
        #[arg(value_parser = non_empty_name)]
        name: String,
        /// Only consider definitions in this module when names collide.
        #[arg(long)]
        module: Option<String>,
        /// Maximum matches to show (1-100000, default 500).
        #[arg(long)]
        limit: Option<usize>,
        /// Show the source line of each hit (indented continuation row).
        #[arg(long)]
        preview: bool,
    },
    /// Print call/use sites of a name (import-aware when the module is unique or provided).
    Callers {
        /// Function name to find call/use sites for.
        #[arg(value_parser = non_empty_name)]
        name: String,
        /// Only consider definitions in this module when names collide.
        #[arg(long)]
        module: Option<String>,
        /// Maximum matches to show (1-100000, default 500).
        #[arg(long)]
        limit: Option<usize>,
        /// Show the source line of each hit (indented continuation row).
        #[arg(long)]
        preview: bool,
    },
    /// Watch a repository and re-index on registered source file changes.
    Watch {
        /// Path to the repository to watch.
        path: PathBuf,
    },
    /// Run the global daemon (used by `brew services start keel`).
    Daemon {
        /// Control API port (default 7646).
        #[arg(long, default_value_t = crate::daemon::DEFAULT_DAEMON_PORT)]
        port: u16,
    },
    /// Register this project with the global daemon (index + watch).
    Start {
        /// Path to the repository (default: current directory).
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Unregister this project from the global daemon.
    Stop,
    /// Show global daemon, watch, and Insights server status.
    Status,
    /// Stop the global daemon (use `stop` to unregister one project).
    DaemonStop,
    /// Diagnose daemon, project registration, index, and MCP health.
    Doctor {
        /// Path to the repository (default: current directory).
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Set up this project: index now and print MCP config.
    Init {
        /// Path to the repository (default: current directory).
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Print implementations of a trait/interface/base class.
    Implementations {
        /// Trait, interface, or base-class name to find implementations for.
        #[arg(value_parser = non_empty_name)]
        name: String,
        /// Only consider the trait in this module when names collide.
        #[arg(long)]
        module: Option<String>,
        /// Maximum matches to show (1-100000, default 500).
        #[arg(long)]
        limit: Option<usize>,
        /// Show the source line of each hit (JSON: `preview` field).
        #[arg(long)]
        preview: bool,
    },
    /// Print modules/files that a module or symbol depends on.
    Dependencies {
        /// Module path, directory, file path, symbol, or qualified symbol
        /// (e.g. crate::mcp::serve) to analyze. Parent modules and
        /// directories cover their whole subtree.
        #[arg(value_parser = non_empty_name)]
        name: String,
        /// Maximum matches to show (1-100000, default 500).
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Print modules that depend on a module, file, or symbol.
    Dependents {
        /// Module path, directory, file path, symbol, or qualified symbol
        /// to analyze. Parent modules and directories cover their whole
        /// subtree.
        #[arg(value_parser = non_empty_name)]
        name: String,
        /// Maximum matches to show (1-100000, default 500).
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Print symbols transitively impacted by changing a name.
    Impact {
        /// Symbol name to analyze impact for.
        #[arg(value_parser = non_empty_name)]
        name: String,
        /// Only consider definitions in this module when names collide.
        #[arg(long)]
        module: Option<String>,
        /// Maximum matches to show (1-100000, default 500).
        #[arg(long)]
        limit: Option<usize>,
        /// Show the source line of each hit (indented continuation row).
        #[arg(long)]
        preview: bool,
    },
    /// Print symbols defined in a file, module, or directory, in source order.
    Outline {
        /// File path (as indexed, e.g. src/auth.ts), module path, or directory.
        path: PathBuf,
        /// Maximum matches to show (1-100000, default 500).
        #[arg(long)]
        limit: Option<usize>,
        /// Show the source line of each hit (indented continuation row).
        #[arg(long)]
        preview: bool,
    },
    /// Print functions with no recorded references (candidate dead code).
    Unused {
        /// File path, module path, or directory to sweep (default: project).
        path: Option<PathBuf>,
        /// Maximum matches to show (1-100000, default 500).
        #[arg(long)]
        limit: Option<usize>,
        /// Show the source line of each hit (indented continuation row).
        #[arg(long)]
        preview: bool,
        /// Also flag functions referenced only from other candidates.
        #[arg(long)]
        transitive: bool,
    },
    /// Search symbol names by substring (case-insensitive).
    Search {
        /// Substring to match against symbol names.
        #[arg(value_parser = non_empty_name)]
        pattern: String,
        /// Maximum matches to show (1-200, default 50).
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Show the source line of each hit (indented continuation row).
        #[arg(long)]
        preview: bool,
    },
    /// Serve the JSON HTTP API (`/symbol`, `/outline`, `/search`, `/impact`,
    /// `/dependents`, `/insights`, `/health`).
    Serve {
        /// TCP port to listen on (default 7645).
        #[arg(long, default_value_t = 7645)]
        port: u16,
    },
    /// Stop this project's background Insights server.
    InsightsStop,
    /// Open the Insights dashboard (starts a background server if needed).
    #[command(visible_alias = "insight")]
    Insights {
        /// Preferred TCP port; a free port is picked when busy (0 = any free port).
        #[arg(long, default_value_t = 7645)]
        port: u16,
    },
    /// Serve the MCP stdio server (NDJSON or Content-Length JSON-RPC).
    Mcp,
}
