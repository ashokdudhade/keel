//! Explanation assembly: resolve the target, gather evidence, pin hashes.

use crate::api::{DependencyDto, ImplDto, ReferenceDto, SymbolDto};
use crate::context::package::{excerpt_from, SourceReader};
use crate::context::SYMBOL_EXCERPT_LINES;
use crate::db::queries;
use crate::error::Result;
use crate::explain::{
    DisambiguationCandidate, ExplainedFile, ExplainedModule, Explanation, ExplainOptions,
    LinkedKnowledge, MAX_EXPLAIN_CALLERS, MAX_EXPLAIN_KNOWLEDGE, MAX_EXPLAIN_SYMBOLS,
};
use crate::facade;
use crate::graph::deps::{self, Dependency};
use crate::graph::target::resolve_file_target;
use crate::knowledge;
use rusqlite::Connection;
use std::collections::{BTreeMap, BTreeSet};

/// Explain one symbol: definition, excerpt, callers, dependencies, linked
/// knowledge. Zero hits yield a miss package; several yield disambiguation
/// candidates (pass `module` or a qualified name to narrow).
pub fn explain_symbol(
    conn: &Connection,
    name: &str,
    module: Option<&str>,
    options: &ExplainOptions,
) -> Result<Explanation> {
    let mut package = Explanation::empty(name, "symbol");
    let qr = facade::definition_with_meta_opts(conn, name, module)?;
    package.notes.extend(qr.notes);
    if qr.results.is_empty() {
        return Ok(package);
    }
    if qr.results.len() > 1 {
        package.disambiguation = qr
            .results
            .iter()
            .map(|d| DisambiguationCandidate {
                name: d.name.clone(),
                module_path: d.module_path.clone(),
                path: d.file.to_string_lossy().into_owned(),
                start_line: d.start_line,
            })
            .collect();
        return Ok(package);
    }
    let def = &qr.results[0];
    let def_path = def.file.to_string_lossy().into_owned();
    package.definition = Some(SymbolDto::from(def));

    let mut reader = SourceReader::new(options.source_root.clone());
    match reader.lines(&def_path).and_then(|lines| {
        excerpt_from(def.start_line, lines, SYMBOL_EXCERPT_LINES)
    }) {
        Some(excerpt) => package.excerpt = Some(excerpt),
        None if options.source_root.is_none() => package
            .notes
            .push("No source root: excerpt omitted.".to_string()),
        None => package.notes.push(format!(
            "Source unreadable for excerpt: {def_path} (citations still apply)."
        )),
    }

    // Callers scoped to the definition's module when known: the most precise
    // attribution available, with tier notes carried over honestly.
    let module_filter = if def.module_path.is_empty() {
        None
    } else {
        Some(def.module_path.as_str())
    };
    let callers = facade::callers_with_meta_opts(conn, &def.name, module_filter)?;
    package.notes.extend(callers.notes);
    package.callers_total = callers.results.len();
    if package.callers_total > MAX_EXPLAIN_CALLERS {
        package.notes.push(format!(
            "Showing {MAX_EXPLAIN_CALLERS} of {} callers.",
            package.callers_total
        ));
    }
    package.callers = callers
        .results
        .iter()
        .take(MAX_EXPLAIN_CALLERS)
        .map(ReferenceDto::from)
        .collect();

    package.dependencies = deps::find_dependencies(conn, &def.name)?
        .iter()
        .map(DependencyDto::from)
        .collect();
    package.dependents = deps::find_dependents(conn, &def.name)?
        .iter()
        .map(DependencyDto::from)
        .collect();
    // Raw lookup, not the trust-enveloped facade call: for non-traits this is
    // simply empty, and a "no implementations" miss note would be noise.
    package.implementations = queries::find_implementations(conn, &def.name)?
        .iter()
        .map(ImplDto::from)
        .collect();

    attach_linked_knowledge(conn, &mut package, None, Some(&def.name))?;

    // Pins cover the package content: the definition file plus shown callers.
    let hashes = queries::existing_hashes(conn)?;
    if let Some(hash) = hashes.get(&def_path) {
        package.source_hashes.insert(def_path, hash.clone());
    }
    for caller in &package.callers {
        if let Some(hash) = hashes.get(&caller.file) {
            package
                .source_hashes
                .entry(caller.file.clone())
                .or_insert_with(|| hash.clone());
        }
    }
    Ok(package)
}

/// Explain one file: outline, imports, dependents, linked knowledge.
pub fn explain_file(conn: &Connection, path: &str) -> Result<Explanation> {
    let mut package = Explanation::empty(path, "file");
    let Some(resolved) = resolve_file_target(conn, path)? else {
        package.notes.push(format!("No indexed file matches `{path}`."));
        return Ok(package);
    };
    let outline = facade::outline_with_meta(conn, &resolved)?;
    package.notes.extend(outline.notes);
    let hashes = queries::existing_hashes(conn)?;
    package.file = Some(ExplainedFile {
        path: resolved.clone(),
        content_hash: hashes.get(&resolved).cloned().unwrap_or_default(),
        symbols: outline
            .results
            .iter()
            .take(MAX_EXPLAIN_SYMBOLS)
            .map(SymbolDto::from)
            .collect(),
        symbols_total: outline.results.len(),
    });
    if outline.results.len() > MAX_EXPLAIN_SYMBOLS {
        package.notes.push(format!(
            "Showing {MAX_EXPLAIN_SYMBOLS} of {} defined symbols.",
            outline.results.len()
        ));
    }

    // The file's own imports are its precise depends-on set; an import with
    // no indexed module reads as external (stdlib/third-party).
    let mut imports = queries::imports_for_file(conn, &resolved)?;
    imports.sort();
    imports.dedup();
    for (module_path, _) in imports {
        let external = queries::files_for_module_path(conn, &module_path)?.is_empty();
        package.dependencies.push(DependencyDto::from(&Dependency {
            module_path,
            file: None,
            external,
        }));
    }

    // Dependents via the file's distinct modules.
    let modules: BTreeSet<String> = outline
        .results
        .iter()
        .map(|s| s.module_path.clone())
        .filter(|m| !m.is_empty())
        .collect();
    let mut dependents: BTreeMap<String, Dependency> = BTreeMap::new();
    for module in &modules {
        for dep in deps::find_dependents(conn, module)? {
            dependents.entry(dep.module_path.clone()).or_insert(dep);
        }
    }
    package.dependents = dependents
        .into_values()
        .map(|d| DependencyDto::from(&d))
        .collect();

    attach_linked_knowledge(conn, &mut package, Some(&resolved), None)?;
    if let Some(hash) = hashes.get(&resolved) {
        package.source_hashes.insert(resolved, hash.clone());
    }
    Ok(package)
}

/// Explain one module: files, symbols, dependencies, linked knowledge.
pub fn explain_module(conn: &Connection, module: &str) -> Result<Explanation> {
    let mut package = Explanation::empty(module, "module");
    let mut files = queries::files_for_module_path(conn, module)?;
    if files.is_empty() {
        package.notes.push(format!("No indexed files for module `{module}`."));
        return Ok(package);
    }
    files.sort();
    files.dedup();
    let mut symbols = Vec::new();
    for file in &files {
        symbols.extend(queries::symbols_in_file(conn, file)?);
    }
    symbols.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then_with(|| a.start_line.cmp(&b.start_line))
    });
    package.module = Some(ExplainedModule {
        module_path: module.to_string(),
        files: files.clone(),
        symbols: symbols.iter().take(MAX_EXPLAIN_SYMBOLS).map(SymbolDto::from).collect(),
        symbols_total: symbols.len(),
    });
    if symbols.len() > MAX_EXPLAIN_SYMBOLS {
        package.notes.push(format!(
            "Showing {MAX_EXPLAIN_SYMBOLS} of {} defined symbols.",
            symbols.len()
        ));
    }

    package.dependencies = deps::find_dependencies(conn, module)?
        .iter()
        .map(DependencyDto::from)
        .collect();
    package.dependents = deps::find_dependents(conn, module)?
        .iter()
        .map(DependencyDto::from)
        .collect();

    // Records citing any module file, merged by id.
    let mut linked: BTreeMap<String, crate::knowledge::KnowledgeRecord> = BTreeMap::new();
    for file in &files {
        for record in knowledge::records_linked(conn, Some(file), None)? {
            linked.entry(record.id.clone()).or_insert(record);
        }
    }
    attach_records(conn, &mut package, linked.into_values().collect())?;

    let hashes = queries::existing_hashes(conn)?;
    for file in &files {
        if let Some(hash) = hashes.get(file) {
            package.source_hashes.insert(file.clone(), hash.clone());
        }
    }
    Ok(package)
}

/// Attach capped linked knowledge (with live freshness) for evidence citing
/// `path` and/or `symbol`.
fn attach_linked_knowledge(
    conn: &Connection,
    package: &mut Explanation,
    path: Option<&str>,
    symbol: Option<&str>,
) -> Result<()> {
    let records = knowledge::records_linked(conn, path, symbol)?;
    attach_records(conn, package, records)
}

/// Freshness-check and cap one linked-record list.
fn attach_records(
    conn: &Connection,
    package: &mut Explanation,
    records: Vec<crate::knowledge::KnowledgeRecord>,
) -> Result<()> {
    if records.len() > MAX_EXPLAIN_KNOWLEDGE {
        package.notes.push(format!(
            "Showing {MAX_EXPLAIN_KNOWLEDGE} of {} linked knowledge records.",
            records.len()
        ));
    }
    for record in records.iter().take(MAX_EXPLAIN_KNOWLEDGE) {
        let (fresh, reasons) = knowledge::check_one(conn, &record.id)?
            .map(|f| (f.fresh, f.reasons))
            .unwrap_or((true, Vec::new()));
        package.linked_knowledge.push(LinkedKnowledge {
            record: record.clone(),
            fresh,
            reasons,
        });
    }
    Ok(())
}

impl Explanation {
    /// Empty package shell for one target (misses and disambiguation fill
    /// notes/candidates and return early).
    fn empty(target: &str, kind: &str) -> Self {
        Self {
            target: target.to_string(),
            target_kind: kind.to_string(),
            generated: false,
            generator: None,
            definition: None,
            excerpt: None,
            file: None,
            module: None,
            callers: Vec::new(),
            callers_total: 0,
            dependencies: Vec::new(),
            dependents: Vec::new(),
            implementations: Vec::new(),
            linked_knowledge: Vec::new(),
            source_hashes: BTreeMap::new(),
            disambiguation: Vec::new(),
            notes: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;
    use crate::index;
    use crate::knowledge::{KnowledgeOrigin, KnowledgeRecord, KnowledgeType};
    use std::fs;
    use tempfile::tempdir;

    /// `src/svc.py` (serve, helper, run→serve), `src/other.py` (second serve).
    fn fixture() -> (tempfile::TempDir, Connection) {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/svc.py"),
            "def serve(x):\n    return helper(x)\n\ndef helper(x):\n    return x\n\ndef run():\n    return serve(1)\n",
        )
        .unwrap();
        fs::write(root.join("src/other.py"), "def serve(y):\n    return y\n").unwrap();
        let mut conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        index::index_repository(root, &mut conn).unwrap();
        (dir, conn)
    }

    fn rooted(dir: &tempfile::TempDir) -> ExplainOptions {
        ExplainOptions {
            source_root: Some(dir.path().to_path_buf()),
        }
    }

    fn seed_knowledge(conn: &mut Connection) {
        let mut record = KnowledgeRecord::new(
            "svc.serve",
            KnowledgeType::Fact,
            "Serve",
            "serve delegates to helper",
            KnowledgeOrigin::Agent,
        );
        record.evidence.push(crate::knowledge::KnowledgeEvidence {
            source_path: "src/svc.py".to_string(),
            symbol: Some("serve".to_string()),
            source_hash: None,
            commit_hash: None,
        });
        knowledge::put_record(conn, &record).unwrap();
    }

    #[test]
    fn symbol_package_has_definition_callers_and_pins() {
        let (dir, mut conn) = fixture();
        seed_knowledge(&mut conn);
        // Disambiguate via the live module path (whatever the convention).
        let defs = crate::db::queries::find_definition(&conn, "serve").unwrap();
        assert_eq!(defs.len(), 2);
        let module = defs
            .iter()
            .find(|d| d.file.to_string_lossy() == "src/svc.py")
            .unwrap()
            .module_path
            .clone();

        let package =
            explain_symbol(&conn, "serve", Some(&module), &rooted(&dir)).unwrap();
        assert_eq!(package.target_kind, "symbol");
        assert!(!package.generated);
        assert!(package.generator.is_none());
        let def = package.definition.as_ref().expect("definition");
        assert_eq!(def.name, "serve");
        assert_eq!(def.file, "src/svc.py");
        assert!(
            package.excerpt.as_deref().unwrap_or("").starts_with("def serve(x):"),
            "excerpt: {:?}",
            package.excerpt
        );
        // `run` calls `serve`.
        assert!(package.callers.iter().any(|c| c.name == "serve"
            && c.file == "src/svc.py"
            && c.start_line == 8));
        assert_eq!(package.callers_total, package.callers.len());
        // Linked knowledge with a live verdict.
        assert_eq!(package.linked_knowledge.len(), 1);
        assert_eq!(package.linked_knowledge[0].record.id, "svc.serve");
        // Pins cover the definition file.
        assert!(package.source_hashes.contains_key("src/svc.py"));
    }

    #[test]
    fn ambiguous_symbol_lists_candidates_with_empty_sections() {
        let (dir, conn) = fixture();
        let package = explain_symbol(&conn, "serve", None, &rooted(&dir)).unwrap();
        assert_eq!(package.disambiguation.len(), 2);
        assert!(package.definition.is_none());
        assert!(package.callers.is_empty());
        assert!(package.notes.iter().any(|n| n.contains("disambiguate")));
    }

    #[test]
    fn unknown_symbol_misses_honestly() {
        let (dir, conn) = fixture();
        let package = explain_symbol(&conn, "nope", None, &rooted(&dir)).unwrap();
        assert!(package.definition.is_none());
        assert!(package.disambiguation.is_empty());
        assert!(package.notes.iter().any(|n| n.contains("No matching symbols found")));
    }

    #[test]
    fn file_package_outlines_pins_and_links() {
        let (_dir, mut conn) = fixture();
        seed_knowledge(&mut conn);
        let package = explain_file(&conn, "src/svc.py").unwrap();
        let file = package.file.as_ref().expect("file summary");
        assert_eq!(file.path, "src/svc.py");
        assert!(!file.content_hash.is_empty());
        assert_eq!(file.symbols_total, 3);
        assert_eq!(package.source_hashes.len(), 1);
        assert_eq!(package.linked_knowledge.len(), 1);

        let miss = explain_file(&conn, "src/gone.py").unwrap();
        assert!(miss.file.is_none());
        assert!(miss.notes.iter().any(|n| n.contains("No indexed file matches")));
    }

    #[test]
    fn module_package_covers_files_and_symbols() {
        let (_dir, conn) = fixture();
        let defs = crate::db::queries::find_definition(&conn, "serve").unwrap();
        let module = defs[0].module_path.clone();
        let package = explain_module(&conn, &module).unwrap();
        let summary = package.module.as_ref().expect("module summary");
        assert!(!summary.files.is_empty());
        assert!(summary.symbols_total >= 1);
        assert!(!package.source_hashes.is_empty());

        let miss = explain_module(&conn, "no.such.module").unwrap();
        assert!(miss.module.is_none());
        assert!(miss.notes.iter().any(|n| n.contains("No indexed files for module")));
    }

    #[test]
    fn packages_are_deterministic_and_rootless_still_cite() {
        let (dir, conn) = fixture();
        let defs = crate::db::queries::find_definition(&conn, "serve").unwrap();
        let module = defs
            .iter()
            .find(|d| d.file.to_string_lossy() == "src/svc.py")
            .unwrap()
            .module_path
            .clone();
        let options = rooted(&dir);
        let a = serde_json::to_string(&explain_symbol(&conn, "serve", Some(&module), &options).unwrap())
            .unwrap();
        let b = serde_json::to_string(&explain_symbol(&conn, "serve", Some(&module), &options).unwrap())
            .unwrap();
        assert_eq!(a, b);

        let bare =
            explain_symbol(&conn, "serve", Some(&module), &ExplainOptions::default()).unwrap();
        assert!(bare.excerpt.is_none());
        assert!(bare.notes.iter().any(|n| n.contains("No source root")));
        assert!(bare.definition.is_some(), "citations survive without a root");
    }
}
