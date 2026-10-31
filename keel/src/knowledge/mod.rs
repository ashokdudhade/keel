//! Source-linked knowledge layer: durable architectural facts with provenance.
//!
//! The knowledge store persists claims about a repository (facts, decisions,
//! explanations, patterns, constraints) together with the source evidence that
//! backs them. Every record carries an [`KnowledgeOrigin`] (who claimed it)
//! and a [`KnowledgeStatus`] (how much the current code still supports it), so
//! agents can distinguish verified structure from stale or inferred guesses.
//!
//! Provenance is enforced, not advisory: human-authored records reject
//! non-human overwrites (see [`KeelError::KnowledgeProvenance`]), and status
//! transitions are explicit — a source citation alone never upgrades a claim
//! to supported.
//!
//! [`KeelError::KnowledgeProvenance`]: crate::error::KeelError::KnowledgeProvenance

pub mod freshness;
pub mod model;
pub mod store;

pub use freshness::{
    check_freshness, check_one, invalidate_stale, validate_record, InvalidationReport, MarkedStale,
    RecordFreshness, ValidationOutcome,
};
pub use model::{
    KnowledgeEvidence, KnowledgeOrigin, KnowledgeRecord, KnowledgeStatus, KnowledgeType,
};
pub use store::{
    get_record, list_records, put_record, records_linked, search_records, DEFAULT_SEARCH_LIMIT,
    MAX_SEARCH_LIMIT,
};
