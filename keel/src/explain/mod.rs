//! Evidence-based explanations: deterministic packages describing a symbol,
//! file, or module.
//!
//! [`Explanation`] gathers what Keel knows — definition, excerpt, callers,
//! dependencies, dependents, implementations, linked knowledge — with source
//! pins (`source_hashes`) so consumers can cache the package and detect drift
//! by comparing hashes. `generated` is always `false` from the core engine:
//! an LLM layer may render prose from the package, but must set `generated`
//! with its model id rather than pass assembly off as authorship. An
//! explanation is never automatically a verified fact.

pub mod build;

pub use build::{explain_file, explain_module, explain_symbol};

use crate::api::{DependencyDto, ImplDto, ReferenceDto, SymbolDto};
use crate::knowledge::KnowledgeRecord;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Cap on callers included per symbol explanation (total still reported).
pub const MAX_EXPLAIN_CALLERS: usize = 20;
/// Cap on outlined symbols per file/module explanation.
pub const MAX_EXPLAIN_SYMBOLS: usize = 100;
/// Cap on linked knowledge records per explanation.
pub const MAX_EXPLAIN_KNOWLEDGE: usize = 10;

/// Options for the `explain_*` builders.
#[derive(Debug, Clone, Default)]
pub struct ExplainOptions {
    /// Project root excerpts resolve against. `None` omits the excerpt
    /// (citations still apply); the CLI/MCP always pass a root.
    pub source_root: Option<PathBuf>,
}

/// A file summary inside a file explanation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExplainedFile {
    /// Indexed path, relative to the project root.
    pub path: String,
    /// Content hash at build time (cache pin).
    pub content_hash: String,
    /// Defined symbols, in source order (capped).
    pub symbols: Vec<SymbolDto>,
    /// Defined symbols in total (before the cap).
    pub symbols_total: usize,
}

/// A module summary inside a module explanation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExplainedModule {
    /// Module path as indexed.
    pub module_path: String,
    /// Indexed files in the module, ordered.
    pub files: Vec<String>,
    /// Defined symbols across those files, ordered by (path, line) (capped).
    pub symbols: Vec<SymbolDto>,
    /// Defined symbols in total (before the cap).
    pub symbols_total: usize,
}

/// One knowledge record linked to the explained target, with its live
/// freshness verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LinkedKnowledge {
    /// Full record: claim, provenance, status, and evidence pins.
    pub record: KnowledgeRecord,
    /// Live verdict: false means pinned evidence verifiably drifted.
    pub fresh: bool,
    /// Reasons behind a non-fresh verdict (empty when fresh).
    pub reasons: Vec<String>,
}

/// One candidate when a symbol name resolves ambiguously.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DisambiguationCandidate {
    /// Symbol name.
    pub name: String,
    /// Defining module path.
    pub module_path: String,
    /// Defining file.
    pub path: String,
    /// 1-based definition line.
    pub start_line: u32,
}

/// Deterministic evidence package for one symbol, file, or module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Explanation {
    /// Target text as given.
    pub target: String,
    /// `symbol`, `file`, or `module`.
    pub target_kind: String,
    /// Always `false` from the core engine (see module docs).
    pub generated: bool,
    /// Always `None` from the core engine; an LLM renderer identifies itself.
    pub generator: Option<String>,
    /// Resolved definition (symbol targets with exactly one hit).
    pub definition: Option<SymbolDto>,
    /// Definition-site excerpt (symbol targets with a source root).
    pub excerpt: Option<String>,
    /// File summary (file targets).
    pub file: Option<ExplainedFile>,
    /// Module summary (module targets).
    pub module: Option<ExplainedModule>,
    /// Call/use sites (symbol targets, capped).
    pub callers: Vec<ReferenceDto>,
    /// Call/use sites in total (before the cap).
    pub callers_total: usize,
    /// Modules the target depends on.
    pub dependencies: Vec<DependencyDto>,
    /// Modules depending on the target.
    pub dependents: Vec<DependencyDto>,
    /// Implementations (non-empty for traits/interfaces/bases).
    pub implementations: Vec<ImplDto>,
    /// Knowledge records citing the target (capped).
    pub linked_knowledge: Vec<LinkedKnowledge>,
    /// Content hashes of cited files at build time (cache pins), ordered.
    pub source_hashes: BTreeMap<String, String>,
    /// Candidates when a symbol name is ambiguous (sections stay empty).
    pub disambiguation: Vec<DisambiguationCandidate>,
    /// Misses, caps, and trust guidance.
    pub notes: Vec<String>,
}
