//! ADR intelligence: decisions connected to implementation and history.
//!
//! An architecture decision record is only as good as its links. For
//! every ADR file the project keeps, this module resolves the symbols
//! and files it cites (reusing drift's extractor and resolvers, so the
//! two tools never disagree), pulls the knowledge records whose
//! evidence touches the same code, and reads the linked files' git
//! history — then renders one verdict: every link live (`supported`),
//! some link dead (`stale`), or nothing checkable cited (`unlinked`).
//!
//! The verdict is about the *implementation links*, orthogonal to the
//! ADR's declared `Status:` line: an `accepted` ADR with dead links is
//! a decision whose code moved on, and that is precisely the finding.
//!
//! Git history needs a `git` binary at runtime and degrades honestly
//! without one (`not a git checkout`, `git not available`); history is
//! read-only (`git log` on pinned files) and deterministic for a given
//! checkout.

pub mod parse;

use crate::db::queries;
use crate::drift::{lookup_symbol, resolve_path, PathResolution, SweepCtx};
use crate::error::Result;
use crate::knowledge::{records_linked, KnowledgeRecord};
use rusqlite::Connection;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Maximum knowledge records attached per ADR; overflow is noted.
pub const MAX_RECORDS: usize = 10;
/// Maximum history entries per ADR (one `git log` across linked files).
pub const MAX_HISTORY: usize = 10;
/// Maximum linked files passed to `git log`; overflow is noted.
const MAX_HISTORY_FILES: usize = 100;

/// Declared `Status:` line, normalized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdrDeclared {
    Proposed,
    Accepted,
    Deprecated,
    Superseded,
    /// Any other declared value, verbatim.
    Other(String),
    /// No `Status:` line found.
    Missing,
}

impl AdrDeclared {
    /// Normalize a raw `Status:` value by its first word.
    pub fn parse(raw: Option<&str>) -> Self {
        let Some(raw) = raw else {
            return AdrDeclared::Missing;
        };
        match raw.split_whitespace().next().unwrap_or("").to_lowercase().as_str() {
            "proposed" | "proposal" | "draft" | "wip" => AdrDeclared::Proposed,
            "accepted" | "done" | "approved" => AdrDeclared::Accepted,
            "deprecated" | "rejected" | "abandoned" => AdrDeclared::Deprecated,
            "superseded" | "replaced" => AdrDeclared::Superseded,
            _ => AdrDeclared::Other(raw.to_string()),
        }
    }

    /// CLI display label.
    pub fn as_str(&self) -> &str {
        match self {
            AdrDeclared::Proposed => "proposed",
            AdrDeclared::Accepted => "accepted",
            AdrDeclared::Deprecated => "deprecated",
            AdrDeclared::Superseded => "superseded",
            AdrDeclared::Other(s) => s,
            AdrDeclared::Missing => "unknown",
        }
    }
}

/// Freshness of an ADR's implementation links.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdrVerdict {
    /// Every link resolves.
    Supported,
    /// At least one link is dead.
    Stale,
    /// No checkable claims cited.
    Unlinked,
}

impl AdrVerdict {
    /// CLI display label.
    pub fn as_str(self) -> &'static str {
        match self {
            AdrVerdict::Supported => "supported",
            AdrVerdict::Stale => "stale",
            AdrVerdict::Unlinked => "unlinked",
        }
    }
}

/// One row of `adr list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdrSummary {
    /// File stem id (`adr-003-auth`).
    pub id: String,
    /// First `#` heading, when present.
    pub title: Option<String>,
    /// Declared `Status:` line.
    pub declared: AdrDeclared,
    /// Path relative to the project root, `/`-separated.
    pub path: String,
    /// Counted links (live + dead).
    pub links: usize,
    /// Link freshness verdict.
    pub verdict: AdrVerdict,
}

/// One cited symbol or file and where (whether) it resolves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdrLink {
    /// The cited text.
    pub claim: String,
    /// 1-based line in the ADR.
    pub line: u32,
    /// `symbol` or `file`.
    pub kind: String,
    /// Resolution target (`path:line` for symbols, path for files).
    pub target: Option<String>,
    /// False when the claim resolves nowhere.
    pub live: bool,
}

/// One commit touching the ADR's linked files, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HistoryEntry {
    /// Full commit hash.
    pub hash: String,
    /// Short date (`YYYY-MM-DD`).
    pub date: String,
    /// Subject line.
    pub subject: String,
}

/// Full `adr show` detail: links, knowledge, and history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdrDetail {
    /// File stem id.
    pub id: String,
    /// First `#` heading, when present.
    pub title: Option<String>,
    /// Declared `Status:` line.
    pub declared: AdrDeclared,
    /// Declared `Date:` line, when present.
    pub date: Option<String>,
    /// Path relative to the project root, `/`-separated.
    pub path: String,
    /// Link freshness verdict.
    pub verdict: AdrVerdict,
    /// Resolved and dead links, in document order.
    pub links: Vec<AdrLink>,
    /// Knowledge records evidencing the linked code, by id (capped).
    pub records: Vec<KnowledgeRecord>,
    /// Recent commits touching linked files, newest first (capped).
    pub history: Vec<HistoryEntry>,
    /// Skips, caps, and history degradations.
    pub notes: Vec<String>,
}

/// Full `adr list` result, ordered by path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdrList {
    /// One row per ADR, ordered by path.
    pub adrs: Vec<AdrSummary>,
    /// Skips and caps.
    pub notes: Vec<String>,
}

/// List every ADR under `root` with link counts and verdicts.
pub fn list_adrs(conn: &Connection, root: &Path) -> Result<AdrList> {
    let paths = parse::discover_adrs(root);
    let mut notes = Vec::new();
    if paths.len() >= parse::MAX_ADRS {
        notes.push(format!(
            "ADR discovery capped at {}; move retired ADRs out of adr/ for the rest",
            parse::MAX_ADRS
        ));
    }
    let ctx = SweepCtx::collect(conn, root)?;
    let mut adrs = Vec::new();
    for path in &paths {
        match detail_of(conn, root, path, &ctx, false)? {
            Some(detail) => adrs.push(AdrSummary {
                id: detail.id,
                title: detail.title,
                declared: detail.declared,
                path: detail.path,
                links: detail.links.len(),
                verdict: detail.verdict,
            }),
            None => notes.push(format!(
                "unreadable; skipped: {}",
                rel_path(root, path)
            )),
        }
    }
    adrs.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(AdrList { adrs, notes })
}

/// Show one ADR by stem id, relative path, or absolute path. Returns
/// `None` (with no suggestion guessing) when nothing matches.
pub fn show_adr(
    conn: &Connection,
    root: &Path,
    id_or_path: &str,
) -> Result<Option<AdrDetail>> {
    let wanted = id_or_path.trim();
    let mut best: Option<PathBuf> = None;
    for candidate in parse::discover_adrs(root) {
        let rel = rel_path(root, &candidate);
        let stem = candidate
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if stem == wanted || rel == wanted || candidate.to_string_lossy() == wanted {
            best = Some(candidate);
            break;
        }
    }
    let Some(path) = best else {
        return Ok(None);
    };
    let ctx = SweepCtx::collect(conn, root)?;
    detail_of(conn, root, &path, &ctx, true)
}

/// Suggest ADR ids containing `query` (case-insensitive), for miss
/// recovery. Ordered by id.
pub fn suggest_adr_ids(root: &Path, query: &str) -> Vec<String> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let mut ids: Vec<String> = parse::discover_adrs(root)
        .iter()
        .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(str::to_string))
        .filter(|id| id.to_lowercase().contains(&q))
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// Build an ADR's detail, or `None` when the file can't be read. With
/// `history` set, linked files' git log is included (list skips it:
/// one subprocess per ADR would make `list` slow on big trees).
fn detail_of(
    conn: &Connection,
    root: &Path,
    path: &Path,
    ctx: &SweepCtx,
    history: bool,
) -> Result<Option<AdrDetail>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => return Ok(None),
    };
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let header = parse::parse_header(file_name, &text);
    let mut notes = Vec::new();
    let mut skipped_ambiguous = 0usize;
    let mut links = Vec::new();
    for span in crate::drift::scan::extract_spans(&text) {
        match crate::drift::scan::classify(&span.text) {
            crate::drift::scan::ClaimKind::Ignore => {}
            crate::drift::scan::ClaimKind::Symbol { name, container } => {
                if let Some(link) =
                    symbol_link(conn, &span, &name, container.as_deref(), ctx)?
                {
                    links.push(link);
                }
            }
            crate::drift::scan::ClaimKind::Path { raw } => {
                match resolve_path(conn, root, path, &raw, &ctx.repo)? {
                    PathResolution::Hit(target) => links.push(AdrLink {
                        claim: raw,
                        line: span.line,
                        kind: "file".to_string(),
                        target: Some(target),
                        live: true,
                    }),
                    PathResolution::Skip { ambiguous } => {
                        if ambiguous {
                            skipped_ambiguous += 1;
                        }
                    }
                    PathResolution::Miss => links.push(AdrLink {
                        claim: raw,
                        line: span.line,
                        kind: "file".to_string(),
                        target: None,
                        live: false,
                    }),
                }
            }
        }
    }
    if skipped_ambiguous > 0 {
        notes.push(format!(
            "{skipped_ambiguous} ambiguous claim(s) skipped; qualify with a directory"
        ));
    }
    let verdict = if links.is_empty() {
        AdrVerdict::Unlinked
    } else if links.iter().any(|l| !l.live) {
        AdrVerdict::Stale
    } else {
        AdrVerdict::Supported
    };
    let records = linked_records(conn, &links, &mut notes)?;
    let history_entries = if history {
        file_history(root, &links, &mut notes)
    } else {
        Vec::new()
    };
    Ok(Some(AdrDetail {
        id: header.id,
        title: header.title,
        declared: AdrDeclared::parse(header.status.as_deref()),
        date: header.date,
        path: rel_path(root, path),
        verdict,
        links,
        records,
        history: history_entries,
        notes,
    }))
}

/// Link one symbol claim: live with its first definition's `path:line`,
/// dead without, or skipped when the shared ladder knows the name.
fn symbol_link(
    conn: &Connection,
    span: &crate::drift::scan::ClaimSpan,
    name: &str,
    container: Option<&str>,
    ctx: &SweepCtx,
) -> Result<Option<AdrLink>> {
    let lookup = lookup_symbol(conn, name, container, ctx)?;
    if lookup.quiet_early || lookup.quiet_late {
        return Ok(None);
    }
    if let Some(want) = container {
        if !lookup.scoped.is_empty() {
            return Ok(Some(defined_link(span, &lookup.scoped)));
        }
        if crate::drift::scan::is_reserved_word(name) {
            return Ok(None);
        }
        if !lookup.bare.is_empty() {
            return Ok(Some(defined_link(span, &lookup.bare)));
        }
        // A value receiver (`db.get`) cites no attributable definition;
        // like drift, linkage skips it rather than crying dead.
        if lookup.container_defs.is_empty() && queries::name_is_referenced(conn, want)? {
            return Ok(None);
        }
        return Ok(Some(dead_link(span)));
    }
    if !lookup.bare.is_empty() {
        return Ok(Some(defined_link(span, &lookup.bare)));
    }
    Ok(Some(dead_link(span)))
}

/// A live link: first definition's `path:line` (` (+N more)` when
/// several share the name).
fn defined_link(
    span: &crate::drift::scan::ClaimSpan,
    defs: &[crate::graph::types::Symbol],
) -> AdrLink {
    let first = &defs[0];
    let mut target = format!(
        "{}:{}",
        first.file.to_string_lossy().replace('\\', "/"),
        first.start_line
    );
    if defs.len() > 1 {
        target.push_str(&format!(" (+{} more)", defs.len() - 1));
    }
    AdrLink {
        claim: span.text.clone(),
        line: span.line,
        kind: "symbol".to_string(),
        target: Some(target),
        live: true,
    }
}

/// A dead link: resolves nowhere.
fn dead_link(span: &crate::drift::scan::ClaimSpan) -> AdrLink {
    AdrLink {
        claim: span.text.clone(),
        line: span.line,
        kind: "symbol".to_string(),
        target: None,
        live: false,
    }
}

/// Knowledge records evidencing the linked code: symbol-scoped records
/// for live symbol links plus file-scoped records for live file links,
/// unioned by id and capped.
fn linked_records(
    conn: &Connection,
    links: &[AdrLink],
    notes: &mut Vec<String>,
) -> Result<Vec<KnowledgeRecord>> {
    let mut ids: Vec<String> = Vec::new();
    for link in links.iter().filter(|l| l.live) {
        let found = match link.kind.as_str() {
            "symbol" => {
                let bare = link
                    .claim
                    .trim_end_matches(')')
                    .rsplit(['.', ':'])
                    .next()
                    .unwrap_or(&link.claim);
                let bare = bare.trim_end_matches("()").trim();
                records_linked(conn, None, Some(bare))?
            }
            _ => match &link.target {
                Some(target) => records_linked(conn, Some(target), None)?,
                None => Vec::new(),
            },
        };
        for record in found {
            if !ids.contains(&record.id) {
                ids.push(record.id);
            }
        }
    }
    ids.sort();
    if ids.len() > MAX_RECORDS {
        notes.push(format!(
            "knowledge links capped at {MAX_RECORDS} ({} more)",
            ids.len() - MAX_RECORDS
        ));
        ids.truncate(MAX_RECORDS);
    }
    let mut out = Vec::new();
    for id in &ids {
        if let Some(record) = crate::knowledge::store::get_record(conn, id)? {
            out.push(record);
        }
    }
    Ok(out)
}

/// Recent commits touching the ADR's live-linked files (symbol targets
/// contribute their defining files), newest first. Degrades to notes
/// outside git checkouts or without a `git` binary; never fails.
fn file_history(root: &Path, links: &[AdrLink], notes: &mut Vec<String>) -> Vec<HistoryEntry> {
    let mut files: Vec<String> = Vec::new();
    for link in links.iter().filter(|l| l.live) {
        let file = match link.kind.as_str() {
            "symbol" => link.target.as_ref().and_then(|t| {
                // Targets read `path:line (+N more)`; history wants the path.
                let path = t.split(" (+").next().unwrap_or(t);
                path.rsplit_once(':').map(|(p, _)| p.to_string())
            }),
            _ => link.target.clone(),
        };
        if let Some(file) = file {
            if !files.contains(&file) {
                files.push(file);
            }
        }
    }
    if files.is_empty() {
        return Vec::new();
    }
    if files.len() > MAX_HISTORY_FILES {
        notes.push(format!(
            "history covers {MAX_HISTORY_FILES} of {} linked files",
            files.len()
        ));
        files.truncate(MAX_HISTORY_FILES);
    }
    let probed = Command::new("git")
        .arg("rev-parse")
        .arg("--is-inside-work-tree")
        .current_dir(root)
        .output();
    match probed {
        Ok(out) if out.status.success() => {}
        Ok(_) => {
            notes.push("history unavailable: not a git checkout".to_string());
            return Vec::new();
        }
        Err(_) => {
            notes.push("history unavailable: git not available".to_string());
            return Vec::new();
        }
    }
    let mut cmd = Command::new("git");
    cmd.arg("log")
        .arg(format!("-n{MAX_HISTORY}"))
        .arg("--format=%H%x1f%ad%x1f%s")
        .arg("--date=short")
        .arg("--")
        .args(&files)
        .current_dir(root);
    let logged = match cmd.output() {
        Ok(out) if out.status.success() => out,
        Ok(out) => {
            notes.push(format!(
                "history unavailable: git log failed ({})",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
            return Vec::new();
        }
        Err(e) => {
            notes.push(format!("history unavailable: git log failed ({e})"));
            return Vec::new();
        }
    };
    let mut entries = Vec::new();
    for line in String::from_utf8_lossy(&logged.stdout).lines() {
        let mut parts = line.splitn(3, '\x1f');
        match (parts.next(), parts.next(), parts.next()) {
            (Some(hash), Some(date), Some(subject)) if !hash.is_empty() => {
                entries.push(HistoryEntry {
                    hash: hash.to_string(),
                    date: date.to_string(),
                    subject: subject.to_string(),
                });
            }
            _ => continue,
        }
    }
    entries
}

/// Root-relative `/`-separated display path.
fn rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_status_normalizes() {
        assert_eq!(AdrDeclared::parse(None), AdrDeclared::Missing);
        assert_eq!(
            AdrDeclared::parse(Some("accepted")),
            AdrDeclared::Accepted
        );
        assert_eq!(
            AdrDeclared::parse(Some("Superseded by ADR-005")),
            AdrDeclared::Superseded
        );
        assert_eq!(AdrDeclared::parse(Some("wip")), AdrDeclared::Proposed);
        assert_eq!(
            AdrDeclared::parse(Some("kicked down the road")),
            AdrDeclared::Other("kicked down the road".to_string())
        );
    }

    #[test]
    fn verdict_labels() {
        assert_eq!(AdrVerdict::Supported.as_str(), "supported");
        assert_eq!(AdrVerdict::Stale.as_str(), "stale");
        assert_eq!(AdrVerdict::Unlinked.as_str(), "unlinked");
    }
}
