//! SecondBrain: deterministic, local-first code intelligence engine.
//!
//! The stable 1.0 library surface is [`Index`] (re-exported at the crate root).
//! Module paths under `db`, `graph`, `index`, and `languages` remain available
//! for the CLI and advanced integrations, but are not the preferred consumer API.

pub mod api;
pub mod cli;
pub mod db;
pub mod error;
pub mod facade;
pub mod graph;
pub mod index;
pub mod languages;
pub mod mcp;

pub use error::{Result, SecondBrainError};
pub use facade::Index;
pub use graph::deps::Dependency;
pub use graph::types::{ImplRecord, Reference, Symbol, SymbolKind};
pub use index::IndexStats;
