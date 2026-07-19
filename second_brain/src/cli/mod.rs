//! Command-line interface definitions.

pub mod commands;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Top-level CLI parser for the `sb` binary.
#[derive(Parser)]
#[command(name = "sb", about = "SecondBrain: deterministic code intelligence")]
pub struct Cli {
    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Commands,
}

/// Available subcommands.
#[derive(Subcommand)]
pub enum Commands {
    /// Index a repository at PATH into `.secondbrain/index.db`.
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
    /// Watch a repository and re-index on `.rs` file changes.
    Watch {
        /// Path to the repository to watch.
        path: PathBuf,
    },
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
}
