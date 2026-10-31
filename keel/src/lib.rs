//! Keel: deterministic, local-first code intelligence engine.
//!
//! The stable 1.0 library surface is [`Index`] (re-exported at the crate root).
//! Module paths under `db`, `graph`, `index`, and `languages` remain available
//! for the CLI and advanced integrations, but are not the preferred consumer API.

pub mod adr;
pub mod api;
pub mod cli;
pub mod context;
pub mod daemon;
pub mod db;
pub mod drift;
pub mod explain;
pub mod wiki;
pub mod error;
pub mod facade;
pub mod graph;
pub mod index;
pub mod knowledge;
pub mod languages;
pub mod mcp;
pub mod usage;

pub use error::{Result, KeelError};
pub use facade::Index;
pub use graph::deps::Dependency;
pub use graph::query_result::{Confidence, QueryResult, ResolutionTier};
pub use graph::types::{ImplRecord, Reference, Symbol, SymbolKind};
pub use index::{index_repository, index_repository_with, IndexStats};
pub use context::{ContextItem, ContextOptions, ContextPackage};
pub use explain::{
    DisambiguationCandidate, ExplainedFile, ExplainedModule, Explanation, ExplainOptions,
    LinkedKnowledge,
};
pub use wiki::{PageState, PageStatus, WikiReport, WikiStatusReport};
pub use drift::{DriftFinding, DriftReport, DriftVerdict};
pub use adr::{AdrDeclared, AdrDetail, AdrLink, AdrList, AdrSummary, AdrVerdict, HistoryEntry};
pub use knowledge::{
    InvalidationReport, KnowledgeEvidence, KnowledgeOrigin, KnowledgeRecord, KnowledgeStatus,
    KnowledgeType, MarkedStale, RecordFreshness, ValidationOutcome,
};
pub use languages::{LanguagePlugin, Registry};
