//! Go language plugin: Tree-sitter based symbol and reference extraction.
//!
//! `module_path` is the package name from the file's `package` clause (e.g.
//! `auth`). Go has no `impl Trait for Type` form — [`LanguagePlugin::extract_impls`]
//! stays at the default empty result; interface satisfaction is future work.

use super::LanguagePlugin;
use crate::error::{Result, SecondBrainError};
use crate::graph::types::{Import, Reference, ReferenceKind, Symbol, SymbolKind};
use std::path::PathBuf;
use tree_sitter::{Node, Parser, Tree};

/// Extractor for Go source using Tree-sitter.
pub struct GoPlugin;

impl GoPlugin {
    fn parse(source: &str) -> Result<Tree> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .map_err(|e| SecondBrainError::TreeSitter(e.to_string()))?;
        parser.parse(source, None).ok_or(SecondBrainError::Parse)
    }
}

impl LanguagePlugin for GoPlugin {
    fn extensions(&self) -> &[&str] {
        &["go"]
    }

    fn extract_symbols(&self, source_code: &str) -> Result<Vec<Symbol>> {
        let tree = Self::parse(source_code)?;
        let src = source_code.as_bytes();
        let package = package_name(tree.root_node(), src)?.unwrap_or_default();
        let mut out = Vec::new();
        walk_symbols(tree.root_node(), src, &package, &mut out)?;
        Ok(out)
    }

    fn extract_references(&self, source_code: &str) -> Result<Vec<Reference>> {
        let tree = Self::parse(source_code)?;
        let src = source_code.as_bytes();
        let package = package_name(tree.root_node(), src)?.unwrap_or_default();
        let mut scope: Vec<String> = Vec::new();
        let mut out = Vec::new();
        walk_references(tree.root_node(), src, &package, &mut scope, &mut out)?;
        Ok(out)
    }

    fn extract_imports(&self, source_code: &str) -> Result<Vec<Import>> {
        let tree = Self::parse(source_code)?;
        let src = source_code.as_bytes();
        let mut out = Vec::new();
        walk_imports(tree.root_node(), src, &mut out)?;
        Ok(out)
    }
}

fn node_text<'a>(node: Node, src: &'a [u8]) -> Result<&'a str> {
    node.utf8_text(src)
        .map_err(|e| SecondBrainError::TreeSitter(e.to_string()))
}

fn package_name(root: Node, src: &[u8]) -> Result<Option<String>> {
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() == "package_clause" {
            let mut pc = child.walk();
            for id in child.named_children(&mut pc) {
                if id.kind() == "package_identifier" {
                    return Ok(Some(node_text(id, src)?.to_string()));
                }
            }
        }
    }
    Ok(None)
}

fn qualify_scope(package: &str, scope: &[String]) -> String {
    if scope.is_empty() {
        String::new()
    } else if package.is_empty() {
        scope.join("::")
    } else {
        format!("{package}::{}", scope.join("::"))
    }
}

fn walk_symbols(node: Node, src: &[u8], package: &str, out: &mut Vec<Symbol>) -> Result<()> {
    match node.kind() {
        "function_declaration" => {
            emit_named_symbol(node, SymbolKind::Function, package, out, src)?;
        }
        "method_declaration" => {
            emit_named_symbol(node, SymbolKind::Function, package, out, src)?;
        }
        "type_spec" => {
            emit_type_spec(node, package, out, src)?;
        }
        "type_alias" => {
            // `type A = B`
            if let Some(name) = node.child_by_field_name("name") {
                push_symbol(name, SymbolKind::Other("type".into()), package, out, src)?;
            }
        }
        "const_spec" => {
            emit_const_names(node, package, out, src)?;
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_symbols(child, src, package, out)?;
    }
    Ok(())
}

fn emit_type_spec(
    node: Node,
    package: &str,
    out: &mut Vec<Symbol>,
    src: &[u8],
) -> Result<()> {
    let Some(name) = node.child_by_field_name("name") else {
        return Ok(());
    };
    let kind = match node.child_by_field_name("type") {
        Some(ty) if ty.kind() == "struct_type" => SymbolKind::Struct,
        Some(ty) if ty.kind() == "interface_type" => SymbolKind::Trait,
        _ => SymbolKind::Other("type".into()),
    };
    push_symbol(name, kind, package, out, src)
}

fn emit_const_names(
    node: Node,
    package: &str,
    out: &mut Vec<Symbol>,
    src: &[u8],
) -> Result<()> {
    // `name` field may list multiple identifiers (`const a, b = …`).
    if let Some(name_field) = node.child_by_field_name("name") {
        if name_field.kind() == "identifier" {
            push_symbol(name_field, SymbolKind::Const, package, out, src)?;
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "identifier" && child != name_field {
                let before_value = match node.child_by_field_name("value") {
                    Some(v) => child.start_byte() < v.start_byte(),
                    None => true,
                };
                let before_type = match node.child_by_field_name("type") {
                    Some(t) => child.start_byte() < t.start_byte(),
                    None => true,
                };
                if before_value && before_type {
                    push_symbol(child, SymbolKind::Const, package, out, src)?;
                }
            }
        }
    }
    Ok(())
}

fn emit_named_symbol(
    node: Node,
    kind: SymbolKind,
    package: &str,
    out: &mut Vec<Symbol>,
    src: &[u8],
) -> Result<()> {
    if let Some(name) = node.child_by_field_name("name") {
        if matches!(name.kind(), "identifier" | "field_identifier") {
            push_symbol(name, kind, package, out, src)?;
        }
    }
    Ok(())
}

fn push_symbol(
    name_node: Node,
    kind: SymbolKind,
    package: &str,
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
        module_path: package.to_string(),
    });
    Ok(())
}

fn walk_references(
    node: Node,
    src: &[u8],
    package: &str,
    scope: &mut Vec<String>,
    out: &mut Vec<Reference>,
) -> Result<()> {
    match node.kind() {
        "function_declaration" | "method_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                if matches!(name.kind(), "identifier" | "field_identifier") {
                    let text = node_text(name, src)?;
                    scope.push(text.to_string());
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        walk_references(child, src, package, scope, out)?;
                    }
                    scope.pop();
                    return Ok(());
                }
            }
        }
        "call_expression" => {
            if let Some(func) = node.child_by_field_name("function") {
                emit_call_reference(func, src, package, scope, out)?;
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_references(child, src, package, scope, out)?;
    }
    Ok(())
}

fn emit_call_reference(
    func: Node,
    src: &[u8],
    package: &str,
    scope: &[String],
    out: &mut Vec<Reference>,
) -> Result<()> {
    match func.kind() {
        "identifier" => {
            let text = node_text(func, src)?;
            push_reference(text.to_string(), func, ReferenceKind::Call, package, scope, out);
        }
        "selector_expression" => {
            if let Some(field) = func.child_by_field_name("field") {
                if matches!(field.kind(), "field_identifier" | "identifier") {
                    let text = node_text(field, src)?;
                    push_reference(
                        text.to_string(),
                        field,
                        ReferenceKind::Method,
                        package,
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

fn push_reference(
    name: String,
    node: Node,
    kind: ReferenceKind,
    package: &str,
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
        container: qualify_scope(package, scope),
    });
}

fn walk_imports(node: Node, src: &[u8], out: &mut Vec<Import>) -> Result<()> {
    if node.kind() == "import_spec" {
        let path = match node.child_by_field_name("path") {
            Some(p) => strip_quotes(node_text(p, src)?),
            None => return Ok(()),
        };
        let alias = match node.child_by_field_name("name") {
            Some(n) if n.kind() == "package_identifier" => Some(node_text(n, src)?.to_string()),
            Some(n) if n.kind() == "blank_identifier" || n.kind() == "dot" => {
                Some(node_text(n, src)?.to_string())
            }
            _ => None,
        };
        out.push(Import {
            module_path: path,
            alias,
            file: PathBuf::new(),
        });
        return Ok(());
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_imports(child, src, out)?;
    }
    Ok(())
}

fn strip_quotes(s: &str) -> String {
    s.trim_matches(|c| c == '"' || c == '`').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = r#"
package auth

import (
        "fmt"
        h "helper"
)

type Storage interface {
        Get(key string) string
}

type User struct {
        Name string
}

const MaxRetries = 3

func CreateOrder() {}

func (u User) Login() {
        CreateOrder()
        fmt.Println("hi")
        h.Run()
}
"#;

    #[test]
    fn extracts_package_aware_funcs_types_and_const() {
        let plugin = GoPlugin;
        let syms = plugin.extract_symbols(SOURCE).unwrap();
        let find = |n: &str| syms.iter().find(|s| s.name == n).cloned();

        let create = find("CreateOrder").expect("CreateOrder");
        assert_eq!(create.kind, SymbolKind::Function);
        assert_eq!(create.module_path, "auth");

        assert_eq!(find("Login").unwrap().kind, SymbolKind::Function);
        assert_eq!(find("User").unwrap().kind, SymbolKind::Struct);
        assert_eq!(find("Storage").unwrap().kind, SymbolKind::Trait);
        assert_eq!(find("MaxRetries").unwrap().kind, SymbolKind::Const);
    }

    #[test]
    fn extracts_imports_with_and_without_alias() {
        let plugin = GoPlugin;
        let imports = plugin.extract_imports(SOURCE).unwrap();

        let fmt = imports
            .iter()
            .find(|i| i.module_path == "fmt")
            .expect("fmt import");
        assert_eq!(fmt.alias, None);

        let helper = imports
            .iter()
            .find(|i| i.module_path == "helper")
            .expect("helper import");
        assert_eq!(helper.alias, Some("h".to_string()));
    }

    #[test]
    fn extracts_call_and_selector_references() {
        let plugin = GoPlugin;
        let refs = plugin.extract_references(SOURCE).unwrap();

        let call = refs
            .iter()
            .find(|r| r.name == "CreateOrder" && r.kind == ReferenceKind::Call)
            .expect("CreateOrder call");
        assert_eq!(call.container, "auth::Login");

        let println = refs
            .iter()
            .find(|r| r.name == "Println" && r.kind == ReferenceKind::Method)
            .expect("Println selector call");
        assert_eq!(println.kind, ReferenceKind::Method);

        let run = refs
            .iter()
            .find(|r| r.name == "Run" && r.kind == ReferenceKind::Method)
            .expect("Run selector call");
        assert_eq!(run.kind, ReferenceKind::Method);
    }

    #[test]
    fn extension_is_go() {
        assert_eq!(GoPlugin.extensions(), &["go"]);
    }
}
