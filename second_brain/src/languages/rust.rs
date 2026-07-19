//! Rust language plugin: Tree-sitter based symbol and reference extraction.
//!
//! Symbols and references use manual, pre-order tree walks so we can track the
//! enclosing `mod`/`fn` scope for qualified `module_path` and reference
//! containers. `impl` extraction uses a compiled Tree-sitter [`Query`] cached
//! in a [`OnceLock`] so query compilation runs once per process.
//!
//! ## Known limitations
//!
//! - Impact analysis uses qualified identity with resolve-aware expansion; see
//!   `graph::impact`.
//! - Go has no trait-impl form; `implementations` stays empty for Go sources.

use super::{file_path_key, LanguagePlugin};
use crate::error::{Result, SecondBrainError};
use crate::graph::types::{ImplRecord, Import, Reference, ReferenceKind, Symbol, SymbolKind};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor, Tree};

/// Compiled once: match `impl` blocks whose type target is a plain identifier.
const IMPL_QUERY_SRC: &str = r#"
(impl_item type: (type_identifier) @type)
"#;

fn impl_query() -> &'static Query {
    static QUERY: OnceLock<Query> = OnceLock::new();
    QUERY.get_or_init(|| {
        let language = tree_sitter_rust::LANGUAGE.into();
        Query::new(&language, IMPL_QUERY_SRC).expect("IMPL_QUERY_SRC is valid")
    })
}

/// Extractor for Rust source using Tree-sitter.
pub struct RustPlugin;

impl RustPlugin {
    fn parse(source: &str) -> Result<Tree> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .map_err(|e| SecondBrainError::TreeSitter(e.to_string()))?;
        parser.parse(source, None).ok_or(SecondBrainError::Parse)
    }
}

impl LanguagePlugin for RustPlugin {
    fn extensions(&self) -> &[&str] {
        &["rs"]
    }

    fn extract_symbols(&self, _path: &Path, source_code: &str) -> Result<Vec<Symbol>> {
        let tree = Self::parse(source_code)?;
        let src = source_code.as_bytes();
        let mut mods: Vec<String> = Vec::new();
        let mut out = Vec::new();
        walk_symbols(tree.root_node(), src, &mut mods, &mut out)?;
        Ok(out)
    }

    fn extract_references(&self, path: &Path, source_code: &str) -> Result<Vec<Reference>> {
        let tree = Self::parse(source_code)?;
        let src = source_code.as_bytes();
        let file_key = file_path_key(path);
        let mut scope: Vec<String> = Vec::new();
        let mut out = Vec::new();
        walk_references(tree.root_node(), src, &file_key, &mut scope, &mut out)?;
        Ok(out)
    }

    fn extract_imports(&self, _path: &Path, source_code: &str) -> Result<Vec<Import>> {
        let tree = Self::parse(source_code)?;
        let src = source_code.as_bytes();
        let mut out = Vec::new();
        walk_imports(tree.root_node(), src, &mut out)?;
        Ok(out)
    }

    fn extract_impls(&self, _path: &Path, source_code: &str) -> Result<Vec<ImplRecord>> {
        let tree = Self::parse(source_code)?;
        let src = source_code.as_bytes();
        let query = impl_query();
        let mut cursor = QueryCursor::new();
        let mut out = Vec::new();
        let mut matches = cursor.matches(query, tree.root_node(), src);
        while let Some(m) = matches.next() {
            for cap in m.captures {
                let ty = cap.node;
                let Some(impl_item) = ty.parent() else {
                    continue;
                };
                if impl_item.kind() != "impl_item" {
                    continue;
                }
                let type_name = node_text(ty, src)?.to_string();
                let trait_name = match impl_item.child_by_field_name("trait") {
                    Some(t) => Some(node_text(t, src)?.to_string()),
                    None => None,
                };
                let pos = impl_item.start_position();
                out.push(ImplRecord {
                    type_name,
                    trait_name,
                    file: PathBuf::new(),
                    start_line: pos.row as u32 + 1,
                    start_col: pos.column as u32 + 1,
                });
            }
        }
        Ok(out)
    }
}

/// UTF-8 text of a node, surfacing decode errors as `TreeSitter`.
fn node_text<'a>(node: Node, src: &'a [u8]) -> Result<&'a str> {
    node.utf8_text(src)
        .map_err(|e| SecondBrainError::TreeSitter(e.to_string()))
}

/// Qualified module path for an item nested under `mods` (top level = `crate`).
fn qualify_mod(mods: &[String]) -> String {
    if mods.is_empty() {
        "crate".to_string()
    } else {
        format!("crate::{}", mods.join("::"))
    }
}

/// Qualified name of the enclosing `fn`/`mod` scope, or the file path when
/// top-level (so empty-scope call sites still participate in impact).
fn qualify_scope(file_key: &str, scope: &[String]) -> String {
    if scope.is_empty() {
        file_key.to_string()
    } else {
        format!("crate::{}", scope.join("::"))
    }
}

/// Pre-order walk emitting a [`Symbol`] per definition, tracking the enclosing
/// `mod` chain so each symbol records its qualified `module_path`.
fn walk_symbols(node: Node, src: &[u8], mods: &mut Vec<String>, out: &mut Vec<Symbol>) -> Result<()> {
    match node.kind() {
        "mod_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                let text = node_text(name, src)?;
                push_symbol(name, SymbolKind::Module, mods, out, src)?;
                mods.push(text.to_string());
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    walk_symbols(child, src, mods, out)?;
                }
                mods.pop();
                return Ok(());
            }
        }
        "function_item" => emit_named_symbol(node, SymbolKind::Function, mods, out, src)?,
        "struct_item" => emit_named_symbol(node, SymbolKind::Struct, mods, out, src)?,
        "trait_item" => emit_named_symbol(node, SymbolKind::Trait, mods, out, src)?,
        "enum_item" => emit_named_symbol(node, SymbolKind::Enum, mods, out, src)?,
        "const_item" => emit_named_symbol(node, SymbolKind::Const, mods, out, src)?,
        "impl_item" => {
            // Match the historical behavior: only plain `type_identifier` impl
            // targets become an `Impl` symbol (skip `impl Vec<T>` etc.).
            if let Some(ty) = node.child_by_field_name("type") {
                if ty.kind() == "type_identifier" {
                    push_symbol(ty, SymbolKind::Impl, mods, out, src)?;
                }
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_symbols(child, src, mods, out)?;
    }
    Ok(())
}

/// Emit a symbol from a node's `name` field child, if present.
fn emit_named_symbol(
    node: Node,
    kind: SymbolKind,
    mods: &[String],
    out: &mut Vec<Symbol>,
    src: &[u8],
) -> Result<()> {
    if let Some(name) = node.child_by_field_name("name") {
        push_symbol(name, kind, mods, out, src)?;
    }
    Ok(())
}

/// Push a symbol for the identifier `name_node` with the current module path.
fn push_symbol(
    name_node: Node,
    kind: SymbolKind,
    mods: &[String],
    out: &mut Vec<Symbol>,
    src: &[u8],
) -> Result<()> {
    let text = node_text(name_node, src)?;
    let pos = name_node.start_position();
    out.push(Symbol {
        name: text.to_string(),
        kind,
        file: PathBuf::new(),
        start_line: pos.row as u32 + 1,
        start_col: pos.column as u32 + 1,
        module_path: qualify_mod(mods),
    });
    Ok(())
}

/// Pre-order walk emitting a [`Reference`] per call/macro/method/type site,
/// tracking the enclosing `fn`/`mod` chain for each reference's `container`.
fn walk_references(
    node: Node,
    src: &[u8],
    file_key: &str,
    scope: &mut Vec<String>,
    out: &mut Vec<Reference>,
) -> Result<()> {
    match node.kind() {
        "mod_item" | "function_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                let text = node_text(name, src)?;
                scope.push(text.to_string());
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    walk_references(child, src, file_key, scope, out)?;
                }
                scope.pop();
                return Ok(());
            }
        }
        "call_expression" => {
            if let Some(func) = node.child_by_field_name("function") {
                emit_call_reference(func, src, file_key, scope, out)?;
            }
        }
        "macro_invocation" => {
            if let Some(mac) = node.child_by_field_name("macro") {
                if let Some((name, target)) = final_segment(mac, src)? {
                    push_reference(name, target, ReferenceKind::Macro, file_key, scope, out);
                }
            }
        }
        // A type usage, unless this identifier is the *name* of its parent
        // definition (struct/enum/trait/union/type alias or a generic type
        // parameter), which would double-count the definition site.
        "type_identifier" if !is_definition_name(node) => {
            let text = node_text(node, src)?;
            push_reference(
                text.to_string(),
                node,
                ReferenceKind::Type,
                file_key,
                scope,
                out,
            );
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_references(child, src, file_key, scope, out)?;
    }
    Ok(())
}

/// Emit a reference for the `function` child of a call expression.
fn emit_call_reference(
    func: Node,
    src: &[u8],
    file_key: &str,
    scope: &[String],
    out: &mut Vec<Reference>,
) -> Result<()> {
    match func.kind() {
        "identifier" => {
            let text = node_text(func, src)?;
            push_reference(
                text.to_string(),
                func,
                ReferenceKind::Call,
                file_key,
                scope,
                out,
            );
        }
        "scoped_identifier" => {
            if let Some((name, target)) = final_segment(func, src)? {
                push_reference(name, target, ReferenceKind::Path, file_key, scope, out);
            }
        }
        "field_expression" => {
            if let Some(field) = func.child_by_field_name("field") {
                if field.kind() == "field_identifier" {
                    let text = node_text(field, src)?;
                    push_reference(
                        text.to_string(),
                        field,
                        ReferenceKind::Method,
                        file_key,
                        scope,
                        out,
                    );
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Resolve the final identifier segment (and its node) for an `identifier` or
/// `scoped_identifier`, used for path calls and macro invocations.
fn final_segment<'a>(node: Node<'a>, src: &[u8]) -> Result<Option<(String, Node<'a>)>> {
    match node.kind() {
        "identifier" => Ok(Some((node_text(node, src)?.to_string(), node))),
        "scoped_identifier" => match node.child_by_field_name("name") {
            Some(name) => Ok(Some((node_text(name, src)?.to_string(), name))),
            None => Ok(None),
        },
        _ => Ok(None),
    }
}

/// True when `node` is the `name` field of its parent definition, i.e. a
/// declaration site rather than a use of a type.
fn is_definition_name(node: Node) -> bool {
    node.parent()
        .and_then(|p| p.child_by_field_name("name"))
        .map(|n| n == node)
        .unwrap_or(false)
}

/// Push a reference with the current enclosing-scope container.
fn push_reference(
    name: String,
    node: Node,
    kind: ReferenceKind,
    file_key: &str,
    scope: &[String],
    out: &mut Vec<Reference>,
) {
    let pos = node.start_position();
    out.push(Reference {
        name,
        file: PathBuf::new(),
        start_line: pos.row as u32 + 1,
        start_col: pos.column as u32 + 1,
        kind,
        container: qualify_scope(file_key, scope),
    });
}

/// Pre-order walk emitting an [`Import`] per imported path, expanding grouped
/// `use a::{b, c as d}` lists into one record each.
fn walk_imports(node: Node, src: &[u8], out: &mut Vec<Import>) -> Result<()> {
    if node.kind() == "use_declaration" {
        if let Some(arg) = node.child_by_field_name("argument") {
            expand_use(arg, "", src, out)?;
        }
        return Ok(());
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_imports(child, src, out)?;
    }
    Ok(())
}

/// Join a path prefix with a trailing segment (`""` prefix yields `seg`).
fn join_path(prefix: &str, seg: &str) -> String {
    if prefix.is_empty() {
        seg.to_string()
    } else {
        format!("{prefix}::{seg}")
    }
}

/// Recursively expand a `use` argument subtree into flat [`Import`] records.
fn expand_use(node: Node, prefix: &str, src: &[u8], out: &mut Vec<Import>) -> Result<()> {
    match node.kind() {
        "scoped_use_list" => {
            let new_prefix = match node.child_by_field_name("path") {
                Some(p) => join_path(prefix, node_text(p, src)?),
                None => prefix.to_string(),
            };
            if let Some(list) = node.child_by_field_name("list") {
                let mut cursor = list.walk();
                for child in list.named_children(&mut cursor) {
                    expand_use(child, &new_prefix, src, out)?;
                }
            }
        }
        "use_list" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                expand_use(child, prefix, src, out)?;
            }
        }
        "use_as_clause" => {
            let module_path = match node.child_by_field_name("path") {
                Some(p) => join_path(prefix, node_text(p, src)?),
                None => prefix.to_string(),
            };
            let alias = match node.child_by_field_name("alias") {
                Some(a) => Some(node_text(a, src)?.to_string()),
                None => None,
            };
            out.push(Import { module_path, alias, file: PathBuf::new() });
        }
        // `self` inside a group refers to the enclosing path itself.
        "self" => {
            if !prefix.is_empty() {
                out.push(Import { module_path: prefix.to_string(), alias: None, file: PathBuf::new() });
            }
        }
        // Simple leaf segment or full scoped path: `use a::b::c;` / `{c}`.
        _ => {
            let seg = node_text(node, src)?;
            out.push(Import {
                module_path: join_path(prefix, seg),
                alias: None,
                file: PathBuf::new(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::types::SymbolKind;

    const SOURCE: &str = "\
pub struct AuthService;
pub trait Storage {}
fn create_order() {}
fn run() {
    create_order();
    println!(\"hi\");
}
";

    // A multi-item fixture exercising module paths, imports, impls, and the
    // various reference kinds. Line numbers referenced in tests are 1-based.
    const RICH: &str = "\
use std::collections::HashMap;
use a::{b, c as d};

pub struct MemStore;

pub trait Storage {}

impl Storage for MemStore {}

impl MemStore {
    fn helper(&self) {}
}

mod auth {
    pub fn login(store: MemStore) {
        let map: HashMap = todo!();
        store.helper();
        greet();
        println!(\"hi\");
    }
}

fn greet() {}
";

    fn test_path() -> &'static Path {
        Path::new("src/lib.rs")
    }

    #[test]
    fn extracts_struct_trait_and_functions() {
        let plugin = RustPlugin;
        let syms = plugin.extract_symbols(test_path(), SOURCE).unwrap();
        let find = |n: &str| syms.iter().find(|s| s.name == n).cloned();

        let auth = find("AuthService").expect("AuthService symbol");
        assert_eq!(auth.kind, SymbolKind::Struct);
        assert_eq!(auth.start_line, 1);

        assert_eq!(find("Storage").unwrap().kind, SymbolKind::Trait);
        assert_eq!(find("create_order").unwrap().kind, SymbolKind::Function);
        assert_eq!(find("run").unwrap().kind, SymbolKind::Function);
    }

    #[test]
    fn extracts_call_and_macro_references() {
        let plugin = RustPlugin;
        let refs = plugin.extract_references(test_path(), SOURCE).unwrap();
        let names: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"create_order"));
        assert!(names.contains(&"println"));

        let call = refs.iter().find(|r| r.name == "create_order").unwrap();
        assert_eq!(call.start_line, 5);
    }

    #[test]
    fn top_level_symbol_has_crate_module_path() {
        let plugin = RustPlugin;
        let syms = plugin.extract_symbols(test_path(), RICH).unwrap();
        let greet = syms.iter().find(|s| s.name == "greet").unwrap();
        assert_eq!(greet.module_path, "crate");
    }

    #[test]
    fn symbol_inside_mod_has_qualified_module_path() {
        let plugin = RustPlugin;
        let syms = plugin.extract_symbols(test_path(), RICH).unwrap();
        let login = syms.iter().find(|s| s.name == "login").unwrap();
        assert_eq!(login.module_path, "crate::auth");
    }

    #[test]
    fn extracts_simple_import() {
        let plugin = RustPlugin;
        let imports = plugin.extract_imports(test_path(), RICH).unwrap();
        let imp = imports
            .iter()
            .find(|i| i.module_path == "std::collections::HashMap")
            .expect("HashMap import");
        assert_eq!(imp.alias, None);
    }

    #[test]
    fn expands_grouped_imports_with_alias() {
        let plugin = RustPlugin;
        let imports = plugin.extract_imports(test_path(), RICH).unwrap();
        let b = imports.iter().find(|i| i.module_path == "a::b").expect("a::b");
        assert_eq!(b.alias, None);
        let c = imports.iter().find(|i| i.module_path == "a::c").expect("a::c");
        assert_eq!(c.alias, Some("d".to_string()));
    }

    #[test]
    fn extracts_trait_and_inherent_impls() {
        let plugin = RustPlugin;
        let impls = plugin.extract_impls(test_path(), RICH).unwrap();

        let trait_impl = impls
            .iter()
            .find(|i| i.type_name == "MemStore" && i.trait_name.is_some())
            .expect("trait impl");
        assert_eq!(trait_impl.trait_name, Some("Storage".to_string()));

        let inherent = impls
            .iter()
            .find(|i| i.type_name == "MemStore" && i.trait_name.is_none())
            .expect("inherent impl");
        assert_eq!(inherent.trait_name, None);
    }

    #[test]
    fn extracts_method_call_reference() {
        let plugin = RustPlugin;
        let refs = plugin.extract_references(test_path(), RICH).unwrap();
        let m = refs
            .iter()
            .find(|r| r.name == "helper")
            .expect("helper method ref");
        assert_eq!(m.kind, ReferenceKind::Method);
    }

    #[test]
    fn reference_container_is_enclosing_fn_qualified_name() {
        let plugin = RustPlugin;
        let refs = plugin.extract_references(test_path(), RICH).unwrap();
        let greet_call = refs
            .iter()
            .find(|r| r.name == "greet" && r.kind == ReferenceKind::Call)
            .expect("greet call ref");
        assert_eq!(greet_call.container, "crate::auth::login");
    }

    #[test]
    fn struct_definition_name_is_not_a_type_reference() {
        let plugin = RustPlugin;
        let refs = plugin.extract_references(test_path(), RICH).unwrap();
        // The struct definition sits on line 4; its name must not be a Type ref.
        let def_as_type = refs
            .iter()
            .any(|r| r.name == "MemStore" && r.kind == ReferenceKind::Type && r.start_line == 4);
        assert!(!def_as_type, "struct definition name double-counted as Type");
        // But genuine uses of the type are still captured as Type references.
        let has_type_use =
            refs.iter().any(|r| r.name == "MemStore" && r.kind == ReferenceKind::Type);
        assert!(has_type_use, "type usage should be captured");
    }
}
