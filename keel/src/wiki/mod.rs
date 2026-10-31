//! Living architecture wiki: deterministic, source-linked documentation.
//!
//! `generate` writes the full page tree under an output directory (default
//! `<root>/docs/keel`); `update` rewrites only pages whose inputs changed;
//! `status` reports per-page freshness without writing. Pages are structural
//! (counts, lists, dependency graphs), never narrated prose — the core engine
//! stays LLM-independent, and every claim cites `path:line` evidence.
//!
//! Human edits survive updates via fenced regions: only the text between
//! [`GEN_START`] / [`GEN_END`] markers is regenerated; anything outside is
//! preserved byte-for-byte. Pages without intact markers are never touched
//! (reported `manual`). A hidden manifest (`.keel-wiki.json`) pins each
//! page's input hash; `update` and `status` diff live inputs against it.

pub mod generate;
pub mod render;
pub mod update;

pub use generate::{collect_pages, generate_all, GeneratedPage};
pub use update::{status_report, update_stale};

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Manifest filename inside the wiki output directory.
pub const MANIFEST_FILE: &str = ".keel-wiki.json";
/// Manifest schema version.
pub const MANIFEST_VERSION: u32 = 1;
/// Marker opening the regenerated region (with page id + format version).
pub const GENEREATED_MARK: &str = "keel:generated";
/// Format version for generated regions (bumped on renderer changes).
pub const REGION_FORMAT_VERSION: u32 = 1;

/// Start marker: `<!-- keel:generated:start <page-id> v1 -->`.
pub fn gen_start(page_id: &str) -> String {
    format!("<!-- {GENEREATED_MARK}:start {page_id} v{REGION_FORMAT_VERSION} -->")
}

/// End marker: `<!-- keel:generated:end -->`.
pub fn gen_end() -> &'static str {
    "<!-- keel:generated:end -->"
}

/// One page's pinned inputs in the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedPage {
    /// Hash of the canonical inputs (files + knowledge states).
    pub inputs: String,
    /// Decimal unix seconds of the generating write.
    pub generated_at: String,
}

/// Wiki manifest: pinned inputs per page path (relative, `/`-separated).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WikiManifest {
    /// Manifest schema version.
    pub version: u32,
    /// Page path → pins, ordered for stable serialization.
    pub pages: BTreeMap<String, PinnedPage>,
}

impl WikiManifest {
    /// Empty manifest at the current version.
    pub fn empty() -> Self {
        Self {
            version: MANIFEST_VERSION,
            pages: BTreeMap::new(),
        }
    }
}

/// Freshness of one wiki page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageState {
    /// Pinned inputs match live inputs.
    Current,
    /// Live inputs differ from the pin (or no pin exists).
    Stale,
    /// Expected page file is missing.
    Missing,
    /// Page exists but its inputs are gone (e.g. directory removed).
    Orphaned,
    /// File lacks intact generated markers; never auto-touched.
    Manual,
}

impl PageState {
    /// Lowercase label matching the JSON spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            PageState::Current => "current",
            PageState::Stale => "stale",
            PageState::Missing => "missing",
            PageState::Orphaned => "orphaned",
            PageState::Manual => "manual",
        }
    }
}

/// Status of one page for `wiki status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageStatus {
    /// Page path, relative to the wiki root.
    pub path: String,
    /// Freshness state.
    pub state: PageState,
    /// Human-readable detail (what drifted, or why manual).
    pub detail: String,
}

/// Report from `wiki status` (read-only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WikiStatusReport {
    /// Per-page states, ordered by path.
    pub pages: Vec<PageStatus>,
    /// Guidance notes (e.g. "run keel wiki update").
    pub notes: Vec<String>,
}

/// Report from `wiki generate` / `wiki update` (writes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WikiReport {
    /// Pages written, ordered.
    pub written: Vec<String>,
    /// Pages skipped (already current), ordered.
    pub skipped_current: Vec<String>,
    /// Generated-only orphaned pages deleted, ordered.
    pub deleted: Vec<String>,
    /// Pages kept despite orphaning (human content), with reasons.
    pub kept: Vec<String>,
    /// Guidance notes.
    pub notes: Vec<String>,
}

impl WikiReport {
    /// Empty report shell.
    pub fn empty() -> Self {
        Self {
            written: Vec::new(),
            skipped_current: Vec::new(),
            deleted: Vec::new(),
            kept: Vec::new(),
            notes: Vec::new(),
        }
    }
}
