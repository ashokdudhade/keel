//! Core error type for the SecondBrain library.

use std::path::PathBuf;
use thiserror::Error;

/// All errors produced by the SecondBrain library.
#[derive(Debug, Error)]
pub enum SecondBrainError {
    #[error("I/O error for {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("database error")]
    Database(#[from] rusqlite::Error),

    #[error("failed to parse source code")]
    Parse,

    #[error("tree-sitter error: {0}")]
    TreeSitter(String),

    #[error("no language plugin registered for extension {0:?}")]
    UnsupportedExtension(String),
}

/// Convenience `Result` alias used throughout the library.
pub type Result<T> = std::result::Result<T, SecondBrainError>;
