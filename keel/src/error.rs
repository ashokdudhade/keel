//! Core error type for the Keel library.

use std::path::PathBuf;
use thiserror::Error;

/// All errors produced by the Keel library.
#[derive(Debug, Error)]
pub enum KeelError {
    /// An I/O error occurred while accessing a file.
    #[error("I/O error for {path}")]
    Io {
        /// Path of the file involved in the failed operation.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// An error originating from the SQLite database layer.
    #[error("database error")]
    Database(#[from] rusqlite::Error),

    /// Source code could not be parsed.
    #[error("failed to parse source code")]
    Parse,

    /// A Tree-sitter operation failed.
    #[error("tree-sitter error: {0}")]
    TreeSitter(String),

    /// No language plugin is registered for the given file extension.
    #[error("no language plugin registered for extension {0:?}")]
    UnsupportedExtension(String),

    /// A filesystem watch operation failed.
    #[error("watch error: {0}")]
    Watch(String),

    /// The global daemon, an Insights server, or a managed background
    /// process failed (client, protocol, pidfile, or spawn/stop errors).
    #[error("daemon error: {0}")]
    Daemon(String),

    /// The JSON HTTP API server failed.
    #[error("API server error: {0}")]
    Api(String),

    /// The MCP stdio server failed.
    #[error("MCP server error: {0}")]
    Mcp(String),

    /// The on-disk schema is newer than this build understands.
    #[error(
        "database schema version {found} is newer than supported version {supported}; upgrade keel to open this index"
    )]
    UnsupportedSchema {
        /// `PRAGMA user_version` found in the database.
        found: i64,
        /// Latest schema version this build can open.
        supported: i64,
    },

    /// The index was written by an older Keel and must be rebuilt before reads.
    #[error(
        "index format version {found} is older than current version {current}; re-index the project (rm -rf .keel && keel start, or query without --no-auto-index)"
    )]
    StaleIndex {
        /// Content-format stamp found in the database (0 when unstamped).
        found: i64,
        /// Content-format version this build writes.
        current: i64,
    },
}

/// Convenience `Result` alias used throughout the library.
pub type Result<T> = std::result::Result<T, KeelError>;
