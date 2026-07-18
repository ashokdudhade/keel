//! Command-line interface definitions.

pub mod commands;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Top-level CLI parser for the `sb` binary.
#[derive(Parser)]
#[command(name = "sb", about = "SecondBrain: deterministic code intelligence")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

/// Available subcommands.
#[derive(Subcommand)]
pub enum Commands {
    /// Index a repository at PATH into `.secondbrain/index.db`.
    Index { path: PathBuf },
    /// Print definition location(s) for a symbol name.
    Definition { name: String },
    /// Print reference location(s) for a name.
    References { name: String },
    /// Print call/use sites of a function name (name-based in v0.1).
    Callers { name: String },
}
