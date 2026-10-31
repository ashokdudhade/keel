//! Architecture rules: explicitly declared dependency constraints,
//! enforced deterministically against the indexed dependency graph.
//!
//! Teams declare constraints in `.keel/rules.json` (see [`Rules`] for the
//! schema). [`check_rules`] evaluates them with the same evidence rules as
//! [`find_dependencies`] — tier ≤ 2,
//! no name-only fallback — so a violation always names a real import or path
//! edge, never a guess.
//!
//! Module patterns match a module and its submodules: `crate::api` covers
//! `crate::api::routes` but not `crate::apix`. The boundary check accepts
//! every language separator (`::`, `.`, `/`), so one schema serves Rust,
//! Python, and TS/JS monorepos alike.

use crate::db::queries;
use crate::error::{KeelError, Result};
use crate::graph::deps::{find_dependencies, Dependency};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// One forbidden dependency edge: `from` must not depend on `to`.
///
/// Both sides are module patterns (see [`module_matches`]). `to` may name an
/// external (unindexed) module such as a stdlib path; `from` must match at
/// least one indexed module or the checker reports a note instead of
/// silently passing.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ForbidRule {
    /// Constrained module pattern (the dependency source).
    pub from: String,
    /// Forbidden dependency target pattern (may be external).
    pub to: String,
    /// Human rationale, echoed on violations.
    #[serde(default)]
    pub reason: Option<String>,
}

/// One named layer. Layers are ordered top (index 0) to bottom: a module in
/// layer `i` may only depend on modules in layers `j >= i`. Dependencies on
/// modules that belong to no layer are unconstrained.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Layer {
    /// Layer name, used in violation messages.
    pub name: String,
    /// Module patterns belonging to this layer (first layer wins on overlap).
    pub modules: Vec<String>,
}

/// A parsed `.keel/rules.json` file: forbidden edges plus an optional layer
/// stack. Both sections default to empty; an empty file is clean by
/// definition and reports a note saying so.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Rules {
    /// Forbidden dependency edges.
    #[serde(default)]
    pub forbid: Vec<ForbidRule>,
    /// Ordered layer stack, top first.
    #[serde(default)]
    pub layers: Vec<Layer>,
}

/// One constraint violation: a real dependency edge that breaks a rule.
#[derive(Debug, Clone, Serialize)]
pub struct RulesViolation {
    /// Human-readable rule label (`forbid a -> b` or `layer 'x' above 'y'`).
    pub rule: String,
    /// Actual indexed module holding the offending edge.
    pub from_module: String,
    /// Actual dependency target module.
    pub to_module: String,
    /// Importing file evidencing the edge, when an import row names it.
    /// Edges derived from path qualifiers alone carry no file.
    pub via: Option<PathBuf>,
    /// Declared rationale for `forbid` rules, if any.
    pub reason: Option<String>,
}

/// Outcome of [`check_rules`]: violations plus honesty notes (unindexed
/// constrained modules, empty rule sets). Deterministically ordered.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RulesReport {
    /// Violations, sorted by `(rule, from_module, to_module)`.
    pub violations: Vec<RulesViolation>,
    /// Notes about the check itself, sorted.
    pub notes: Vec<String>,
    /// Number of `forbid` rules evaluated.
    pub forbid_checked: usize,
    /// Number of layers evaluated.
    pub layers_checked: usize,
}

impl RulesReport {
    /// True when no rule was violated (notes may still be present).
    pub fn is_clean(&self) -> bool {
        self.violations.is_empty()
    }
}

/// True when `actual` equals `pattern` or sits beneath it on a separator
/// boundary (`::`, `.`, or `/`). `crate::api` matches `crate::api::routes`
/// but not `crate::apix`; the same holds for dotted and path-style ids.
pub fn module_matches(pattern: &str, actual: &str) -> bool {
    if actual == pattern {
        return true;
    }
    let rest = match actual.strip_prefix(pattern) {
        Some(rest) => rest,
        None => return false,
    };
    matches!(rest.chars().next(), Some(':') | Some('.') | Some('/'))
}

/// Read and parse a JSON rules file. I/O failures surface as
/// [`KeelError::Io`]; malformed JSON as [`KeelError::Rules`].
pub fn load_rules(path: &Path) -> Result<Rules> {
    let text = std::fs::read_to_string(path).map_err(|source| KeelError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_str(&text)
        .map_err(|err| KeelError::Rules(format!("{}: {err}", path.display())))
}

/// Check `rules` against the indexed dependency graph.
///
/// Every actual indexed module is resolved once via `find_dependencies`;
/// `forbid` rules fire on any edge (including external targets) whose ends
/// match, while layer rules fire only when both ends belong to known layers
/// and the edge points upward. Constrained modules (`forbid.from` and layer
/// members) that match nothing indexed yield notes, not silent passes.
pub fn check_rules(conn: &Connection, rules: &Rules) -> Result<RulesReport> {
    let mut report = RulesReport {
        forbid_checked: rules.forbid.len(),
        layers_checked: rules.layers.len(),
        ..RulesReport::default()
    };
    if rules.forbid.is_empty() && rules.layers.is_empty() {
        report
            .notes
            .push("rules file declares no constraints".to_string());
        return Ok(report);
    }

    let actual_modules = distinct_indexed_modules(conn)?;
    let mut deps_cache: HashMap<String, Vec<Dependency>> = HashMap::new();
    let deps_of = |conn: &Connection,
                       module: &str,
                       cache: &mut HashMap<String, Vec<Dependency>>|
     -> Result<Vec<Dependency>> {
        if let Some(cached) = cache.get(module) {
            return Ok(cached.clone());
        }
        let deps = find_dependencies(conn, module)?;
        cache.insert(module.to_string(), deps.clone());
        Ok(deps)
    };

    let mut constrained: BTreeSet<&str> = BTreeSet::new();
    for rule in &rules.forbid {
        constrained.insert(rule.from.as_str());
        for actual in matching_modules(&actual_modules, &rule.from) {
            let deps = deps_of(conn, actual, &mut deps_cache)?;
            for dep in &deps {
                if module_matches(&rule.to, &dep.module_path) {
                    report.violations.push(RulesViolation {
                        rule: format!("forbid {} -> {}", rule.from, rule.to),
                        from_module: actual.to_string(),
                        to_module: dep.module_path.clone(),
                        via: import_evidence(conn, actual, &rule.to)?,
                        reason: rule.reason.clone(),
                    });
                }
            }
        }
    }
    for (index, layer) in rules.layers.iter().enumerate() {
        for module in &layer.modules {
            constrained.insert(module.as_str());
        }
        for actual in matching_modules_in_layer(&actual_modules, layer) {
            // First layer wins: skip modules already claimed above.
            if layer_of(rules, actual) != Some(index) {
                continue;
            }
            let deps = deps_of(conn, actual, &mut deps_cache)?;
            for dep in &deps {
                if let Some(target_layer) = layer_of(rules, &dep.module_path) {
                    if target_layer < index {
                        report.violations.push(RulesViolation {
                            rule: format!(
                                "layer '{}' above layer '{}'",
                                layer.name, rules.layers[target_layer].name
                            ),
                            from_module: actual.to_string(),
                            to_module: dep.module_path.clone(),
                            via: import_evidence_for_layer(
                                conn,
                                actual,
                                &rules.layers[target_layer],
                            )?,
                            reason: None,
                        });
                    }
                }
            }
        }
    }
    // Layer membership of a submodule can shadow an exact member of a lower
    // layer, so coverage is computed per actual module, not per pattern.
    for pattern in constrained {
        if !actual_modules
            .iter()
            .any(|actual| module_matches(pattern, actual))
        {
            report.notes.push(format!(
                "rule mentions '{pattern}', which matches no indexed module"
            ));
        }
    }

    report.violations.sort_by(|a, b| {
        (&a.rule, &a.from_module, &a.to_module)
            .cmp(&(&b.rule, &b.from_module, &b.to_module))
    });
    report.violations.dedup_by(|a, b| {
        a.rule == b.rule && a.from_module == b.from_module && a.to_module == b.to_module
    });
    report.notes.sort();
    Ok(report)
}

/// Distinct `module_path` values across all indexed files, sorted.
fn distinct_indexed_modules(conn: &Connection) -> Result<Vec<String>> {
    let mut modules = BTreeSet::new();
    for file in queries::indexed_files(conn)? {
        for module in queries::module_paths_in_file(conn, &file)? {
            modules.insert(module);
        }
    }
    Ok(modules.into_iter().collect())
}

/// Indexed modules covered by `pattern`, in sorted order.
fn matching_modules<'a>(actual_modules: &'a [String], pattern: &str) -> Vec<&'a str> {
    actual_modules
        .iter()
        .map(String::as_str)
        .filter(|actual| module_matches(pattern, actual))
        .collect()
}

/// Indexed modules whose layer membership resolves to `layer` (first layer
/// whose patterns match wins; checked by the caller via [`layer_of`]).
fn matching_modules_in_layer<'a>(
    actual_modules: &'a [String],
    layer: &Layer,
) -> Vec<&'a str> {
    actual_modules
        .iter()
        .map(String::as_str)
        .filter(|actual| {
            layer
                .modules
                .iter()
                .any(|pattern| module_matches(pattern, actual))
        })
        .collect()
}

/// Index of the first layer claiming `module`, if any. First layer wins
/// on overlap, so a submodule claimed by an upper layer never
/// double-attributes to a lower one.
fn layer_of(rules: &Rules, module: &str) -> Option<usize> {
    rules.layers.iter().position(|layer| {
        layer
            .modules
            .iter()
            .any(|pattern| module_matches(pattern, module))
    })
}

/// First file (sorted) under `from_module` whose imports name a module
/// matching `to_pattern`: the evidence trail for a `forbid` violation.
/// Returns `None` when the edge comes from path qualifiers alone.
fn import_evidence(
    conn: &Connection,
    from_module: &str,
    to_pattern: &str,
) -> Result<Option<PathBuf>> {
    let mut files = queries::files_for_module_path(conn, from_module)?;
    files.sort();
    for file in files {
        for (imported, _) in queries::imports_for_file(conn, &file)? {
            if module_matches(to_pattern, &imported) {
                return Ok(Some(PathBuf::from(file)));
            }
        }
    }
    Ok(None)
}

/// Evidence variant for layer violations: any import matching one of the
/// target layer's patterns.
fn import_evidence_for_layer(
    conn: &Connection,
    from_module: &str,
    target: &Layer,
) -> Result<Option<PathBuf>> {
    let mut files = queries::files_for_module_path(conn, from_module)?;
    files.sort();
    for file in files {
        for (imported, _) in queries::imports_for_file(conn, &file)? {
            if target
                .modules
                .iter()
                .any(|pattern| module_matches(pattern, &imported))
            {
                return Ok(Some(PathBuf::from(file)));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_patterns_match_submodules_on_separator_boundaries() {
        assert!(module_matches("crate::api", "crate::api"));
        assert!(module_matches("crate::api", "crate::api::routes"));
        assert!(!module_matches("crate::api", "crate::apix"));
        assert!(!module_matches("crate::api", "crate::ap"));
        assert!(module_matches("pkg.mod", "pkg.mod.sub"));
        assert!(!module_matches("pkg.mod", "pkg.modx"));
        assert!(module_matches("src/auth", "src/auth/service"));
        assert!(!module_matches("src/auth", "src/authx"));
        assert!(!module_matches("crate::api", "other::api"));
    }

    #[test]
    fn load_rules_rejects_malformed_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rules.json");
        std::fs::write(&path, "{ not json").unwrap();
        let err = load_rules(&path).unwrap_err();
        assert!(matches!(err, KeelError::Rules(_)));
    }

    #[test]
    fn load_rules_reports_missing_file_as_io() {
        let err = load_rules(Path::new("/nonexistent/rules.json")).unwrap_err();
        assert!(matches!(err, KeelError::Io { .. }));
    }

    #[test]
    fn load_rules_accepts_empty_object() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rules.json");
        std::fs::write(&path, "{}").unwrap();
        let rules = load_rules(&path).unwrap();
        assert!(rules.forbid.is_empty());
        assert!(rules.layers.is_empty());
    }

    #[test]
    fn empty_rules_are_clean_with_note() {
        // Empty rules return before touching the database.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let report = check_rules(&conn, &Rules::default()).unwrap();
        assert!(report.is_clean());
        assert_eq!(report.notes.len(), 1);
    }
}
