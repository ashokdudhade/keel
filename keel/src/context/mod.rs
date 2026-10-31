//! Task-specific context packages: deterministic code + knowledge retrieval.
//!
//! Given a free-form task ("Add refund support to payments"), the engine
//! extracts search terms and gathers candidates from five signals — exact
//! symbol matches, symbol-name substrings, filename/path relevance,
//! dependency proximity, linked knowledge, and related tests — then ranks,
//! cites, and budget-fits them into a [`ContextPackage`].
//!
//! Every choice here is deterministic: identical task text, options, and
//! index state produce byte-identical packages. Scores are weighted signal
//! hits; the weights are documented hypotheses to tune against benchmarks,
//! not learned truths. Token estimates use [`estimate_tokens`] (chars/4),
//! a rough but stable heuristic applied to the exact text emitted.

pub mod package;
pub mod signals;

pub use package::build_package;
pub use signals::extract_terms;

use crate::knowledge::KnowledgeRecord;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Default token budget for a context package (roadmap: `--budget 8000`).
pub const DEFAULT_TOKEN_BUDGET: usize = 8000;
/// Minimum/maximum accepted budgets; out-of-range values clamp with a note.
pub const MIN_TOKEN_BUDGET: usize = 100;
/// Maximum accepted budget; out-of-range values clamp with a note.
pub const MAX_TOKEN_BUDGET: usize = 100_000;
/// Source lines per symbol excerpt (from the definition line).
pub const SYMBOL_EXCERPT_LINES: usize = 12;
/// Source lines per file excerpt (from the top of the file).
pub const FILE_EXCERPT_LINES: usize = 20;

/// Score weights. Each is a hypothesis about signal value, kept as named
/// constants so benchmarks can tune them without touching ranking logic.
pub const SCORE_EXACT_SYMBOL: u32 = 10;
/// Whole task text names a symbol exactly (bonus on top of term hits).
pub const SCORE_WHOLE_TASK_SYMBOL: u32 = 6;
/// Supported knowledge matching a term.
pub const SCORE_KNOWLEDGE_PINNED: u32 = 6;
/// Symbol name merely containing a term.
pub const SCORE_SYMBOL_SUBSTRING: u32 = 4;
/// File path containing a term.
pub const SCORE_PATH_MATCH: u32 = 4;
/// Non-supported knowledge matching a term.
pub const SCORE_KNOWLEDGE_OTHER: u32 = 3;
/// Test file referencing a matched symbol.
pub const SCORE_TEST_REF: u32 = 3;
/// Term-matching symbol one dependency hop from an exact match.
pub const SCORE_PROXIMITY: u32 = 2;
/// Term-matching symbol living in a path-matched file.
pub const SCORE_PATH_FILE_BONUS: u32 = 2;

/// Options for [`build_package`].
#[derive(Debug, Clone)]
pub struct ContextOptions {
    /// Token budget the package must fit (clamped to
    /// [`MIN_TOKEN_BUDGET`]..=[`MAX_TOKEN_BUDGET`]).
    pub token_budget: usize,
    /// Include test files referencing matched symbols.
    pub include_tests: bool,
    /// Include linked knowledge records.
    pub include_knowledge: bool,
    /// Project root source excerpts resolve against. `None` omits excerpts
    /// (items still cite `path:line`); the CLI/MCP always pass a root.
    pub source_root: Option<PathBuf>,
}

impl Default for ContextOptions {
    fn default() -> Self {
        Self {
            token_budget: DEFAULT_TOKEN_BUDGET,
            include_tests: true,
            include_knowledge: true,
            source_root: None,
        }
    }
}

/// One ranked context item. Every variant cites its source (`path`, and
/// `start_line` for code) and explains its score in `reasons`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContextItem {
    /// A matched symbol with a definition-site excerpt.
    Symbol {
        /// Indexed file path, relative to the project root.
        path: String,
        /// Symbol name.
        name: String,
        /// Symbol kind label (`function`, `struct`, …).
        symbol_kind: String,
        /// 1-based definition line.
        start_line: u32,
        /// Source excerpt ([`SYMBOL_EXCERPT_LINES`] from the definition),
        /// absent without a source root or when unreadable.
        text: Option<String>,
        /// Sum of matched signal weights.
        score: u32,
        /// One entry per matched signal, in signal order.
        reasons: Vec<String>,
    },
    /// A path-matched file with none of its symbols matched.
    File {
        /// Indexed file path, relative to the project root.
        path: String,
        /// Leading source excerpt ([`FILE_EXCERPT_LINES`]), absent without
        /// a source root or when unreadable.
        text: Option<String>,
        /// Sum of matched signal weights.
        score: u32,
        /// One entry per matched signal, in signal order.
        reasons: Vec<String>,
    },
    /// A test reference to a matched symbol.
    Test {
        /// Indexed file path, relative to the project root.
        path: String,
        /// 1-based reference line.
        start_line: u32,
        /// The referenced source line, absent without a source root or
        /// when unreadable.
        text: Option<String>,
        /// Matched symbol referenced here.
        symbol: String,
        /// Sum of matched signal weights.
        score: u32,
        /// One entry per matched signal, in signal order.
        reasons: Vec<String>,
    },
    /// A linked knowledge record (the record itself is the content).
    Knowledge {
        /// Full record: claim, provenance, status, and evidence pins.
        record: KnowledgeRecord,
        /// Sum of matched signal weights.
        score: u32,
        /// One entry per matched signal, in signal order.
        reasons: Vec<String>,
    },
}

impl ContextItem {
    /// Sort key after score: variant order, then path, line, name/id.
    /// Total across all items, so ranking is fully deterministic.
    pub(crate) fn order_key(&self) -> (u8, &str, u32, &str) {
        match self {
            ContextItem::Symbol {
                path,
                name,
                start_line,
                ..
            } => (0, path, *start_line, name),
            ContextItem::Knowledge { record, .. } => (1, "", 0, &record.id),
            ContextItem::Test {
                path,
                start_line,
                symbol,
                ..
            } => (2, path, *start_line, symbol),
            ContextItem::File { path, .. } => (3, path, 0, ""),
        }
    }

    /// Current score.
    pub fn score(&self) -> u32 {
        match self {
            ContextItem::Symbol { score, .. }
            | ContextItem::File { score, .. }
            | ContextItem::Test { score, .. }
            | ContextItem::Knowledge { score, .. } => *score,
        }
    }

    /// Add a signal hit to the score with its reason.
    pub(crate) fn add_hit(&mut self, weight: u32, reason: String) {
        let (score, reasons) = match self {
            ContextItem::Symbol { score, reasons, .. }
            | ContextItem::File { score, reasons, .. }
            | ContextItem::Test { score, reasons, .. }
            | ContextItem::Knowledge { score, reasons, .. } => (score, reasons),
        };
        *score += weight;
        reasons.push(reason);
    }
}

/// A ranked, cited, budget-fitted answer to one task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPackage {
    /// The task text, echoed.
    pub task: String,
    /// Effective budget after clamping.
    pub token_budget: usize,
    /// Estimated tokens of the task echo plus included items
    /// ([`estimate_tokens`] over the exact emitted JSON per item).
    pub estimated_tokens: usize,
    /// Included items, ranked (score desc, then [`ContextItem`] order key).
    pub items: Vec<ContextItem>,
    /// Ranked candidates cut by the budget.
    pub omitted: usize,
    /// Budget clamps, truncations, and empty-result explanations.
    pub notes: Vec<String>,
}

/// Heuristic token estimate: `ceil(chars / 4)`.
///
/// Rough for prose, close for code; chosen for stability (identical input →
/// identical count), not accuracy. Applied to the exact text emitted so the
/// budget it enforces is the budget the consumer pays.
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimate_is_stable_and_documented() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        // Unicode counts characters, not bytes.
        assert_eq!(estimate_tokens("é".repeat(8).as_str()), 2);
    }

    #[test]
    fn ranking_order_is_total_and_documented() {
        let symbol_a = ContextItem::Symbol {
            path: "b.rs".into(),
            name: "x".into(),
            symbol_kind: "function".into(),
            start_line: 1,
            text: None,
            score: 5,
            reasons: vec![],
        };
        let symbol_b = ContextItem::Symbol {
            path: "a.rs".into(),
            name: "y".into(),
            symbol_kind: "function".into(),
            start_line: 9,
            text: None,
            score: 5,
            reasons: vec![],
        };
        let knowledge = ContextItem::Knowledge {
            record: KnowledgeRecord::new(
                "k",
                crate::knowledge::KnowledgeType::Fact,
                "T",
                "C",
                crate::knowledge::KnowledgeOrigin::Agent,
            ),
            score: 5,
            reasons: vec![],
        };
        // Same score: symbols before knowledge, paths ascending.
        let mut items = [symbol_a.clone(), knowledge.clone(), symbol_b.clone()];
        items.sort_by(|a, b| {
            b.score().cmp(&a.score()).then_with(|| a.order_key().cmp(&b.order_key()))
        });
        assert_eq!(items[0], symbol_b);
        assert_eq!(items[1], symbol_a);
        assert_eq!(items[2], knowledge);
    }
}
