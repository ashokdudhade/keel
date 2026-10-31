//! Wiki page collection: gather inputs, render pages, hash pins.
//!
//! Collection is deterministic (sorted files, symbols, and records) and
//! read-only; writing lives in [`generate_all`] (full) and `update`
//! (stale-only), with freshness diffing in `status_report`.

use crate::db::queries;
use crate::error::Result;
use crate::graph::types::ReferenceKind;
use crate::knowledge::{self, KnowledgeType};
use crate::wiki::render::{
    self, FlowData, IndexData, KnowledgeRow, ModuleData, ModuleEntry, SymbolRow, MAX_ARCH_EDGES,
    MAX_ARCH_EXTERNALS, MAX_FLOW_EDGES, MAX_MODULE_DEPS, MAX_MODULE_SYMBOLS,
};
use crate::wiki::{WikiReport, MANIFEST_FILE, WikiManifest, PinnedPage};
use hex::encode as hex_encode;
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// One generated page with its pinned inputs.
pub struct GeneratedPage {
    /// Path relative to the wiki root (`modules/src-auth.md`).
    pub rel_path: String,
    /// Managed-region id (the relative path; unique per page).
    pub id: String,
    /// Rendered inner markdown (without markers).
    pub inner: String,
    /// Input files → content hashes.
    pub files: BTreeMap<String, String>,
    /// Input knowledge records → `status|updated_at`.
    pub knowledge: BTreeMap<String, String>,
}

impl GeneratedPage {
    /// Canonical inputs hash: sha256 over sorted `f:`/`k:` lines.
    pub fn inputs_hash(&self) -> String {
        let mut canonical = String::new();
        for (path, hash) in &self.files {
            canonical.push_str(&format!("f:{path}:{hash}\n"));
        }
        for (id, state) in &self.knowledge {
            canonical.push_str(&format!("k:{id}:{state}\n"));
        }
        hex_encode(Sha256::digest(canonical.as_bytes()))
    }

    /// Short pin for page footers.
    pub fn inputs_short(&self) -> String {
        self.inputs_hash().chars().take(12).collect()
    }
}

/// Collect every wiki page deterministically: index, architecture, one
/// module + flow page per source directory, flows index, decisions index.
pub fn collect_pages(conn: &Connection) -> Result<Vec<GeneratedPage>> {
    let mut files = queries::indexed_files(conn)?;
    files.sort();
    let hashes = queries::existing_hashes(conn)?;
    let records = knowledge::list_records(conn)?;
    let dirs = group_dirs(&files);
    let slugs = slugify(dirs.keys().cloned().collect());

    let mut pages = Vec::new();
    pages.push(index_page(conn, &files, &hashes, &records, &dirs, &slugs)?);
    pages.push(architecture_page(conn, &files, &hashes)?);
    for (dir, dir_files) in &dirs {
        let slug = &slugs[dir];
        pages.push(module_page(conn, dir, dir_files, &hashes, slug)?);
        pages.push(flow_page(conn, dir, dir_files, &hashes, slug)?);
    }
    pages.push(flows_index_page(&dirs, &slugs, &hashes));
    pages.push(decisions_page(conn, &records)?);
    pages.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(pages)
}

/// Group indexed files by parent directory (`""` for the project root).
fn group_dirs(files: &[String]) -> BTreeMap<String, Vec<String>> {
    let mut dirs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for file in files {
        let dir = parent_dir(file);
        dirs.entry(dir).or_default().push(file.clone());
    }
    for dir_files in dirs.values_mut() {
        dir_files.sort();
    }
    dirs
}

/// Parent directory with both separators tolerated (`""` for root files).
fn parent_dir(path: &str) -> String {
    match path.rsplit_once(['/', '\\']) {
        Some((dir, _)) => dir.to_string(),
        None => String::new(),
    }
}

/// Display label for a directory (`(root)` for the project root).
fn display_dir(dir: &str) -> String {
    if dir.is_empty() {
        "(root)".to_string()
    } else {
        dir.to_string()
    }
}

/// Slugify directories for page filenames, disambiguating collisions
/// deterministically (`a/b` vs `a-b` → `a-b`, `a-b-2`).
fn slugify(dirs: Vec<String>) -> BTreeMap<String, String> {
    let mut ordered = dirs;
    ordered.sort();
    let mut used: BTreeSet<String> = BTreeSet::new();
    let mut slugs = BTreeMap::new();
    for dir in ordered {
        let base = if dir.is_empty() {
            "root".to_string()
        } else {
            dir.replace(['/', '\\'], "-")
        };
        let mut slug = base.clone();
        let mut n = 2;
        while used.contains(&slug) {
            slug = format!("{base}-{n}");
            n += 1;
        }
        used.insert(slug.clone());
        slugs.insert(dir, slug);
    }
    slugs
}

/// Language label for an indexed path, by extension.
fn language_label(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "rs" => "Rust",
        "ts" | "tsx" | "mts" | "cts" => "TypeScript",
        "js" | "jsx" | "mjs" | "cjs" => "JavaScript",
        "py" | "pyi" => "Python",
        "go" => "Go",
        _ => "Other",
    }
}

/// File stem (no directories, no final extension) for flow caller labels.
fn file_stem(path: &str) -> String {
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file).to_string()
}

/// Knowledge state pin: status + writer stamp (every `put` bumps the stamp,
/// so content and evidence edits are covered).
fn record_state(record: &knowledge::KnowledgeRecord) -> String {
    format!("{}|{}", record.status.as_db(), record.updated_at)
}

/// Live freshness verdict for one record id (lenient default on vanishing).
fn freshness_of(conn: &Connection, id: &str) -> (bool, Vec<String>) {
    knowledge::check_one(conn, id)
        .ok()
        .flatten()
        .map(|f| (f.fresh, f.reasons))
        .unwrap_or((true, Vec::new()))
}

fn knowledge_row(conn: &Connection, record: &knowledge::KnowledgeRecord) -> KnowledgeRow {
    let (fresh, _) = freshness_of(conn, &record.id);
    KnowledgeRow {
        id: record.id.clone(),
        title: record.title.clone(),
        status: record.status.as_db().to_string(),
        origin: record.origin.as_db(),
        fresh,
        evidence: record
            .evidence
            .iter()
            .map(|e| match &e.symbol {
                Some(symbol) => format!("{} ({symbol})", e.source_path),
                None => e.source_path.clone(),
            })
            .collect(),
    }
}

fn index_page(
    conn: &Connection,
    files: &[String],
    hashes: &std::collections::HashMap<String, String>,
    records: &[knowledge::KnowledgeRecord],
    dirs: &BTreeMap<String, Vec<String>>,
    slugs: &BTreeMap<String, String>,
) -> Result<GeneratedPage> {
    let mut symbols_total = 0usize;
    let mut languages: BTreeMap<&str, usize> = BTreeMap::new();
    for file in files {
        *languages.entry(language_label(file)).or_default() += 1;
        symbols_total += queries::symbols_in_file(conn, file)?.len();
    }
    let mut modules = Vec::new();
    for (dir, dir_files) in dirs {
        let mut symbols = 0usize;
        for file in dir_files {
            symbols += queries::symbols_in_file(conn, file)?.len();
        }
        modules.push(ModuleEntry {
            dir: display_dir(dir),
            slug: slugs[dir].clone(),
            files: dir_files.len(),
            symbols,
        });
    }
    let mut by_status: BTreeMap<String, usize> = BTreeMap::new();
    for record in records {
        *by_status.entry(record.status.as_db().to_string()).or_default() += 1;
    }
    // Index counts depend on everything: all files + all knowledge states.
    let mut page = GeneratedPage {
        rel_path: "index.md".to_string(),
        id: "index.md".to_string(),
        inner: String::new(),
        files: files
            .iter()
            .map(|f| (f.clone(), hashes.get(f).cloned().unwrap_or_default()))
            .collect(),
        knowledge: records
            .iter()
            .map(|r| (r.id.clone(), record_state(r)))
            .collect(),
    };
    page.inner = render::render_index(&IndexData {
        files: files.len(),
        symbols: symbols_total,
        languages: languages.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        modules,
        knowledge_total: records.len(),
        knowledge_by_status: by_status.into_iter().collect(),
        inputs_short: page.inputs_short(),
    });
    Ok(page)
}

fn architecture_page(
    conn: &Connection,
    files: &[String],
    hashes: &std::collections::HashMap<String, String>,
) -> Result<GeneratedPage> {
    // Directory-level edges from imports: module → first indexed file's dir.
    let mut edge_sites: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut externals: BTreeSet<String> = BTreeSet::new();
    for file in files {
        let from = display_dir(&parent_dir(file));
        let mut imports = queries::imports_for_file(conn, file)?;
        imports.sort();
        imports.dedup();
        for (module_path, _) in imports {
            match queries::files_for_module_path(conn, &module_path)? {
                hits if hits.is_empty() => {
                    externals.insert(module_path);
                }
                resolved => {
                    let to = display_dir(&parent_dir(&resolved[0]));
                    if to != from {
                        *edge_sites.entry((from.clone(), to)).or_default() += 1;
                    }
                }
            }
        }
    }
    let mut edges: Vec<((String, String), usize)> = edge_sites.into_iter().collect();
    edges.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let edge_total = edges.len();
    let capped: Vec<(String, String)> = edges
        .iter()
        .take(MAX_ARCH_EDGES)
        .map(|(e, _)| e.clone())
        .collect();
    let mut externals: Vec<String> = externals.into_iter().collect();
    externals.sort();
    let externals_total = externals.len();
    let externals_capped: Vec<String> = externals.into_iter().take(MAX_ARCH_EXTERNALS).collect();

    let mut page = GeneratedPage {
        rel_path: "architecture.md".to_string(),
        id: "architecture.md".to_string(),
        inner: String::new(),
        files: files
            .iter()
            .map(|f| (f.clone(), hashes.get(f).cloned().unwrap_or_default()))
            .collect(),
        knowledge: BTreeMap::new(),
    };
    page.inner = render::render_architecture(&crate::wiki::render::ArchData {
        edges: capped,
        edge_total,
        externals: externals_capped,
        externals_total,
        inputs_short: page.inputs_short(),
    });
    Ok(page)
}

fn module_page(
    conn: &Connection,
    dir: &str,
    dir_files: &[String],
    hashes: &std::collections::HashMap<String, String>,
    slug: &str,
) -> Result<GeneratedPage> {
    // Top-level symbols ranked by recorded reference count (name-based, like
    // liveness: same-named sites count for every same-named definition).
    // Members (container non-empty) are excluded from ranking: ubiquitous
    // field names (`id`, `conn`) would otherwise drown the module's API.
    let mut rows: Vec<SymbolRow> = Vec::new();
    let mut symbols_total = 0usize;
    for file in dir_files {
        for symbol in queries::symbols_in_file(conn, file)? {
            symbols_total += 1;
            if !symbol.container.is_empty() {
                continue;
            }
            let refs = queries::find_references(conn, &symbol.name)?.len();
            rows.push(SymbolRow {
                name: symbol.name,
                kind: symbol.kind.as_db(),
                path: file.clone(),
                line: symbol.start_line,
                refs,
            });
        }
    }
    rows.sort_by(|a, b| {
        b.refs
            .cmp(&a.refs)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.name.cmp(&b.name))
    });
    let ranked_total = rows.len();
    let symbols: Vec<SymbolRow> = rows.into_iter().take(MAX_MODULE_SYMBOLS).collect();

    let mut dependencies: BTreeSet<String> = BTreeSet::new();
    for file in dir_files {
        for (module_path, _) in queries::imports_for_file(conn, file)? {
            dependencies.insert(module_path);
        }
    }
    let mut dependencies: Vec<String> = dependencies.into_iter().collect();
    dependencies.sort();
    let dependencies_total = dependencies.len();
    let dependencies_capped: Vec<String> =
        dependencies.into_iter().take(MAX_MODULE_DEPS).collect();

    let mut modules: BTreeSet<String> = BTreeSet::new();
    for file in dir_files {
        for module in queries::module_paths_in_file(conn, file)? {
            if !module.is_empty() {
                modules.insert(module);
            }
        }
    }
    let mut dependents: BTreeSet<String> = BTreeSet::new();
    for module in &modules {
        for dep in crate::graph::deps::find_dependents(conn, module)? {
            dependents.insert(dep.module_path);
        }
    }
    let mut dependents: Vec<String> = dependents.into_iter().collect();
    dependents.sort();
    let dependents_total = dependents.len();
    let dependents_capped: Vec<String> = dependents.into_iter().take(MAX_MODULE_DEPS).collect();

    // Linked knowledge across the directory's files, merged by id.
    let mut linked: BTreeMap<String, knowledge::KnowledgeRecord> = BTreeMap::new();
    for file in dir_files {
        for record in knowledge::records_linked(conn, Some(file), None)? {
            linked.entry(record.id.clone()).or_insert(record);
        }
    }
    let knowledge_total = linked.len();
    let knowledge: Vec<KnowledgeRow> = linked
        .values()
        .take(crate::wiki::render::MAX_MODULE_KNOWLEDGE)
        .map(|r| knowledge_row(conn, r))
        .collect::<Vec<_>>();

    // Inputs: own files + files referencing the directory's symbols
    // (dependency closure for callers/dependents sections) + knowledge.
    let mut input_files: BTreeMap<String, String> = dir_files
        .iter()
        .map(|f| (f.clone(), hashes.get(f).cloned().unwrap_or_default()))
        .collect();
    for file in dir_files {
        for symbol in queries::symbols_in_file(conn, file)? {
            for reference in queries::find_references(conn, &symbol.name)? {
                let path = reference.file.to_string_lossy().into_owned();
                input_files
                    .entry(path.clone())
                    .or_insert_with(|| hashes.get(&path).cloned().unwrap_or_default());
            }
        }
    }
    let input_knowledge: BTreeMap<String, String> = linked
        .values()
        .map(|r| (r.id.clone(), record_state(r)))
        .collect();

    let rel_path = format!("modules/{slug}.md");
    let mut page = GeneratedPage {
        id: rel_path.clone(),
        rel_path,
        inner: String::new(),
        files: input_files,
        knowledge: input_knowledge,
    };
    page.inner = render::render_module(&ModuleData {
        dir: display_dir(dir),
        files: dir_files.to_vec(),
        symbols,
        ranked_total,
        symbols_total,
        dependencies: dependencies_capped,
        dependencies_total,
        dependents: dependents_capped,
        dependents_total,
        knowledge,
        knowledge_total,
        inputs_short: page.inputs_short(),
    });
    Ok(page)
}

fn flow_page(
    conn: &Connection,
    dir: &str,
    dir_files: &[String],
    hashes: &std::collections::HashMap<String, String>,
    slug: &str,
) -> Result<GeneratedPage> {
    // Static call edges from the directory's own references: caller label is
    // the enclosing container (or file stem), callee is the referenced name.
    let mut edge_sites: BTreeMap<(String, String), usize> = BTreeMap::new();
    for file in dir_files {
        for reference in queries::references_in_file(conn, file)? {
            if reference.kind != ReferenceKind::Call {
                continue;
            }
            let caller = if reference.container.is_empty() {
                file_stem(file)
            } else {
                reference.container.clone()
            };
            *edge_sites
                .entry((caller, reference.name.clone()))
                .or_default() += 1;
        }
    }
    let mut edges: Vec<((String, String), usize)> = edge_sites.into_iter().collect();
    edges.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let edge_total = edges.len();
    let capped: Vec<(String, String, usize)> = edges
        .iter()
        .take(MAX_FLOW_EDGES)
        .map(|((a, b), n)| (a.clone(), b.clone(), *n))
        .collect();

    let rel_path = format!("flows/{slug}.md");
    let mut page = GeneratedPage {
        id: rel_path.clone(),
        rel_path,
        inner: String::new(),
        files: dir_files
            .iter()
            .map(|f| (f.clone(), hashes.get(f).cloned().unwrap_or_default()))
            .collect(),
        knowledge: BTreeMap::new(),
    };
    page.inner = render::render_flow(&FlowData {
        dir: display_dir(dir),
        edges: capped,
        edge_total,
        inputs_short: page.inputs_short(),
    });
    Ok(page)
}

fn flows_index_page(
    dirs: &BTreeMap<String, Vec<String>>,
    slugs: &BTreeMap<String, String>,
    hashes: &std::collections::HashMap<String, String>,
) -> GeneratedPage {
    let entries: Vec<(String, String)> = dirs
        .keys()
        .map(|d| (display_dir(d), slugs[d].clone()))
        .collect();
    let files: BTreeMap<String, String> = hashes.clone().into_iter().collect();
    let mut page = GeneratedPage {
        rel_path: "flows/index.md".to_string(),
        id: "flows/index.md".to_string(),
        inner: String::new(),
        files,
        knowledge: BTreeMap::new(),
    };
    page.inner = render::render_flows_index(&entries, &page.inputs_short());
    page
}

fn decisions_page(
    conn: &Connection,
    records: &[knowledge::KnowledgeRecord],
) -> Result<GeneratedPage> {
    let mut rows = Vec::new();
    let mut knowledge = BTreeMap::new();
    for record in records {
        if record.record_type != KnowledgeType::Decision
            && record.record_type != KnowledgeType::Constraint
        {
            continue;
        }
        knowledge.insert(record.id.clone(), record_state(record));
        rows.push(knowledge_row(conn, record));
    }
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    let mut page = GeneratedPage {
        rel_path: "decisions/index.md".to_string(),
        id: "decisions/index.md".to_string(),
        inner: String::new(),
        files: BTreeMap::new(),
        knowledge,
    };
    page.inner = render::render_decisions(&crate::wiki::render::DecisionData {
        records: rows,
        kinds: vec!["decision".to_string(), "constraint".to_string()],
        inputs_short: page.inputs_short(),
    });
    Ok(page)
}

/// Write the full page tree: new files fenced, existing files region-merged
/// (human text preserved), orphaned generated-only pages deleted, manifest
/// refreshed. Pages whose rendered region is unchanged keep their bytes.
pub fn generate_all(conn: &Connection, out_dir: &Path) -> Result<WikiReport> {
    let pages = collect_pages(conn)?;
    write_pages(conn, out_dir, &pages, false)
}

/// Shared writer: `stale_only` skips pages whose pins still match (update).
pub(crate) fn write_pages(
    _conn: &Connection,
    out_dir: &Path,
    pages: &[GeneratedPage],
    stale_only: bool,
) -> Result<WikiReport> {
    let mut report = WikiReport::empty();
    let mut manifest = load_manifest(out_dir);
    let live: BTreeSet<&str> = pages.iter().map(|p| p.rel_path.as_str()).collect();

    for page in pages {
        let target = out_dir.join(&page.rel_path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|source| crate::error::KeelError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let pin_matches = manifest
            .pages
            .get(&page.rel_path)
            .is_some_and(|pinned| pinned.inputs == page.inputs_hash());
        if stale_only && pin_matches && target.exists() {
            report.skipped_current.push(page.rel_path.clone());
            continue;
        }
        if !target.exists() {
            write_file(&target, &render::fenced(&page.id, &page.inner))?;
            report.written.push(page.rel_path.clone());
        } else {
            let existing = read_file(&target)?;
            match crate::wiki::render::replace_region(&existing, &page.id, &page.inner) {
                Some(merged) if merged != existing => {
                    write_file(&target, &merged)?;
                    report.written.push(page.rel_path.clone());
                }
                Some(_) => {
                    report.skipped_current.push(page.rel_path.clone());
                }
                None => {
                    report.notes.push(format!(
                        "`{}` has no intact managed region; left untouched (manual).",
                        page.rel_path
                    ));
                    continue;
                }
            }
        }
        manifest.pages.insert(
            page.rel_path.clone(),
            PinnedPage {
                inputs: page.inputs_hash(),
                generated_at: knowledge::model::now_timestamp(),
            },
        );
    }

    // Orphaned manifest entries: inputs gone (directory removed).
    let mut orphans: Vec<String> = manifest
        .pages
        .keys()
        .filter(|p| !live.contains(p.as_str()))
        .cloned()
        .collect();
    orphans.sort();
    for orphan in orphans {
        let target = out_dir.join(&orphan);
        let human = target
            .exists()
            .then(|| read_file(&target))
            .transpose()?
            .and_then(|text| crate::wiki::render::human_content(&text, &orphan))
            .unwrap_or_default();
        manifest.pages.remove(&orphan);
        if !target.exists() {
            continue;
        }
        if human.is_empty() {
            std::fs::remove_file(&target).map_err(|source| crate::error::KeelError::Io {
                path: target.clone(),
                source,
            })?;
            report.deleted.push(orphan);
        } else {
            report.kept.push(format!(
                "`{orphan}` kept: directory removed but the page holds human content."
            ));
        }
    }

    write_manifest(out_dir, &manifest)?;
    report.written.sort();
    report.skipped_current.sort();
    report.deleted.sort();
    report.kept.sort();
    Ok(report)
}

/// Load the manifest, or an empty one when missing/unparseable. A corrupt
/// manifest degrades to "everything stale" rather than failing the command.
pub(crate) fn load_manifest(out_dir: &Path) -> WikiManifest {
    let path = out_dir.join(MANIFEST_FILE);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .filter(|manifest: &WikiManifest| manifest.version == crate::wiki::MANIFEST_VERSION)
        .unwrap_or_else(WikiManifest::empty)
}

fn write_manifest(out_dir: &Path, manifest: &WikiManifest) -> Result<()> {
    let path = out_dir.join(MANIFEST_FILE);
    let text = serde_json::to_string_pretty(manifest)
        .map_err(|e| crate::error::KeelError::Watch(e.to_string()))?;
    write_file(&path, &text)
}

fn read_file(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|source| crate::error::KeelError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn write_file(path: &Path, text: &str) -> Result<()> {
    std::fs::write(path, text).map_err(|source| crate::error::KeelError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;
    use crate::index;
    use crate::knowledge::{KnowledgeOrigin, KnowledgeRecord, KnowledgeType};
    use crate::wiki::update::{status_report, update_stale};
    use std::fs;
    use tempfile::tempdir;

    /// `src/auth.py` (login, validate), `src/pay.py` (charge),
    /// `tests/test_auth.py`, root `main.py` — four directories counting root.
    fn fixture() -> (tempfile::TempDir, Connection) {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("tests")).unwrap();
        fs::write(
            root.join("src/auth.py"),
            "def login(u):\n    return u\n\ndef validate(t):\n    return True\n",
        )
        .unwrap();
        fs::write(root.join("src/pay.py"), "def charge(a):\n    return a\n").unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub struct Config {\n    pub id: u32,\n}\n\npub fn build() -> Config {\n    Config { id: 1 }\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("tests/test_auth.py"),
            "from src.auth import login\n\ndef test_login():\n    assert login('x')\n",
        )
        .unwrap();
        fs::write(root.join("main.py"), "import src.pay\nprint('hi')\n").unwrap();
        let mut conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        index::index_repository(root, &mut conn).unwrap();
        (dir, conn)
    }

    fn seed_decision(conn: &mut Connection) {
        let record = KnowledgeRecord::new(
            "adr.jwt",
            KnowledgeType::Decision,
            "Use JWT",
            "Authenticate API clients with JWT",
            KnowledgeOrigin::Human,
        );
        knowledge::put_record(conn, &record).unwrap();
    }

    fn wiki_dir(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("docs").join("keel")
    }

    fn read_page(dir: &tempfile::TempDir, rel: &str) -> String {
        fs::read_to_string(wiki_dir(dir).join(rel)).unwrap()
    }

    #[test]
    fn generate_writes_tree_manifest_and_sections() {
        let (_dir, mut conn) = fixture();
        seed_decision(&mut conn);
        let out = wiki_dir(&_dir);
        let report = generate_all(&conn, &out).unwrap();
        assert!(report.deleted.is_empty());
        assert!(report.kept.is_empty());

        for expected in [
            "index.md",
            "architecture.md",
            "modules/src.md",
            "modules/tests.md",
            "modules/root.md",
            "flows/src.md",
            "flows/tests.md",
            "flows/root.md",
            "flows/index.md",
            "decisions/index.md",
            ".keel-wiki.json",
        ] {
            assert!(out.join(expected).exists(), "missing {expected}");
            assert!(
                report.written.iter().any(|p| p == expected) || expected == ".keel-wiki.json",
                "unreported {expected}: {:?}",
                report.written
            );
        }

        let index = read_page(&_dir, "index.md");
        assert!(index.contains("# Project wiki"));
        assert!(index.contains("keel:generated:start index.md"));
        assert!(index.contains("modules/src.md"));
        let module = read_page(&_dir, "modules/src.md");
        assert!(module.contains("login"));
        assert!(module.contains("charge"));
        assert!(module.contains("## Dependencies"));
        // Key symbols rank the top-level API: struct fields stay out even
        // though `id` is referenced (the name-based count over-attributes).
        let keys = module
            .split("## Key symbols")
            .nth(1)
            .unwrap()
            .split("## Dependencies")
            .next()
            .unwrap();
        assert!(!keys.contains("| field |"), "{keys}");
        assert!(keys.contains("`build`"), "{keys}");
        let decisions = read_page(&_dir, "decisions/index.md");
        assert!(decisions.contains("adr.jwt"));
        assert!(decisions.contains("human"));
        // tests/ has a call site (login('x')): mermaid graph expected.
        let flow = read_page(&_dir, "flows/tests.md");
        assert!(flow.contains("not observed runtime behavior"));
        assert!(flow.contains("```mermaid"));
        assert!(flow.contains("login"));
        // src/ has no call sites: honest empty section instead of a graph.
        let quiet = read_page(&_dir, "flows/src.md");
        assert!(quiet.contains("No call relationships recorded"));

        // Manifest pins every page.
        let manifest: WikiManifest =
            serde_json::from_str(&fs::read_to_string(out.join(".keel-wiki.json")).unwrap())
                .unwrap();
        assert_eq!(manifest.version, crate::wiki::MANIFEST_VERSION);
        assert_eq!(manifest.pages.len(), report.written.len());
    }

    #[test]
    fn generate_is_byte_stable_and_update_is_noop_when_current() {
        let (_dir, conn) = fixture();
        let out = wiki_dir(&_dir);
        generate_all(&conn, &out).unwrap();
        let before: BTreeMap<String, String> = collect_pages(&conn)
            .unwrap()
            .iter()
            .map(|p| (p.rel_path.clone(), fs::read_to_string(out.join(&p.rel_path)).unwrap()))
            .collect();

        // Second generate + update touch nothing.
        let report = generate_all(&conn, &out).unwrap();
        assert!(report.written.is_empty());
        assert!(!report.skipped_current.is_empty());
        let report = update_stale(&conn, &out).unwrap();
        assert!(report.written.is_empty());
        assert!(report.notes.iter().any(|n| n.contains("current")));

        for (rel, body) in &before {
            assert_eq!(&fs::read_to_string(out.join(rel)).unwrap(), body, "{rel}");
        }
        let status = status_report(&conn, &out).unwrap();
        assert!(status.pages.iter().all(|p| p.state == crate::wiki::PageState::Current));
    }

    #[test]
    fn update_rewrites_only_affected_pages() {
        let (dir, mut conn) = fixture();
        let out = wiki_dir(&dir);
        generate_all(&conn, &out).unwrap();
        let untouched_before = read_page(&dir, "modules/tests.md");

        // Edit only src/pay.py: src pages + globals go stale, tests stays.
        fs::write(dir.path().join("src/pay.py"), "def charge(a):\n    return 2 * a\n").unwrap();
        index::index_repository(dir.path(), &mut conn).unwrap();

        let status = status_report(&conn, &out).unwrap();
        let state_of = |rel: &str| {
            status.pages.iter().find(|p| p.path == rel).unwrap().state
        };
        assert_eq!(state_of("modules/src.md"), crate::wiki::PageState::Stale);
        assert_eq!(state_of("flows/src.md"), crate::wiki::PageState::Stale);
        assert_eq!(state_of("index.md"), crate::wiki::PageState::Stale);
        assert_eq!(state_of("architecture.md"), crate::wiki::PageState::Stale);
        assert_eq!(state_of("modules/tests.md"), crate::wiki::PageState::Current);

        let report = update_stale(&conn, &out).unwrap();
        assert!(report.written.contains(&"modules/src.md".to_string()));
        assert!(report.skipped_current.contains(&"modules/tests.md".to_string()));
        assert_eq!(read_page(&dir, "modules/tests.md"), untouched_before);
        assert!(read_page(&dir, "modules/src.md").contains("charge"));
    }

    #[test]
    fn human_edits_survive_and_manual_pages_are_untouched() {
        let (dir, mut conn) = fixture();
        let out = wiki_dir(&dir);
        generate_all(&conn, &out).unwrap();

        // Human note outside the markers survives a real rewrite: edit a
        // source file so the region content actually changes.
        let target = out.join("modules/src.md");
        let existing = fs::read_to_string(&target).unwrap();
        fs::write(&target, format!("{existing}\n## My notes\n\nKeep me.\n")).unwrap();
        fs::write(
            dir.path().join("src/pay.py"),
            "def charge(a):\n    return a\n\ndef refund(a):\n    return -a\n",
        )
        .unwrap();
        index::index_repository(dir.path(), &mut conn).unwrap();
        let report = update_stale(&conn, &out).unwrap();
        assert!(report.written.contains(&"modules/src.md".to_string()));
        let merged = fs::read_to_string(&target).unwrap();
        assert!(merged.contains("Keep me."));
        assert!(merged.contains("keel:generated:start modules/src.md"));
        assert!(merged.contains("refund"));

        // A page with stripped markers is manual: status flags, update skips.
        let manual_target = out.join("modules/tests.md");
        fs::write(&manual_target, "# Hand-written\n\nAll mine.\n").unwrap();
        let status = status_report(&conn, &out).unwrap();
        let state = status.pages.iter().find(|p| p.path == "modules/tests.md").unwrap();
        assert_eq!(state.state, crate::wiki::PageState::Manual);
        update_stale(&conn, &out).unwrap();
        assert_eq!(
            fs::read_to_string(&manual_target).unwrap(),
            "# Hand-written\n\nAll mine.\n"
        );
    }

    #[test]
    fn orphaned_pages_delete_only_when_generated_only() {
        let (dir, mut conn) = fixture();
        let out = wiki_dir(&dir);
        generate_all(&conn, &out).unwrap();

        // Human content on the tests module page before its dir vanishes.
        let target = out.join("modules/tests.md");
        let existing = fs::read_to_string(&target).unwrap();
        fs::write(&target, format!("{existing}\nHuman paragraph.\n")).unwrap();

        fs::remove_file(dir.path().join("tests/test_auth.py")).unwrap();
        index::index_repository(dir.path(), &mut conn).unwrap();

        let status = status_report(&conn, &out).unwrap();
        assert!(status.pages.iter().any(|p| p.path == "modules/tests.md"
            && p.state == crate::wiki::PageState::Orphaned));

        let report = update_stale(&conn, &out).unwrap();
        // Generated-only flows page deleted; human-bearing module page kept.
        assert!(report.deleted.contains(&"flows/tests.md".to_string()));
        assert!(!out.join("flows/tests.md").exists());
        assert!(report.kept.iter().any(|k| k.contains("modules/tests.md")));
        assert!(out.join("modules/tests.md").exists());
    }

    #[test]
    fn slug_collisions_disambiguate_deterministically() {
        let map = slugify(vec!["a/b".to_string(), "a-b".to_string(), String::new()]);
        assert_eq!(map["a-b"], "a-b");
        assert_eq!(map["a/b"], "a-b-2");
        assert_eq!(map[""], "root");
    }

    #[test]
    fn missing_files_report_as_missing() {
        let (_dir, conn) = fixture();
        let out = wiki_dir(&_dir);
        generate_all(&conn, &out).unwrap();
        fs::remove_file(out.join("index.md")).unwrap();
        let status = status_report(&conn, &out).unwrap();
        let state = status.pages.iter().find(|p| p.path == "index.md").unwrap();
        assert_eq!(state.state, crate::wiki::PageState::Missing);
        // Update restores the missing page.
        let report = update_stale(&conn, &out).unwrap();
        assert!(report.written.contains(&"index.md".to_string()));
    }
}
