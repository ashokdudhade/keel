//! Core error type for the SecondBrain library.

use std::path::PathBuf;
use thiserror::Error;

/// All errors produced by the SecondBrain library.
#[derive(Debug, Error)]
pub enum SecondBrainError {
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

    /// The JSON HTTP API server failed.
    #[error("API server error: {0}")]
    Api(String),
}

/// Convenience `Result` alias used throughout the library.
pub type Result<T> = std::result::Result<T, SecondBrainError>;
