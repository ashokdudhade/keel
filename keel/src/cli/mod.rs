//! Command-line interface definitions.

pub mod commands;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Top-level CLI parser for the `keel` binary.
#[derive(Parser)]
#[command(name = "keel", about = "Keel: deterministic code intelligence")]
pub struct Cli {
    /// Skip the automatic incremental index that runs before queries.
    #[arg(long, global = true)]
    pub no_auto_index: bool,

    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Commands,
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
        name: String,
    },
    /// Print reference location(s) for a name.
    References {
        /// Name to find references for.
        name: String,
    },
    /// Print call/use sites of a function name (name-based in v0.1).
    Callers {
        /// Function name to find call/use sites for.
        name: String,
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
    /// Show global daemon and this project's watch status.
    Status,
    /// Print implementations of a trait.
    Implementations {
        /// Trait name to find implementations for.
        name: String,
    },
    /// Print modules/files that a module or symbol depends on.
    Dependencies {
        /// Module path or symbol name to analyze.
        name: String,
    },
    /// Print symbols transitively impacted by changing a name.
    Impact {
        /// Symbol name to analyze impact for.
        name: String,
    },
    /// Serve the JSON HTTP API (`GET /symbol/{name}`, `GET /health`).
    Serve {
        /// TCP port to listen on (default 7645).
        #[arg(long, default_value_t = 7645)]
        port: u16,
    },
    /// Serve the MCP stdio server (NDJSON or Content-Length JSON-RPC).
    Mcp,
}
