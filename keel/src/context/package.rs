//! Package assembly: rank, excerpt, and budget-fit candidates.
//!
//! Ranking is total (score desc, then the item order key), so output order is
//! stable. Costs are [`estimate_tokens`] over the exact JSON emitted per
//! item, and the greedy fill stops before exceeding the budget — the package
//! never silently overruns.

use crate::context::signals::{extract_terms, gather_candidates};
use crate::context::{
    estimate_tokens, ContextItem, ContextOptions, ContextPackage, FILE_EXCERPT_LINES,
    MAX_TOKEN_BUDGET, MIN_TOKEN_BUDGET, SYMBOL_EXCERPT_LINES,
};
use crate::db::queries;
use crate::error::Result;
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Ranked, cited, budget-fitted context for `task`.
///
/// Empty tasks yield an empty package with an explanatory note (never an
/// error); boundary layers (CLI/MCP) reject empties loudly before this.
pub fn build_package(
    conn: &Connection,
    task: &str,
    options: &ContextOptions,
) -> Result<ContextPackage> {
    let mut notes = Vec::new();
    let budget = options.token_budget.clamp(MIN_TOKEN_BUDGET, MAX_TOKEN_BUDGET);
    if budget != options.token_budget {
        notes.push(format!(
            "Token budget clamped to {budget} (accepted {MIN_TOKEN_BUDGET}..={MAX_TOKEN_BUDGET})."
        ));
    }
    if task.trim().is_empty() {
        notes.push("Empty task: no terms to retrieve.".to_string());
        return Ok(ContextPackage {
            task: task.to_string(),
            token_budget: budget,
            estimated_tokens: estimate_tokens(task),
            items: Vec::new(),
            omitted: 0,
            notes,
        });
    }

    let mut gathered = gather_candidates(conn, task, options)?;
    notes.append(&mut gathered.notes);
    if queries::is_index_empty(conn).unwrap_or(false) {
        notes.push(
            "Index holds no files; only knowledge records considered. Run `keel index` or `keel start`."
                .to_string(),
        );
    }
    let mut candidates = gathered.candidates;

    // Files whose symbols are cited separately add no information: drop the
    // File candidate when any Symbol candidate shares its path.
    let symbol_paths: std::collections::BTreeSet<String> = candidates
        .iter()
        .filter_map(|c| match c {
            ContextItem::Symbol { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect();
    candidates.retain(|c| match c {
        ContextItem::File { path, .. } => !symbol_paths.contains(path),
        _ => true,
    });

    // Total order: score desc, then the item order key.
    candidates
        .sort_by(|a, b| b.score().cmp(&a.score()).then_with(|| a.order_key().cmp(&b.order_key())));

    if candidates.is_empty() {
        notes.push(format!(
            "No code or knowledge matched the task terms: {}.",
            extract_terms(task).join(", ")
        ));
        return Ok(ContextPackage {
            task: task.to_string(),
            token_budget: budget,
            estimated_tokens: estimate_tokens(task),
            items: Vec::new(),
            omitted: 0,
            notes,
        });
    }

    // Excerpts resolve against the source root (cached per file).
    let mut reader = SourceReader::new(options.source_root.clone());
    for candidate in &mut candidates {
        attach_excerpt(&mut reader, candidate);
    }
    if options.source_root.is_none() {
        notes.push("No source root: excerpts omitted (citations still apply).".to_string());
    } else if reader.missing > 0 {
        notes.push(format!(
            "{} item(s) lack excerpts (source unreadable).",
            reader.missing
        ));
    }

    // Greedy fill in rank order; costs are over the exact emitted JSON.
    let total = candidates.len();
    let mut items = Vec::new();
    let mut tokens = estimate_tokens(task);
    for candidate in candidates {
        let cost = estimate_tokens(
            &serde_json::to_string(&candidate).unwrap_or_default(),
        );
        if tokens + cost > budget && !items.is_empty() {
            continue;
        }
        // The top hit is always included (with a note when it alone
        // overruns): an empty package helps nobody.
        if tokens + cost > budget {
            notes.push(format!(
                "Top item exceeds the {budget}-token budget alone; included anyway ({} estimated).",
                tokens + cost
            ));
        }
        tokens += cost;
        items.push(candidate);
    }
    // `omitted` counts ranked candidates cut by the budget.
    let omitted = total - items.len();
    Ok(ContextPackage {
        task: task.to_string(),
        token_budget: budget,
        estimated_tokens: tokens,
        items,
        omitted,
        notes,
    })
}

/// Cached source-file reader resolving indexed paths against a root.
pub(crate) struct SourceReader {
    root: Option<PathBuf>,
    cache: HashMap<PathBuf, Option<Vec<String>>>,
    missing: usize,
}

impl SourceReader {
    pub(crate) fn new(root: Option<PathBuf>) -> Self {
        Self {
            root,
            cache: HashMap::new(),
            missing: 0,
        }
    }

    /// File lines, cached; `None` without a root or when unreadable.
    pub(crate) fn lines(&mut self, path: &str) -> Option<&Vec<String>> {
        let root = self.root.clone()?;
        if !self.cache.contains_key(Path::new(path)) {
            let lines = std::fs::read_to_string(root.join(path))
                .ok()
                .map(|text| text.lines().map(str::to_string).collect());
            self.cache.insert(PathBuf::from(path), lines);
        }
        self.cache
            .get(Path::new(path))
            .and_then(|entry| entry.as_ref())
    }
}

/// Attach the kind-appropriate excerpt (or count a miss).
fn attach_excerpt(reader: &mut SourceReader, item: &mut ContextItem) {
    let excerpt = match item {
        ContextItem::Symbol {
            path, start_line, ..
        } => reader
            .lines(path)
            .and_then(|lines| excerpt_from(*start_line, lines, SYMBOL_EXCERPT_LINES)),
        ContextItem::File { path, .. } => reader
            .lines(path)
            .map(|lines| {
                lines
                    .iter()
                    .take(FILE_EXCERPT_LINES)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .filter(|s| !s.is_empty()),
        ContextItem::Test {
            path, start_line, ..
        } => reader
            .lines(path)
            .and_then(|lines| excerpt_from(*start_line, lines, 1)),
        // The record itself is the content; nothing to excerpt.
        ContextItem::Knowledge { .. } => return,
    };
    if excerpt.is_none() {
        reader.missing += 1;
    }
    match item {
        ContextItem::Symbol { text, .. }
        | ContextItem::File { text, .. }
        | ContextItem::Test { text, .. } => *text = excerpt,
        ContextItem::Knowledge { .. } => {}
    }
}

/// Up to `max` lines from 1-based `start` (clamped to the file end).
pub(crate) fn excerpt_from(start: u32, lines: &[String], max: usize) -> Option<String> {
    let from = start.checked_sub(1)? as usize;
    if from >= lines.len() {
        return None;
    }
    let excerpt: Vec<&str> = lines[from..].iter().take(max).map(String::as_str).collect();
    if excerpt.is_empty() {
        return None;
    }
    Some(excerpt.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;
    use crate::index;
    use crate::knowledge::{
        KnowledgeOrigin, KnowledgeRecord, KnowledgeStatus, KnowledgeType,
    };
    use std::fs;
    use tempfile::tempdir;

    /// Same shape as the signals fixture: `src/payments.py` (charge, refund),
    /// `tests/test_payments.py` (references refund), `src/auth.py` (login).
    fn fixture() -> (tempfile::TempDir, Connection) {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("tests")).unwrap();
        fs::write(
            root.join("src/payments.py"),
            "def charge(amount):\n    return amount\n\ndef refund(amount):\n    return -amount\n",
        )
        .unwrap();
        fs::write(
            root.join("tests/test_payments.py"),
            "from src.payments import refund\n\ndef test_refund():\n    assert refund(5) == -5\n",
        )
        .unwrap();
        fs::write(root.join("src/auth.py"), "def login(user):\n    return user\n").unwrap();
        let mut conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        index::index_repository(root, &mut conn).unwrap();
        (dir, conn)
    }

    fn rooted(dir: &tempfile::TempDir) -> ContextOptions {
        ContextOptions {
            source_root: Some(dir.path().to_path_buf()),
            ..ContextOptions::default()
        }
    }

    #[test]
    fn package_cites_ranked_symbols_with_excerpts() {
        let (dir, conn) = fixture();
        let package = build_package(&conn, "Add refund support to payments", &rooted(&dir)).unwrap();
        assert!(!package.items.is_empty());
        // Top hit is the exact `refund` symbol with a real excerpt.
        match &package.items[0] {
            ContextItem::Symbol { name, path, start_line, text, reasons, .. } => {
                assert_eq!(name, "refund");
                assert_eq!(path, "src/payments.py");
                assert_eq!(*start_line, 4);
                assert!(
                    text.as_deref().unwrap_or("").starts_with("def refund(amount):"),
                    "excerpt: {text:?}"
                );
                assert!(reasons.iter().any(|r| r.contains("exact symbol match")));
            }
            other => panic!("expected symbol top hit, got {other:?}"),
        }
        // Ranked: scores never increase down the list.
        let scores: Vec<u32> = package.items.iter().map(ContextItem::score).collect();
        assert!(
            scores.windows(2).all(|w| w[0] >= w[1]),
            "scores: {scores:?}"
        );
    }

    #[test]
    fn package_is_byte_stable_across_runs() {
        let (dir, conn) = fixture();
        let options = rooted(&dir);
        let a = serde_json::to_string(&build_package(&conn, "refund payments", &options).unwrap())
            .unwrap();
        let b = serde_json::to_string(&build_package(&conn, "refund payments", &options).unwrap())
            .unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn tiny_budget_omits_tail_and_top_item_always_fits() {
        let (dir, conn) = fixture();
        let mut options = rooted(&dir);
        options.token_budget = MIN_TOKEN_BUDGET;
        let package = build_package(&conn, "refund payments", &options).unwrap();
        assert!(!package.items.is_empty(), "top hit always included");
        assert!(package.omitted > 0, "tail must be cut at {MIN_TOKEN_BUDGET}");
        // items + omitted covers every ranked candidate.
        let full = build_package(&conn, "refund payments", &rooted(&dir)).unwrap();
        assert_eq!(package.items.len() + package.omitted, full.items.len() + full.omitted);
        // Compliance: only the documented single-item overrun may exceed.
        if package.estimated_tokens > package.token_budget {
            assert_eq!(package.items.len(), 1);
            assert!(package.notes.iter().any(|n| n.contains("exceeds")));
        }
    }

    #[test]
    fn file_shadowed_by_its_symbols_but_kept_for_path_only_matches() {
        let (dir, conn) = fixture();
        let options = rooted(&dir);
        // `refund` cites src/payments.py via symbols: no File item for it.
        let package = build_package(&conn, "refund", &options).unwrap();
        assert!(package.items.iter().all(|c| match c {
            ContextItem::File { path, .. } => path != "src/payments.py",
            _ => true,
        }));
        // `auth` matches only the path (no symbol named like it): File kept.
        let package = build_package(&conn, "auth", &options).unwrap();
        let file = package.items.iter().find_map(|c| match c {
            ContextItem::File { path, text, .. } => Some((path, text)),
            _ => None,
        });
        let (path, text) = file.expect("path-only File item");
        assert_eq!(path, "src/auth.py");
        assert!(text.as_deref().unwrap_or("").contains("def login"));
    }

    #[test]
    fn excerpts_omitted_without_root_with_note() {
        let (_dir, conn) = fixture();
        let package =
            build_package(&conn, "refund", &ContextOptions::default()).unwrap();
        assert!(package.items.iter().all(|c| match c {
            ContextItem::Symbol { text, .. }
            | ContextItem::File { text, .. }
            | ContextItem::Test { text, .. } => text.is_none(),
            ContextItem::Knowledge { .. } => true,
        }));
        assert!(package.notes.iter().any(|n| n.contains("No source root")));
    }

    #[test]
    fn empty_and_unmatched_tasks_explain_themselves() {
        let (dir, conn) = fixture();
        let empty = build_package(&conn, "   ", &rooted(&dir)).unwrap();
        assert!(empty.items.is_empty());
        assert!(empty.notes.iter().any(|n| n.contains("Empty task")));

        let miss = build_package(&conn, "zzzqqq", &rooted(&dir)).unwrap();
        assert!(miss.items.is_empty());
        assert!(miss.notes.iter().any(|n| n.contains("zzzqqq")), "{:?}", miss.notes);
    }

    #[test]
    fn budget_clamps_with_note() {
        let (dir, conn) = fixture();
        let mut options = rooted(&dir);
        options.token_budget = 0;
        let package = build_package(&conn, "refund", &options).unwrap();
        assert_eq!(package.token_budget, MIN_TOKEN_BUDGET);
        assert!(package.notes.iter().any(|n| n.contains("clamped")));
        options.token_budget = usize::MAX;
        let package = build_package(&conn, "refund", &options).unwrap();
        assert_eq!(package.token_budget, MAX_TOKEN_BUDGET);
    }

    #[test]
    fn empty_index_still_serves_knowledge_with_note() {
        let conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        let mut conn = conn;
        let record = KnowledgeRecord::new(
            "payments.refunds",
            KnowledgeType::Fact,
            "Refunds",
            "Refunds negate the charge amount",
            KnowledgeOrigin::Agent,
        );
        crate::knowledge::put_record(&mut conn, &record).unwrap();

        let package = build_package(&conn, "refund", &ContextOptions::default()).unwrap();
        assert_eq!(package.items.len(), 1);
        assert!(package.notes.iter().any(|n| n.contains("only knowledge records")));
    }

    #[test]
    fn knowledge_records_join_the_package() {
        let (dir, mut conn) = fixture();
        let mut record = KnowledgeRecord::new(
            "payments.refunds",
            KnowledgeType::Fact,
            "Refunds",
            "Refunds negate the charge amount",
            KnowledgeOrigin::Agent,
        );
        record.status = KnowledgeStatus::Supported;
        crate::knowledge::put_record(&mut conn, &record).unwrap();

        let package = build_package(&conn, "refund", &rooted(&dir)).unwrap();
        let hit = package.items.iter().find_map(|c| match c {
            ContextItem::Knowledge { record, reasons, .. } => Some((record, reasons)),
            _ => None,
        });
        let (record, reasons) = hit.expect("knowledge hit");
        assert_eq!(record.id, "payments.refunds");
        assert!(reasons.iter().any(|r| r.contains("supported knowledge")));
    }
}
