//! Documentation drift detection (KnowledgeCI).
//!
//! Docs drift: symbols get renamed, files move, traits become structs, and
//! the architecture page still says the old thing. The drift checker
//! re-reads the repo's human docs (`docs/`, agent instructions, ADRs) plus
//! generated Keel wiki pages and reports every claim that no longer matches
//! the index — a CI gate (`keel drift` exits 1 on findings) with
//! human-reviewed suggestions, never silent rewrites.
//!
//! Three verdicts, each backed by deterministic evidence:
//! - [`DriftVerdict::PotentiallyStale`] — the claim is gone but a close
//!   match exists (a possible rename), or a wiki page's inputs changed
//!   since generation.
//! - [`DriftVerdict::Unsupported`] — the claim resolves to nothing: no
//!   such symbol, no such file. It may be removed, external, or prose
//!   that merely looks like code; the evidence says which checks ran.
//! - [`DriftVerdict::Contradicted`] — the claim resolves but disagrees:
//!   the doc calls `` `Store` `` a trait while the index defines a struct.
//!
//! What the checker reads: backtick spans outside fenced code blocks
//! (see [`scan`]). Fenced blocks are samples, not claims. A bare word in
//! backticks reads as a symbol claim — docs should quote prose emphasis
//! (`"never"`, not `` `never` ``) to stay out of the checker's way.
//! Leading-dot shorthands (`.rs`) read as extension mentions, not files.
//!
//! Path claims resolve against the doc dir, then the project root, then
//! a unique basename or suffix anywhere in the repo (`daemon.rs`,
//! `languages/mod.rs`); ambiguous fragments skip quietly with a sweep
//! note. `:LINE` suffixes strip, `path::symbol` hybrids resolve both
//! halves, and MCP paths, runtime files (`watch.log`), and root-escaping
//! `../…` examples skip. Shell commands, URLs, templates, globs,
//! assignments, MIME types, and format escapes never read as paths.
//! Kind checks need a kind word separated from the span by whitespace
//! alone, plus a uniquely-defined symbol (`impl` rows don't count
//! toward ambiguity).
//!
//! Symbol misses climb a ladder before becoming findings: reserved
//! words (keywords, `ts`-style shorthands, toolchain binaries),
//! live schema names and `meta` keys, external imports, unique
//! case-insensitive matches, `KEEL_*` config, the project name, live
//! MCP/CLI names, and referenced-but-undefined names (`Sync`).
//! Member claims on values (`db.get()`) skip as unattributable receiver
//! samples. A real definition always beats every rung.
//!
//! Limits, stated plainly: only backticked claims are checked (bare prose
//! is invisible), and semantic contradictions ("X is fast") are reviewer
//! territory — the core flags structural mismatch only.

pub mod scan;

use crate::db::queries;
use crate::error::Result;
use crate::fs::{prune_skipped_dirs, walker};
use crate::graph::resolve::suggest_names;
use crate::graph::types::{Symbol, SymbolKind};
use clap::CommandFactory;
use rusqlite::Connection;
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Verdict on one doc claim. Ordered by severity for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DriftVerdict {
    /// Gone, but a close match exists (possible rename), or a wiki
    /// page's pinned inputs changed.
    PotentiallyStale,
    /// Resolves to nothing: no such symbol, no such file.
    Unsupported,
    /// Resolves, but disagrees with the doc (kind mismatch).
    Contradicted,
}

impl DriftVerdict {
    /// CLI display label.
    pub fn as_str(self) -> &'static str {
        match self {
            DriftVerdict::PotentiallyStale => "potentially stale",
            DriftVerdict::Unsupported => "unsupported",
            DriftVerdict::Contradicted => "contradicted",
        }
    }
}

/// One doc claim that no longer matches the index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DriftFinding {
    /// Doc path relative to the project root, `/`-separated.
    pub doc: String,
    /// 1-based line of the claim; 0 for page-level findings (wiki pins).
    pub line: u32,
    /// The claim text (span content, or the wiki page id).
    pub claim: String,
    /// The verdict and why it applies.
    pub verdict: DriftVerdict,
    /// Evidence sentence: what was checked and what came back.
    pub evidence: String,
    /// Human-reviewed suggestion, never applied automatically
    /// (rename target, actual kind, indexed path).
    pub suggestion: Option<String>,
}

/// A full drift sweep over a project's docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DriftReport {
    /// Prose docs scanned (wiki pages counted separately).
    pub docs_checked: usize,
    /// Wiki pages checked against their manifest pins.
    pub wiki_pages_checked: usize,
    /// Findings ordered by `(doc, line, claim)`.
    pub findings: Vec<DriftFinding>,
    /// Non-finding notes: skips, caps, empty-index caveats.
    pub notes: Vec<String>,
}

impl DriftReport {
    /// True when no claim mismatched.
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
}

/// Maximum prose docs per sweep; overflow is reported, not silent.
pub const MAX_DOCS: usize = 500;
/// Maximum findings kept per doc; overflow is reported, not silent.
pub const MAX_FINDINGS_PER_DOC: usize = 100;
/// Rename suggestions offered per stale symbol.
const SUGGESTION_LIMIT: usize = 3;

/// Agent-instruction filenames honored at any depth.
const AGENT_FILES: &[&str] = &["agents.md", "claude.md", "cursorrules.md", "muse.md"];
/// Directory names that mark decision records at any depth.
const ADR_DIRS: &[&str] = &["adr", "adrs", "decisions"];

/// Discover the docs a drift sweep covers: `docs/**/*.md` (excluding the
/// generated `docs/keel` wiki, which is checked via its manifest pins),
/// agent instructions (`AGENTS.md`, `CLAUDE.md`, …) at any depth, and
/// ADR/decision records. Sorted by `/`-separated relative path.
pub fn discover_docs(root: &Path) -> Vec<PathBuf> {
    let mut docs: Vec<PathBuf> = walker(root)
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                // Prune the generated wiki tree before descending: its
                // drift comes from manifest pins, not prose spans.
                if entry.file_name().to_str().unwrap_or("") == "keel"
                    && entry
                        .path()
                        .parent()
                        .and_then(|p| p.file_name())
                        .and_then(|n| n.to_str())
                        == Some("docs")
                {
                    return false;
                }
                return true;
            }
            true
        })
        .build()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path().to_path_buf())
        .filter(|path| path.is_file())
        .filter(|path| {
            path.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
                .unwrap_or(false)
        })
        .filter(|path| {
            let rel = path.strip_prefix(root).unwrap_or(path);
            covers_doc(rel)
        })
        .collect();
    docs.sort();
    docs.truncate(MAX_DOCS);
    docs
}

/// True when a root-relative path is in drift scope. Matching is
/// case-insensitive; separators are normalized to `/`.
fn covers_doc(rel: &Path) -> bool {
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    let lower = rel_str.to_lowercase();
    if lower.starts_with("docs/keel/") || lower == "docs/keel" {
        return false;
    }
    if lower.starts_with("docs/") {
        return true;
    }
    let file_name = rel
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_lowercase();
    if AGENT_FILES.contains(&file_name.as_str()) {
        return true;
    }
    if lower.split('/').any(|seg| ADR_DIRS.contains(&seg)) {
        return true;
    }
    let stem = file_name
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(&file_name);
    stem.contains("adr")
}

/// Every file and directory in the repo (root-relative, `/`-separated,
/// sorted), collected once per sweep so path claims resolve against the
/// same snapshot. Honors `.gitignore`/`.keelignore` and prunes the same
/// junk dirs as the indexer, but keeps hidden trees (`.github/`) since
/// docs legitimately reference them.
pub(crate) struct RepoFiles {
    paths: Vec<String>,
}

impl RepoFiles {
    pub(crate) fn collect(root: &Path) -> Vec<String> {
        let mut paths: Vec<String> = walker(root)
            .hidden(false)
            .filter_entry(prune_skipped_dirs)
            .build()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path().to_path_buf())
            .filter(|path| path != root)
            .map(|path| {
                path.strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }

    /// Repo paths whose final segment is `name` (files and dirs alike).
    pub(crate) fn basename_hits(&self, name: &str) -> Vec<&str> {
        self.paths
            .iter()
            .filter(|p| p.rsplit('/').next() == Some(name))
            .map(String::as_str)
            .collect()
    }

    /// Repo paths ending at multi-segment `suffix` on a `/` boundary.
    pub(crate) fn suffix_hits(&self, suffix: &str) -> Vec<&str> {
        self.paths
            .iter()
            .filter(|p| {
                p.len() > suffix.len()
                    && p.ends_with(suffix)
                    && p.as_bytes()[p.len() - suffix.len() - 1] == b'/'
            })
            .map(String::as_str)
            .collect()
    }
}

/// Per-sweep resolution context: everything a claim can resolve
/// against besides symbol definitions, collected once so every doc sees
/// the same snapshot. Shared with the ADR module, which resolves the
/// same claims into links instead of findings.
pub(crate) struct SweepCtx {
    /// Every file and directory in the repo (path ladder).
    pub(crate) repo: RepoFiles,
    /// Lowercased external import modules plus their last segments and
    /// aliases (`rusqlite`, `sync` from `std::sync`).
    external: HashSet<String>,
    /// Live MCP tool names (exact).
    mcp_tools: HashSet<String>,
    /// CLI (sub)command names, kebab-case (exact).
    cli_verbs: HashSet<String>,
    /// Lowercased DB table names.
    tables: HashSet<String>,
    /// Lowercased DB column names.
    columns: HashSet<String>,
    /// Lowercased `meta` keys (`keel_version`).
    meta_keys: HashSet<String>,
    /// Lowercased project root basename (`keel`).
    root_name: Option<String>,
}

impl SweepCtx {
    pub(crate) fn collect(conn: &Connection, root: &Path) -> Result<Self> {
        let mut external = HashSet::new();
        for module in queries::distinct_import_modules(conn)? {
            // Root and leaf (`serde`, `serialize` from `serde::Serialize`):
            // docs cite crate roots and imported names alike.
            let segs: Vec<&str> = module
                .split([':', '/', '.'])
                .filter(|s| !s.is_empty())
                .collect();
            if let Some(first) = segs.first() {
                external.insert(first.to_lowercase());
            }
            if let Some(last) = segs.last() {
                external.insert(last.to_lowercase());
            }
        }
        for (_file, _module, alias) in queries::aliased_imports(conn)? {
            external.insert(alias.to_lowercase());
        }
        let mut mcp_tools = HashSet::new();
        let tools_payload = crate::mcp::tools_list_result();
        if let Some(tools) = tools_payload.get("tools").and_then(|t| t.as_array()) {
            for tool in tools {
                if let Some(name) = tool.get("name").and_then(|n| n.as_str()) {
                    mcp_tools.insert(name.to_string());
                }
            }
        }
        let mut cli_verbs = HashSet::new();
        // Top-level verbs plus one nesting level (`knowledge status`),
        // aliases included (`insight`).
        for sub in crate::cli::Cli::command().get_subcommands() {
            cli_verbs.insert(sub.get_name().to_string());
            cli_verbs.extend(sub.get_aliases().map(str::to_string));
            cli_verbs.extend(sub.get_visible_aliases().map(str::to_string));
            for nested in sub.get_subcommands() {
                cli_verbs.insert(nested.get_name().to_string());
                cli_verbs.extend(nested.get_aliases().map(str::to_string));
                cli_verbs.extend(nested.get_visible_aliases().map(str::to_string));
            }
        }
        let (tables, columns) = live_schema_names(conn)?;
        let meta_keys: HashSet<String> = conn
            .prepare("SELECT key FROM meta")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .map(|k| k.to_lowercase())
            .collect();
        let root_name = root
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_lowercase());
        Ok(SweepCtx {
            repo: RepoFiles {
                paths: RepoFiles::collect(root),
            },
            external,
            mcp_tools,
            cli_verbs,
            tables,
            columns,
            meta_keys,
            root_name,
        })
    }
}

/// Lowercased table and column names from the live database schema, so
/// doc citations of storage (`symbols`, `symbols.container`) resolve
/// without a hardcoded copy.
fn live_schema_names(conn: &Connection) -> Result<(HashSet<String>, HashSet<String>)> {
    let mut tables = HashSet::new();
    let mut columns = HashSet::new();
    let table_names: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for table in &table_names {
        tables.insert(table.to_lowercase());
        // Table names come from our own schema; still quoted since
        // `references` is a reserved word.
        let pragma = format!("PRAGMA table_info(\"{table}\")");
        let col_names: Vec<String> = conn
            .prepare(&pragma)?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for col in col_names {
            columns.insert(col.to_lowercase());
        }
    }
    Ok((tables, columns))
}

/// Check `docs` (absolute paths under `root`) plus the generated wiki at
/// `<root>/docs/keel`, returning an ordered report.
pub fn check_docs(
    conn: &Connection,
    root: &Path,
    docs: &[PathBuf],
) -> Result<DriftReport> {
    let mut findings = Vec::new();
    let mut notes = Vec::new();
    if docs.len() >= MAX_DOCS {
        notes.push(format!(
            "doc discovery capped at {MAX_DOCS}; scope with --path for the rest"
        ));
    }
    let index_empty = queries::distinct_symbol_names(conn)?.is_empty();
    if index_empty {
        notes.push(
            "index holds no symbols; symbol checks skipped, path checks still ran".to_string(),
        );
    }
    let ctx = SweepCtx::collect(conn, root)?;
    let mut ambiguous_paths = 0usize;
    for doc in docs {
        // Vanished paths are not drift; scoped callers (facade, CLI) note
        // missing paths themselves, discovery never yields them.
        if !doc.is_file() {
            continue;
        }
        let (mut doc_findings, mut doc_notes) =
            check_one_doc(conn, root, doc, index_empty, &ctx, &mut ambiguous_paths)?;
        findings.append(&mut doc_findings);
        notes.append(&mut doc_notes);
    }
    if ambiguous_paths > 0 {
        notes.push(format!(
            "{ambiguous_paths} ambiguous path claim(s) skipped; qualify with a directory to check them"
        ));
    }
    let wiki_pages_checked = check_wiki(conn, root, &mut findings, &mut notes)?;
    findings.sort_by(|a, b| {
        (&a.doc, a.line, &a.claim).cmp(&(&b.doc, b.line, &b.claim))
    });
    Ok(DriftReport {
        docs_checked: docs.len(),
        wiki_pages_checked,
        findings,
        notes,
    })
}

/// Check one prose doc. Returns its findings plus any notes.
fn check_one_doc(
    conn: &Connection,
    root: &Path,
    doc: &Path,
    index_empty: bool,
    ctx: &SweepCtx,
    ambiguous_paths: &mut usize,
) -> Result<(Vec<DriftFinding>, Vec<String>)> {
    let rel = doc
        .strip_prefix(root)
        .unwrap_or(doc)
        .to_string_lossy()
        .replace('\\', "/");
    let text = match std::fs::read_to_string(doc) {
        Ok(text) => text,
        Err(_) => {
            return Ok((
                Vec::new(),
                vec![format!("unreadable; skipped: {rel}")],
            ));
        }
    };
    let mut check = DocCheck {
        conn,
        rel: &rel,
        root,
        doc,
        ctx,
        ambiguous_paths,
    };
    let mut findings = Vec::new();
    for span in scan::extract_spans(&text) {
        match scan::classify(&span.text) {
            scan::ClaimKind::Ignore => {}
            scan::ClaimKind::Path { raw } => {
                if let Some(finding) = check.path_claim(&span, &raw)? {
                    findings.push(finding);
                }
            }
            scan::ClaimKind::Symbol { name, container } => {
                if index_empty {
                    continue;
                }
                if let Some(finding) = check.symbol_claim(&span, &name, container.as_deref())? {
                    findings.push(finding);
                }
            }
        }
    }
    let mut notes = Vec::new();
    if findings.len() > MAX_FINDINGS_PER_DOC {
        notes.push(format!(
            "{rel}: findings capped at {MAX_FINDINGS_PER_DOC}"
        ));
        findings.truncate(MAX_FINDINGS_PER_DOC);
    }
    Ok((findings, notes))
}

/// Strip up to two trailing `:NUMBER` groups (`file.rs:12:34` → `file.rs`).
/// Non-numeric tails (`C:/x`, `host:port`) pass through untouched.
fn strip_line_suffix(path: &str) -> &str {
    let mut out = path;
    for _ in 0..2 {
        match out.rsplit_once(':') {
            Some((head, tail))
                if !head.is_empty()
                    && !tail.is_empty()
                    && tail.chars().all(|c| c.is_ascii_digit()) =>
            {
                out = head;
            }
            _ => break,
        }
    }
    out
}

/// True when `target` resolved from `base` leaves the project `root`
/// lexically (`../../x.py` from a doc dir). Relative bases can't be
/// judged and return false (the claim checks normally).
fn escapes_root(base: &Path, target: &str, root: &Path) -> bool {
    if !base.is_absolute() || !root.is_absolute() {
        return false;
    }
    let joined = base.join(target).to_string_lossy().replace('\\', "/");
    let norm = crate::graph::target::clean_path(&joined);
    let root_str = root.to_string_lossy().replace('\\', "/");
    norm != root_str && !norm.starts_with(&format!("{root_str}/"))
}

/// Doc kind words mapped to indexed kind labels (`fn` reads as function,
/// `static` as const). Words outside this table make no kind claim.
fn doc_kind(word: &str) -> Option<&'static str> {
    match word {
        "function" | "fn" => Some("function"),
        "method" => Some("method"),
        "struct" => Some("struct"),
        "trait" => Some("trait"),
        "class" => Some("class"),
        "interface" => Some("interface"),
        "enum" => Some("enum"),
        "module" => Some("module"),
        "const" => Some("const"),
        "static" => Some("const"),
        _ => None,
    }
}

/// Symbol lookup ingredients shared by drift findings and ADR links:
/// container-scoped defs, bare-name defs, container defs, plus the two
/// miss-ladder outcomes. `quiet_early` (single-char spans, `self`/`this`
/// receivers, schema pairs) wins over definitions; `quiet_late` (the
/// bare-name ladder) is consulted only when nothing is defined.
pub(crate) struct SymbolLookup {
    pub(crate) scoped: Vec<Symbol>,
    pub(crate) bare: Vec<Symbol>,
    pub(crate) container_defs: Vec<Symbol>,
    pub(crate) quiet_early: bool,
    pub(crate) quiet_late: bool,
}

/// Look up a symbol claim once for every consumer. The scoped lookup
/// short-circuits on hit; misses fetch bare and container defs together
/// so shaping never re-queries.
pub(crate) fn lookup_symbol(
    conn: &Connection,
    name: &str,
    container: Option<&str>,
    ctx: &SweepCtx,
) -> Result<SymbolLookup> {
    let mut out = SymbolLookup {
        scoped: Vec::new(),
        bare: Vec::new(),
        container_defs: Vec::new(),
        quiet_early: false,
        quiet_late: false,
    };
    if name.chars().count() < 2 {
        out.quiet_early = true;
        return Ok(out);
    }
    if let Some(want) = container {
        if want == "self" || want == "this" {
            out.quiet_early = true;
            return Ok(out);
        }
        if ctx.tables.contains(&want.to_lowercase())
            && ctx.columns.contains(&name.to_lowercase())
        {
            out.quiet_early = true;
            return Ok(out);
        }
        out.scoped = queries::find_definition_by_container(conn, want, name)?;
        if out.scoped.is_empty() {
            if scan::is_reserved_word(name) {
                return Ok(out);
            }
            // Both stay queried: shaping needs container defs even when
            // the bare member exists elsewhere (`db.get` with top-level
            // `get` still reads as a receiver sample).
            out.bare = queries::find_definition(conn, name)?;
            out.container_defs = queries::find_definition(conn, want)?;
        }
        return Ok(out);
    }
    out.bare = queries::find_definition(conn, name)?;
    if out.bare.is_empty() {
        let lower = name.to_lowercase();
        out.quiet_late = scan::is_reserved_word(name)
            || ctx.tables.contains(&lower)
            || ctx.columns.contains(&lower)
            || ctx.meta_keys.contains(&lower)
            || ctx.external.contains(&lower)
            || queries::symbol_names_matching_nocase(conn, name)?.len() == 1
            || is_keel_env(name)
            || ctx.root_name.as_deref() == Some(lower.as_str())
            || ctx.mcp_tools.contains(name)
            || ctx.cli_verbs.contains(name);
    }
    Ok(out)
}

/// Path resolution shared by drift findings and ADR links: a display
/// path on hit (repo-relative when under the root), a quiet skip
/// (MCP/runtime/escape claims, or ambiguous multi-hits), else a miss.
pub(crate) enum PathResolution {
    Hit(String),
    Skip { ambiguous: bool },
    Miss,
}

/// Resolve a path claim through the full ladder: `:LINE` suffixes
/// strip, `path::symbol` hybrids resolve both halves, then doc dir,
/// root escape, MCP/runtime names, and unique basename/suffix.
pub(crate) fn resolve_path(
    conn: &Connection,
    root: &Path,
    doc: &Path,
    raw: &str,
    repo: &RepoFiles,
) -> Result<PathResolution> {
    let normalized = raw.replace('\\', "/");
    let target = strip_line_suffix(&normalized);
    if let Some((head, tail)) = target.split_once("::") {
        if !head.is_empty() && head.contains('/') {
            if let scan::ClaimKind::Symbol { name: tail_name, .. } = scan::classify(tail) {
                if !queries::find_definition(conn, &tail_name)?.is_empty() {
                    if let Some(hit) = hit_display(root, doc, repo, head) {
                        return Ok(PathResolution::Hit(hit));
                    }
                }
            }
        }
    }
    if let Some(hit) = hit_display(root, doc, repo, target) {
        return Ok(PathResolution::Hit(hit));
    }
    if let Some(parent) = doc.parent() {
        if escapes_root(parent, target, root) {
            return Ok(PathResolution::Skip { ambiguous: false });
        }
    }
    let trimmed = target.trim_end_matches('/');
    if scan::MCP_PATHS.contains(&trimmed) {
        return Ok(PathResolution::Skip { ambiguous: false });
    }
    if let Some(base) = trimmed.rsplit('/').next() {
        if scan::RUNTIME_FILES.contains(&base) {
            return Ok(PathResolution::Skip { ambiguous: false });
        }
    }
    let hits = if trimmed.contains('/') {
        repo.suffix_hits(trimmed)
    } else {
        repo.basename_hits(trimmed)
    };
    if hits.is_empty() {
        return Ok(PathResolution::Miss);
    }
    if hits.len() > 1 {
        return Ok(PathResolution::Skip { ambiguous: true });
    }
    Ok(PathResolution::Hit(hits[0].to_string()))
}

/// Display path for a claim that hits the filesystem or the repo list:
/// repo-relative when under the root, absolute otherwise. Doc dir and
/// root existence win over basename/suffix matches.
fn hit_display(root: &Path, doc: &Path, repo: &RepoFiles, target: &str) -> Option<String> {
    if let Some(parent) = doc.parent() {
        let joined = parent.join(target);
        if joined.exists() {
            return Some(display_path(root, &joined));
        }
    }
    if root.join(target).exists() {
        return Some(crate::graph::target::clean_path(target));
    }
    let trimmed = target.trim_end_matches('/');
    let hits = if trimmed.contains('/') {
        repo.suffix_hits(trimmed)
    } else {
        repo.basename_hits(trimmed)
    };
    if hits.len() == 1 {
        return Some(hits[0].to_string());
    }
    None
}

/// Render an absolute path repo-relative when it sits under `root`.
fn display_path(root: &Path, abs: &Path) -> String {
    match abs.strip_prefix(root) {
        Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
        Err(_) => abs.to_string_lossy().replace('\\', "/"),
    }
}

/// Per-document check state: the shared sweep inputs plus the doc's
/// identity and the sweep's ambiguity counter.
struct DocCheck<'a> {
    conn: &'a Connection,
    rel: &'a str,
    root: &'a Path,
    doc: &'a Path,
    ctx: &'a SweepCtx,
    ambiguous_paths: &'a mut usize,
}

impl DocCheck<'_> {
    /// Check a symbol claim: the name must be defined; a member qualifier
    /// must name the real container; an adjacent kind word on a unique
    /// definition must agree with the indexed kind.
    ///
    /// Misses resolve through a ladder before becoming findings: reserved
    /// words (keywords, shorthands, toolchain), schema names, external
    /// imports, unique case-insensitive matches, `KEEL_*` config, the
    /// project name, and live MCP/CLI names. A real definition always wins
    /// over every ladder rung.
    fn symbol_claim(
        &self,
        span: &scan::ClaimSpan,
        name: &str,
        container: Option<&str>,
    ) -> Result<Option<DriftFinding>> {
        let lookup = lookup_symbol(self.conn, name, container, self.ctx)?;
        if lookup.quiet_early {
            return Ok(None);
        }
        // Member-qualified form first: `Widget.show` claims `show` on `Widget`.
        if let Some(want) = container {
            if !lookup.scoped.is_empty() {
                return check_kind(self.rel, span, &lookup.scoped);
            }
            // A referenced-but-undefined container is a value, not a type
            // (`db.get()`), so the claim is a receiver sample without an
            // attributable type — quiet, like `self.save`. Checked before
            // the member lookup: the member existing elsewhere doesn't make
            // the receiver attributable.
            if lookup.container_defs.is_empty()
                && queries::name_is_referenced(self.conn, want)?
            {
                return Ok(None);
            }
            if scan::is_reserved_word(name) {
                return Ok(None);
            }
            if !lookup.bare.is_empty() {
                let mut holders: Vec<String> =
                    lookup.bare.iter().map(|s| s.container.clone()).collect();
                holders.sort();
                holders.dedup();
                holders.retain(|c| !c.is_empty());
                let suggestion = if holders.is_empty() {
                    format!("`{name}` is only defined at top level, not on `{want}`")
                } else {
                    format!("`{name}` lives on {}, not on `{want}`", holders.join(", "))
                };
                return Ok(Some(DriftFinding {
                    doc: self.rel.to_string(),
                    line: span.line,
                    claim: span.text.clone(),
                    verdict: DriftVerdict::Unsupported,
                    evidence: format!("no member `{name}` on `{want}`"),
                    suggestion: Some(suggestion),
                }));
            }
            // Neither the member nor anything under that container is
            // defined (receiver samples already returned above). A container
            // unknown everywhere is illustration-shaped (`Type.member`) —
            // unsupported without noise suggestions. A known container keeps
            // rename suggestions (`Widget.bogus` → `show`).
            if lookup.container_defs.is_empty() {
                return Ok(Some(DriftFinding {
                    doc: self.rel.to_string(),
                    line: span.line,
                    claim: span.text.clone(),
                    verdict: DriftVerdict::Unsupported,
                    evidence: format!("neither `{want}` nor `{name}` found in the index"),
                    suggestion: None,
                }));
            }
            return missing_symbol(self.conn, self.rel, span);
        }
        if !lookup.bare.is_empty() {
            return check_kind(self.rel, span, &lookup.bare);
        }
        if lookup.quiet_late {
            return Ok(None);
        }
        missing_symbol(self.conn, self.rel, span)
    }

    /// Check a path claim against the filesystem (files and dirs alike):
    /// the doc dir first, then the project root, then a unique basename
    /// (single-segment claims like `daemon.rs`) or suffix (multi-segment
    /// like `languages/mod.rs`) anywhere in the repo. Unique hits resolve
    /// the claim; multiple hits are ambiguous fragments that skip quietly
    /// (counted for the sweep note); zero hits everywhere is unsupported.
    ///
    /// Before the ladder: `:LINE[:COL]` suffixes strip, `path::symbol`
    /// hybrids resolve both halves, MCP protocol paths and runtime
    /// basenames (`watch.log`) skip, and doc-relative claims escaping the
    /// project root read as illustrative (`../../x.py`).
    fn path_claim(
        &mut self,
        span: &scan::ClaimSpan,
        raw: &str,
    ) -> Result<Option<DriftFinding>> {
        match resolve_path(self.conn, self.root, self.doc, raw, &self.ctx.repo)? {
            PathResolution::Hit(_) => Ok(None),
            PathResolution::Skip { ambiguous } => {
                if ambiguous {
                    *self.ambiguous_paths += 1;
                }
                Ok(None)
            }
            PathResolution::Miss => Ok(Some(DriftFinding {
                doc: self.rel.to_string(),
                line: span.line,
                claim: raw.to_string(),
                verdict: DriftVerdict::Unsupported,
                evidence: format!("linked path `{raw}` not found in the repo"),
                suggestion: None,
            })),
        }
    }
}

/// True for Keel's own config namespace: `KEEL_` plus uppercase,
/// digits, and underscores (`KEEL_INDEX_DB`). Other all-caps names
/// still check (removed constants are real drift).
fn is_keel_env(name: &str) -> bool {
    name.starts_with("KEEL_")
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// A missing symbol is potentially stale when close matches exist (a
/// possible rename) and unsupported when nothing resembles it. Names
/// the code uses but never defines (`Sync`, `Vec`) are live external
/// vocabulary, not drift, and resolve quietly.
fn missing_symbol(
    conn: &Connection,
    rel: &str,
    span: &scan::ClaimSpan,
) -> Result<Option<DriftFinding>> {
    let bare = bare_lookup(&span.text);
    if queries::name_is_referenced(conn, &bare)? {
        return Ok(None);
    }
    let suggestions = suggest_names(conn, &bare, SUGGESTION_LIMIT).unwrap_or_default();
    if suggestions.is_empty() {
        Ok(Some(DriftFinding {
            doc: rel.to_string(),
            line: span.line,
            claim: span.text.clone(),
            verdict: DriftVerdict::Unsupported,
            evidence: format!(
                "no symbol named `{bare}`; may be removed, external, or never indexed"
            ),
            suggestion: None,
        }))
    } else {
        Ok(Some(DriftFinding {
            doc: rel.to_string(),
            line: span.line,
            claim: span.text.clone(),
            verdict: DriftVerdict::PotentiallyStale,
            evidence: format!("no symbol named `{bare}`"),
            suggestion: Some(format!(
                "did you mean {}? (possible rename)",
                suggestions
                    .iter()
                    .map(|s| format!("`{s}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }))
    }
}

/// Last identifier segment of a span (`show` for `Widget.show` and
/// `m::show`); falls back to the raw text when unparseable.
fn bare_lookup(span_text: &str) -> String {
    let mut text = span_text.trim();
    while let Some(stripped) = text.strip_suffix("()") {
        text = stripped.trim_end();
    }
    match text.rsplit(['.', ':']).next() {
        Some(last) if !last.is_empty() => last.to_string(),
        _ => text.to_string(),
    }
}

/// Kind agreement: an adjacent kind word on a uniquely-defined symbol
/// must match the indexed kind, else the doc contradicts the index.
/// Missing kind words and ambiguous (multi-defined) symbols make no
/// kind claim and pass quietly. `impl` rows are excluded from the
/// uniqueness count: they are definition-adjacent bookkeeping (every
/// `impl Widget` reuses the type name), never what a doc kind-claims.
fn check_kind(
    rel: &str,
    span: &scan::ClaimSpan,
    defs: &[Symbol],
) -> Result<Option<DriftFinding>> {
    let claimed = span.kind_word.as_deref().and_then(doc_kind);
    let Some(want) = claimed else {
        return Ok(None);
    };
    let kinds: Vec<&Symbol> = defs
        .iter()
        .filter(|d| d.kind != SymbolKind::Impl)
        .collect();
    if kinds.len() != 1 {
        return Ok(None);
    }
    let actual = kinds[0].kind.as_db().to_lowercase();
    if actual == want {
        return Ok(None);
    }
    let at = format!(
        "{}:{}",
        kinds[0].file.to_string_lossy().replace('\\', "/"),
        kinds[0].start_line
    );
    Ok(Some(DriftFinding {
        doc: rel.to_string(),
        line: span.line,
        claim: span.text.clone(),
        verdict: DriftVerdict::Contradicted,
        evidence: format!(
            "doc calls `{}` {} {}; index defines {} {} at {at}",
            span.text,
            article_for(want),
            want,
            article_for(&actual),
            actual
        ),
        suggestion: Some(format!("defined as {actual}")),
    }))
}

/// Indefinite article for a kind label.
fn article_for(kind: &str) -> &'static str {
    match kind.chars().next().unwrap_or('x') {
        'a' | 'e' | 'i' | 'o' | 'u' => "an",
        _ => "a",
    }
}

/// Check generated wiki pages against their manifest pins. Stale pages
/// (inputs changed) and missing pages (deleted since generation) become
/// page-level findings (`line` 0); current, manual, and orphaned pages
/// are lifecycle states, not drift. Returns pages checked.
fn check_wiki(
    conn: &Connection,
    root: &Path,
    findings: &mut Vec<DriftFinding>,
    notes: &mut Vec<String>,
) -> Result<usize> {
    use crate::wiki::PageState;
    let wiki_dir = root.join("docs").join("keel");
    // No manifest means no wiki to drift against: `status_report` would
    // call every live page missing, so skip before calling it.
    if !wiki_dir.join(crate::wiki::MANIFEST_FILE).is_file() {
        if wiki_dir.is_dir() {
            notes.push(
                "docs/keel exists without a wiki manifest; run `keel wiki generate`".to_string(),
            );
        }
        return Ok(0);
    }
    let status = crate::wiki::status_report(conn, &wiki_dir)?;
    let mut checked = 0usize;
    for page in &status.pages {
        let rel = format!("docs/keel/{}", page.path);
        match page.state {
            PageState::Current | PageState::Manual | PageState::Orphaned => {
                checked += 1;
            }
            PageState::Stale => {
                checked += 1;
                findings.push(DriftFinding {
                    doc: rel,
                    line: 0,
                    claim: page.path.clone(),
                    verdict: DriftVerdict::PotentiallyStale,
                    evidence: format!("wiki inputs changed: {}", page.detail),
                    suggestion: Some("run `keel wiki update` and review the diff".to_string()),
                });
            }
            PageState::Missing => {
                checked += 1;
                findings.push(DriftFinding {
                    doc: rel,
                    line: 0,
                    claim: page.path.clone(),
                    verdict: DriftVerdict::Unsupported,
                    evidence: format!("wiki page missing: {}", page.detail),
                    suggestion: Some("run `keel wiki update` to regenerate".to_string()),
                });
            }
        }
    }
    Ok(checked)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_scope_rules() {
        assert!(covers_doc(Path::new("docs/arch.md")));
        assert!(covers_doc(Path::new("docs/nested/deep.md")));
        assert!(covers_doc(Path::new("AGENTS.md")));
        assert!(covers_doc(Path::new("nested/CLAUDE.md")));
        assert!(covers_doc(Path::new("notes/adr-003-auth.md")));
        assert!(covers_doc(Path::new("docs/adr/001.md")));
        assert!(covers_doc(Path::new("team/decisions/x.md")));
        assert!(!covers_doc(Path::new("docs/keel/index.md")));
        assert!(!covers_doc(Path::new("README.md")));
        assert!(!covers_doc(Path::new("src/lib.rs")));
        assert!(!covers_doc(Path::new("notes/random.md")));
    }

    #[test]
    fn verdict_labels() {
        assert_eq!(DriftVerdict::PotentiallyStale.as_str(), "potentially stale");
        assert_eq!(DriftVerdict::Unsupported.as_str(), "unsupported");
        assert_eq!(DriftVerdict::Contradicted.as_str(), "contradicted");
    }

    #[test]
    fn kind_vocab() {
        assert_eq!(doc_kind("trait"), Some("trait"));
        assert_eq!(doc_kind("fn"), Some("function"));
        assert_eq!(doc_kind("static"), Some("const"));
        assert_eq!(doc_kind("thing"), None);
    }

    #[test]
    fn bare_lookup_strips() {
        assert_eq!(bare_lookup("serve"), "serve");
        assert_eq!(bare_lookup("Widget.show"), "show");
        assert_eq!(bare_lookup("m::show"), "show");
        assert_eq!(bare_lookup("save()"), "save");
    }

    #[test]
    fn line_suffixes_strip_but_ports_survive() {
        assert_eq!(strip_line_suffix("a/b.rs:12"), "a/b.rs");
        assert_eq!(strip_line_suffix("a/b.rs:12:34"), "a/b.rs");
        assert_eq!(strip_line_suffix("C:/x"), "C:/x");
        assert_eq!(strip_line_suffix("host:port"), "host:port");
        assert_eq!(strip_line_suffix("a.rs:12b"), "a.rs:12b");
    }

    #[test]
    fn root_escape_detection() {
        let root = Path::new("/repo");
        assert!(escapes_root(Path::new("/repo/docs"), "../../x.py", root));
        assert!(!escapes_root(Path::new("/repo/docs"), "../src/x.py", root));
        assert!(!escapes_root(Path::new("/repo/docs"), "x.md", root));
        // Relative bases can't be judged: check normally.
        assert!(!escapes_root(Path::new("docs"), "../../x.py", root));
    }

    #[test]
    fn keel_env_namespace() {
        assert!(is_keel_env("KEEL_INDEX_DB"));
        assert!(is_keel_env("KEEL_HOME"));
        assert!(!is_keel_env("KEEL_HOME_DIRTY!"));
        assert!(!is_keel_env("MAX_RESULT_LIMIT"));
        assert!(!is_keel_env("keel_home"));
    }

}
