//! Retrieval signals: task terms plus the five candidate sources.
//!
//! Signal order is fixed (exact symbols, whole-task symbol, substrings,
//! paths, knowledge, tests, proximity) and every intermediate list is sorted
//! before accumulation, so candidate reasons — and therefore packages — are
//! byte-stable for identical inputs.

use crate::context::{
    SCORE_EXACT_SYMBOL, SCORE_KNOWLEDGE_OTHER, SCORE_KNOWLEDGE_PINNED, SCORE_PATH_FILE_BONUS,
    SCORE_PATH_MATCH, SCORE_PROXIMITY, SCORE_SYMBOL_SUBSTRING, SCORE_TEST_REF,
    SCORE_WHOLE_TASK_SYMBOL,
};
use crate::context::{ContextItem, ContextOptions};
use crate::db::queries;
use crate::error::Result;
use crate::graph::deps;
use crate::knowledge::{self, KnowledgeStatus};
use rusqlite::Connection;
use std::collections::{BTreeSet, HashMap};

/// Maximum task terms used (deterministic prefix; the package notes cuts).
pub const MAX_TERMS: usize = 32;
/// Symbol-substring hits pulled per term (bounded fan-out; noted when hit).
pub const MAX_SUBSTRING_HITS_PER_TERM: usize = 50;
/// Knowledge hits pulled per term.
pub const MAX_KNOWLEDGE_HITS_PER_TERM: usize = 10;
/// Test references kept per matched symbol.
pub const MAX_TEST_REFS_PER_SYMBOL: usize = 100;

/// Articles, prepositions, and pronouns dropped from task text. Task verbs
/// (`add`, `fix`, …) are deliberately kept: they rarely match exactly, and
/// when they do (a symbol literally named `add`) the hit is legitimate.
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "to", "in", "on", "of", "for", "with", "by", "from", "that",
    "this", "it", "is", "are", "be", "how", "what", "why", "when", "where", "as", "at", "into",
];

/// Split task text into lowercase search terms: alphanumeric runs, stopwords
/// and single characters dropped, deduplicated in first-seen order.
pub fn extract_terms(task: &str) -> Vec<String> {
    let mut terms = Vec::new();
    for raw in task.split(|c: char| !c.is_alphanumeric()) {
        let term = raw.to_lowercase();
        if term.len() < 2 || STOPWORDS.contains(&term.as_str()) || terms.contains(&term) {
            continue;
        }
        terms.push(term);
    }
    terms
}

/// A path is test-related when a directory segment is a test dir (`test`,
/// `tests`, `spec`, `specs`, `__tests__`) or the file stem follows test
/// naming: exactly `test`/`spec`/`conftest`, `test_*`/`spec_*` prefixes,
/// `*_test`/`*_spec`/`*.spec` suffixes, or camel `TestXxx`/`SpecXxx`.
/// Word-boundaried deliberately: looser checks claim `latest.rs`,
/// `contest.rs`, and `testing-guide.md` as tests.
pub fn is_test_path(path: &str) -> bool {
    const TEST_DIRS: &[&str] = &["test", "tests", "spec", "specs", "__tests__"];
    let lower = path.to_lowercase();
    let mut segments: Vec<&str> = lower.split(['/', '\\']).collect();
    let file = segments.pop().unwrap_or("");
    if segments.iter().any(|seg| TEST_DIRS.contains(seg)) {
        return true;
    }
    let stem = file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file);
    if stem == "test" || stem == "spec" || stem == "conftest" {
        return true;
    }
    if stem.starts_with("test_")
        || stem.starts_with("spec_")
        || stem.ends_with("_test")
        || stem.ends_with("_spec")
        || stem.ends_with(".spec")
    {
        return true;
    }
    // Camel `TestAuth` / `SpecRunner`: prefix + uppercase boundary on the
    // original-case stem (lowercased text cannot show the boundary).
    let orig_file = path.rsplit(['/', '\\']).next().unwrap_or("");
    let orig_stem = orig_file.rsplit_once('.').map(|(s, _)| s).unwrap_or(orig_file);
    (orig_stem.starts_with("Test") || orig_stem.starts_with("Spec"))
        && orig_stem.chars().nth(4).is_some_and(|c| c.is_uppercase())
}

/// Candidate identity: one entry per citable unit.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum CandidateKey {
    Symbol { path: String, name: String, line: u32 },
    File { path: String },
    Test { path: String, line: u32 },
    Knowledge { id: String },
}

/// An accumulating candidate: the item plus the symbol's module for the
/// proximity signal (symbols only).
struct Candidate {
    item: ContextItem,
    module: Option<String>,
}

/// Gathered candidates plus gathering notes (term cuts, truncations).
pub struct Gathered {
    /// Candidates in no particular order (the assembler ranks them).
    pub candidates: Vec<ContextItem>,
    /// Notes about truncated signals.
    pub notes: Vec<String>,
}

/// Run all signals and accumulate scored candidates.
pub fn gather_candidates(
    conn: &Connection,
    task: &str,
    options: &ContextOptions,
) -> Result<Gathered> {
    let mut notes = Vec::new();
    let mut terms = extract_terms(task);
    if terms.len() > MAX_TERMS {
        notes.push(format!(
            "Task yielded {} terms; using the first {MAX_TERMS}.",
            terms.len()
        ));
        terms.truncate(MAX_TERMS);
    }
    let mut acc: HashMap<CandidateKey, Candidate> = HashMap::new();

    // Signal 1: exact symbol matches per term (case-sensitive, like
    // `definition`), plus a bonus when the whole task names a symbol.
    let mut exact_names: BTreeSet<String> = BTreeSet::new();
    for term in &terms {
        let mut defs = queries::find_definition(conn, term)?;
        defs.sort_by(|a, b| {
            a.file
                .cmp(&b.file)
                .then_with(|| a.start_line.cmp(&b.start_line))
                .then_with(|| a.name.cmp(&b.name))
        });
        for def in defs {
            exact_names.insert(def.name.clone());
            upsert_symbol(&mut acc, &def, SCORE_EXACT_SYMBOL, format!("exact symbol match for `{term}`"));
        }
    }
    let whole = task.trim();
    if !whole.is_empty() {
        let mut defs = queries::find_definition(conn, whole)?;
        defs.sort_by(|a, b| {
            a.file
                .cmp(&b.file)
                .then_with(|| a.start_line.cmp(&b.start_line))
                .then_with(|| a.name.cmp(&b.name))
        });
        for def in defs {
            exact_names.insert(def.name.clone());
            upsert_symbol(
                &mut acc,
                &def,
                SCORE_WHOLE_TASK_SYMBOL,
                format!("task names `{whole}` exactly"),
            );
        }
    }

    // Signal 2: symbol-name substrings (case-insensitive), capped per term.
    for term in &terms {
        let (mut hits, truncated) =
            queries::search_symbols(conn, term, MAX_SUBSTRING_HITS_PER_TERM)?;
        if truncated {
            notes.push(format!(
                "Symbol matches for `{term}` truncated at {MAX_SUBSTRING_HITS_PER_TERM}; narrow terms for full recall."
            ));
        }
        hits.sort_by(|a, b| {
            a.file
                .cmp(&b.file)
                .then_with(|| a.start_line.cmp(&b.start_line))
                .then_with(|| a.name.cmp(&b.name))
        });
        for hit in hits {
            upsert_symbol(
                &mut acc,
                &hit,
                SCORE_SYMBOL_SUBSTRING,
                format!("symbol name contains `{term}`"),
            );
        }
    }

    // Signal 3: filename/path relevance. Matched files become File
    // candidates; matched symbols inside them earn a bonus.
    let mut files = queries::indexed_files(conn)?;
    files.sort();
    let mut matched_files: BTreeSet<String> = BTreeSet::new();
    for file in &files {
        let lower = file.to_lowercase();
        for term in &terms {
            if lower.contains(term) {
                matched_files.insert(file.clone());
                acc.entry(CandidateKey::File { path: file.clone() })
                    .or_insert_with(|| Candidate {
                        item: ContextItem::File {
                            path: file.clone(),
                            text: None,
                            score: 0,
                            reasons: Vec::new(),
                        },
                        module: None,
                    })
                    .item
                    .add_hit(SCORE_PATH_MATCH, format!("path matches `{term}`"));
            }
        }
    }
    for candidate in acc.values_mut() {
        if let ContextItem::Symbol { path, .. } = &candidate.item {
            if matched_files.contains(path) {
                candidate.item.add_hit(
                    SCORE_PATH_FILE_BONUS,
                    format!("defined in path-matched file `{path}`"),
                );
            }
        }
    }

    // Signal 4: linked knowledge (optional).
    if options.include_knowledge {
        for term in &terms {
            for record in
                knowledge::search_records(conn, term, Some(MAX_KNOWLEDGE_HITS_PER_TERM))?
            {
                let (weight, label) = if record.status == KnowledgeStatus::Supported {
                    (SCORE_KNOWLEDGE_PINNED, "supported")
                } else {
                    (SCORE_KNOWLEDGE_OTHER, record.status.as_db())
                };
                acc.entry(CandidateKey::Knowledge { id: record.id.clone() })
                    .or_insert_with(|| Candidate {
                        item: ContextItem::Knowledge {
                            record: record.clone(),
                            score: 0,
                            reasons: Vec::new(),
                        },
                        module: None,
                    })
                    .item
                    .add_hit(weight, format!("{label} knowledge matches `{term}`"));
            }
        }
    }

    // Signal 5: tests referencing exact-matched symbols (optional).
    if options.include_tests {
        for name in &exact_names {
            let mut refs = queries::find_references(conn, name)?;
            refs.sort_by(|a, b| {
                a.file
                    .cmp(&b.file)
                    .then_with(|| a.start_line.cmp(&b.start_line))
            });
            refs.truncate(MAX_TEST_REFS_PER_SYMBOL);
            for r in refs {
                let path = r.file.to_string_lossy().into_owned();
                if !is_test_path(&path) {
                    continue;
                }
                acc.entry(CandidateKey::Test {
                    path: path.clone(),
                    line: r.start_line,
                })
                .or_insert_with(|| Candidate {
                    item: ContextItem::Test {
                        path,
                        start_line: r.start_line,
                        text: None,
                        symbol: name.clone(),
                        score: 0,
                        reasons: Vec::new(),
                    },
                    module: None,
                })
                .item
                .add_hit(SCORE_TEST_REF, format!("tests `{name}`"));
            }
        }
    }

    // Signal 6: dependency proximity — term-matching symbols one hop from an
    // exact match's module. Bonus-only by design: it re-ranks, never floods.
    let mut neighbor_modules: BTreeSet<String> = BTreeSet::new();
    let mut exact_modules: BTreeSet<String> = BTreeSet::new();
    for candidate in acc.values() {
        if let (ContextItem::Symbol { .. }, Some(module)) = (&candidate.item, &candidate.module) {
            if !module.is_empty() {
                exact_modules.insert(module.clone());
            }
        }
    }
    for module in &exact_modules {
        for dep in deps::find_dependencies(conn, module)? {
            if !dep.external {
                neighbor_modules.insert(dep.module_path);
            }
        }
        for dep in deps::find_dependents(conn, module)? {
            if !dep.external {
                neighbor_modules.insert(dep.module_path);
            }
        }
    }
    neighbor_modules.retain(|m| !exact_modules.contains(m));
    for candidate in acc.values_mut() {
        if let (ContextItem::Symbol { .. }, Some(module)) = (&candidate.item, &candidate.module) {
            if neighbor_modules.contains(module) {
                candidate.item.add_hit(
                    SCORE_PROXIMITY,
                    format!("one dependency hop from a matched module (in `{module}`)"),
                );
            }
        }
    }

    notes.sort();
    notes.dedup();
    Ok(Gathered {
        candidates: acc.into_values().map(|c| c.item).collect(),
        notes,
    })
}

/// Insert or bump a symbol candidate.
fn upsert_symbol(
    acc: &mut HashMap<CandidateKey, Candidate>,
    def: &crate::graph::types::Symbol,
    weight: u32,
    reason: String,
) {
    let path = def.file.to_string_lossy().into_owned();
    acc.entry(CandidateKey::Symbol {
        path: path.clone(),
        name: def.name.clone(),
        line: def.start_line,
    })
    .or_insert_with(|| Candidate {
        item: ContextItem::Symbol {
            path,
            name: def.name.clone(),
            symbol_kind: def.kind.as_db(),
            start_line: def.start_line,
            text: None,
            score: 0,
            reasons: Vec::new(),
        },
        module: Some(def.module_path.clone()),
    })
    .item
    .add_hit(weight, reason);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;
    use crate::index;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn terms_split_lowercase_and_drop_stopwords() {
        assert_eq!(
            extract_terms("Add refund support to the Payments API!"),
            vec!["add", "refund", "support", "payments", "api"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );
        assert_eq!(extract_terms("a I x"), Vec::<String>::new());
        // Dedup preserves first-seen order.
        assert_eq!(
            extract_terms("auth Auth AUTH"),
            vec!["auth".to_string()]
        );
    }

    #[test]
    fn test_path_heuristic_covers_common_layouts() {
        for yes in [
            "tests/test_auth.py",
            "src/auth_test.go",
            "src/auth.spec.ts",
            "tests/conftest.py",
            "spec/models/user_spec.rb",
            "src/__tests__/auth.ts",
            "src/TestAuth.java",
        ] {
            assert!(is_test_path(yes), "{yes}");
        }
        for no in [
            "src/auth.py",
            "src/latest.rs",
            "src/contest.rs",
            "src/greatest.py",
            "docs/deploy.md",
            "docs/testing-guide.md",
        ] {
            assert!(!is_test_path(no), "{no}");
        }
    }

    /// Fixture: `src/payments.py` (charge, refund), `tests/test_payments.py`
    /// (references refund), `src/auth.py` (login).
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

    #[test]
    fn gather_finds_symbols_tests_and_paths() {
        let (_dir, conn) = fixture();
        let options = ContextOptions::default();
        let gathered = gather_candidates(&conn, "Add refund support to payments", &options).unwrap();

        // Exact symbol hit for `refund`.
        let symbol = gathered.candidates.iter().find_map(|c| match c {
            ContextItem::Symbol { name, score, reasons, .. } if name == "refund" => {
                Some((score, reasons))
            }
            _ => None,
        });
        let (score, reasons) = symbol.expect("refund symbol candidate");
        assert!(*score >= SCORE_EXACT_SYMBOL, "reasons: {reasons:?}");
        assert!(reasons.iter().any(|r| r.contains("exact symbol match")));

        // `charge` shares the path-matched file but matches no term: substring
        // signal stays quiet, so it appears only via nothing — assert absence
        // to pin the bonus-only proximity/path design (no flooding).
        assert!(
            gathered.candidates.iter().all(|c| match c {
                ContextItem::Symbol { name, .. } => name != "charge",
                _ => true,
            }),
            "unmatched symbols must not flood candidates"
        );

        // Test reference to `refund`.
        assert!(
            gathered.candidates.iter().any(|c| matches!(
                c,
                ContextItem::Test { symbol, path, .. }
                    if symbol == "refund" && path == "tests/test_payments.py"
            )),
            "test candidate missing"
        );

        // Path match on payments.py becomes a File candidate (symbols in it
        // are cited separately; the assembler dedupes).
        assert!(
            gathered.candidates.iter().any(|c| matches!(
                c,
                ContextItem::File { path, .. } if path == "src/payments.py"
            )),
            "file candidate missing"
        );
    }

    #[test]
    fn gather_respects_kb_and_test_toggles() {
        let (_dir, conn) = fixture();
        let options = ContextOptions {
            include_tests: false,
            include_knowledge: false,
            ..ContextOptions::default()
        };
        let gathered = gather_candidates(&conn, "refund payments", &options).unwrap();
        assert!(gathered.candidates.iter().all(|c| !matches!(
            c,
            ContextItem::Test { .. } | ContextItem::Knowledge { .. }
        )));
    }
}
