//! Python language plugin: Tree-sitter based symbol and reference extraction.
//!
//! `module_path` is the source path with the extension stripped and separators
//! converted to dots (e.g. `pkg/auth/service.py` → `pkg.auth.service`).
//!
//! Handles `.py` and `.pyi`. Class bases are not recorded as trait
//! implementations in v1.1 (`extract_impls` stays empty).

use super::{file_path_key, LanguagePlugin};
use crate::error::{Result, KeelError};
use crate::graph::types::{Import, Reference, ReferenceKind, Symbol, SymbolKind};
use std::path::{Path, PathBuf};
use tree_sitter::{Node, Parser, Tree};

/// Extractor for Python / stub source using Tree-sitter.
pub struct PythonPlugin;

impl PythonPlugin {
    fn parse(source: &str) -> Result<Tree> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .map_err(|e| KeelError::TreeSitter(e.to_string()))?;
        parser.parse(source, None).ok_or(KeelError::Parse)
    }
}

impl LanguagePlugin for PythonPlugin {
    fn extensions(&self) -> &[&str] {
        &["py", "pyi"]
    }

    fn extract_symbols(&self, path: &Path, source_code: &str) -> Result<Vec<Symbol>> {
        let tree = Self::parse(source_code)?;
        let src = source_code.as_bytes();
        let module_path = python_module_identity(path);
        let mut out = Vec::new();
        walk_symbols(tree.root_node(), src, &module_path, true, &mut out)?;
        Ok(out)
    }

    fn extract_references(&self, path: &Path, source_code: &str) -> Result<Vec<Reference>> {
        let tree = Self::parse(source_code)?;
        let src = source_code.as_bytes();
        let file_key = file_path_key(path);
        let module_path = python_module_identity(path);
        let mut scope: Vec<String> = Vec::new();
        let mut out = Vec::new();
        walk_references(
            tree.root_node(),
            src,
            &file_key,
            &module_path,
            &mut scope,
            &mut out,
        )?;
        Ok(out)
    }

    fn extract_imports(&self, _path: &Path, source_code: &str) -> Result<Vec<Import>> {
        let tree = Self::parse(source_code)?;
        let src = source_code.as_bytes();
        let mut out = Vec::new();
        walk_imports(tree.root_node(), src, &mut out)?;
        Ok(out)
    }
}

/// Path → dotted Python-style module identity.
pub fn python_module_identity(path: &Path) -> String {
    path.with_extension("")
        .to_string_lossy()
        .replace('\\', "/")
        .replace('/', ".")
}

fn node_text<'a>(node: Node, src: &'a [u8]) -> Result<&'a str> {
    node.utf8_text(src)
        .map_err(|e| KeelError::TreeSitter(e.to_string()))
}

fn qualify_scope(file_key: &str, module_path: &str, scope: &[String]) -> String {
    if scope.is_empty() {
        file_key.to_string()
    } else {
        format!("{module_path}::{}", scope.join("::"))
    }
}

fn walk_symbols(
    node: Node,
    src: &[u8],
    module_path: &str,
    module_scope: bool,
    out: &mut Vec<Symbol>,
) -> Result<()> {
    match node.kind() {
        "function_definition" => {
            emit_named_symbol(node, SymbolKind::Function, module_path, out, src)?;
            // Nested functions are still walked via children.
        }
        "class_definition" => {
            emit_named_symbol(node, SymbolKind::Struct, module_path, out, src)?;
        }
        "assignment" | "augmented_assignment" if module_scope => {
            emit_module_constant(node, module_path, out, src)?;
        }
        "decorated_definition" => {
            // Walk into the wrapped definition; do not treat decorator as scope.
        }
        _ => {}
    }

    let next_module_scope = match node.kind() {
        "function_definition" | "class_definition" => false,
        _ => module_scope,
    };

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_symbols(child, src, module_path, next_module_scope, out)?;
    }
    Ok(())
}

fn emit_module_constant(
    node: Node,
    module_path: &str,
    out: &mut Vec<Symbol>,
    src: &[u8],
) -> Result<()> {
    let Some(left) = node.child_by_field_name("left") else {
        return Ok(());
    };
    // Only simple identifiers that look constant-like (ALL_CAPS).
    if left.kind() != "identifier" {
        return Ok(());
    }
    let name = node_text(left, src)?;
    if !is_constant_like(name) {
        return Ok(());
    }
    push_symbol(left, SymbolKind::Const, module_path, out, src)
}

fn is_constant_like(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
        && name.chars().any(|c| c.is_ascii_alphabetic())
}

fn emit_named_symbol(
    node: Node,
    kind: SymbolKind,
    module_path: &str,
    out: &mut Vec<Symbol>,
    src: &[u8],
) -> Result<()> {
    if let Some(name) = node.child_by_field_name("name") {
        if name.kind() == "identifier" {
            push_symbol(name, kind, module_path, out, src)?;
        }
    }
    Ok(())
}

fn push_symbol(
    name_node: Node,
    kind: SymbolKind,
    module_path: &str,
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
        module_path: module_path.to_string(),
    });
    Ok(())
}

fn walk_references(
    node: Node,
    src: &[u8],
    file_key: &str,
    module_path: &str,
    scope: &mut Vec<String>,
    out: &mut Vec<Reference>,
) -> Result<()> {
    match node.kind() {
        "function_definition" | "class_definition" => {
            if let Some(name) = node.child_by_field_name("name") {
                if name.kind() == "identifier" {
                    let text = node_text(name, src)?;
                    scope.push(text.to_string());
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        walk_references(child, src, file_key, module_path, scope, out)?;
                    }
                    scope.pop();
                    return Ok(());
                }
            }
        }
        "call" => {
            if let Some(func) = node.child_by_field_name("function") {
                emit_call_reference(func, src, file_key, module_path, scope, out)?;
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_references(child, src, file_key, module_path, scope, out)?;
    }
    Ok(())
}

fn emit_call_reference(
    func: Node,
    src: &[u8],
    file_key: &str,
    module_path: &str,
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
                module_path,
                scope,
                out,
            );
        }
        "attribute" => {
            if let Some(attr) = func.child_by_field_name("attribute") {
                if attr.kind() == "identifier" {
                    let text = node_text(attr, src)?;
                    push_reference(
                        text.to_string(),
                        attr,
                        ReferenceKind::Method,
                        file_key,
                        module_path,
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
    file_key: &str,
    module_path: &str,
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
        container: qualify_scope(file_key, module_path, scope),
    });
}

fn walk_imports(node: Node, src: &[u8], out: &mut Vec<Import>) -> Result<()> {
    match node.kind() {
        "import_statement" => emit_import_statement(node, src, out)?,
        "import_from_statement" => emit_import_from(node, src, out)?,
        "future_import_statement" => {}
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                walk_imports(child, src, out)?;
            }
        }
    }
    Ok(())
}

fn emit_import_statement(node: Node, src: &[u8], out: &mut Vec<Import>) -> Result<()> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "dotted_name" => {
                let module_path = node_text(child, src)?.to_string();
                out.push(Import {
                    module_path,
                    alias: None,
                    file: PathBuf::new(),
                });
            }
            "aliased_import" => {
                let name = child
                    .child_by_field_name("name")
                    .map(|n| node_text(n, src))
                    .transpose()?;
                let alias = child
                    .child_by_field_name("alias")
                    .map(|n| node_text(n, src).map(|s| s.to_string()))
                    .transpose()?;
                if let Some(module_path) = name {
                    out.push(Import {
                        module_path: module_path.to_string(),
                        alias,
                        file: PathBuf::new(),
                    });
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn emit_import_from(node: Node, src: &[u8], out: &mut Vec<Import>) -> Result<()> {
    let module_path = resolve_from_module(node, src)?;
    let mut emitted = false;

    let mut cursor = node.walk();
    for child in node.children_by_field_name("name", &mut cursor) {
        match child.kind() {
            "aliased_import" => {
                let name = child
                    .child_by_field_name("name")
                    .map(|n| node_text(n, src))
                    .transpose()?;
                let alias = child
                    .child_by_field_name("alias")
                    .map(|n| node_text(n, src).map(|s| s.to_string()))
                    .transpose()?;
                if let Some(name) = name {
                    out.push(Import {
                        module_path: format!("{module_path}::{name}"),
                        alias,
                        file: PathBuf::new(),
                    });
                    emitted = true;
                }
            }
            "dotted_name" | "identifier" => {
                let name = node_text(child, src)?;
                out.push(Import {
                    module_path: format!("{module_path}::{name}"),
                    alias: None,
                    file: PathBuf::new(),
                });
                emitted = true;
            }
            _ => {}
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "wildcard_import" {
            out.push(Import {
                module_path: format!("{module_path}::*"),
                alias: None,
                file: PathBuf::new(),
            });
            emitted = true;
        }
    }

    if !emitted {
        out.push(Import {
            module_path,
            alias: None,
            file: PathBuf::new(),
        });
    }
    Ok(())
}

fn resolve_from_module(node: Node, src: &[u8]) -> Result<String> {
    if let Some(module) = node.child_by_field_name("module_name") {
        return Ok(node_text(module, src)?.to_string());
    }
    Ok(String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = r#"
import os
import sys as system
from .util import helper as h
from pkg.auth import AuthService
from typing import *

MAX_RETRIES = 3
local_var = 1

class AuthService:
    def login(self):
        h()
        self.refresh()
        AuthService()

    def refresh(self):
        pass

async def create_order():
    pass

def run():
    create_order()
"#;

    fn test_path() -> &'static Path {
        Path::new("pkg/auth/service.py")
    }

    #[test]
    fn extracts_class_functions_methods_async_and_constants() {
        let plugin = PythonPlugin;
        let syms = plugin.extract_symbols(test_path(), SOURCE).unwrap();
        let find = |n: &str| syms.iter().find(|s| s.name == n).cloned();

        assert_eq!(find("AuthService").unwrap().kind, SymbolKind::Struct);
        assert_eq!(
            find("AuthService").unwrap().module_path,
            "pkg.auth.service"
        );
        assert_eq!(find("create_order").unwrap().kind, SymbolKind::Function);
        assert_eq!(find("login").unwrap().kind, SymbolKind::Function);
        assert_eq!(find("refresh").unwrap().kind, SymbolKind::Function);
        assert_eq!(find("run").unwrap().kind, SymbolKind::Function);
        assert_eq!(find("MAX_RETRIES").unwrap().kind, SymbolKind::Const);
        assert!(find("local_var").is_none());
    }

    #[test]
    fn extracts_imports_including_relative_and_aliases() {
        let plugin = PythonPlugin;
        let imports = plugin.extract_imports(test_path(), SOURCE).unwrap();

        assert!(imports.iter().any(|i| i.module_path == "os" && i.alias.is_none()));
        let sys = imports
            .iter()
            .find(|i| i.module_path == "sys")
            .expect("sys import");
        assert_eq!(sys.alias, Some("system".to_string()));

        let helper = imports
            .iter()
            .find(|i| i.module_path == ".util::helper")
            .expect("relative helper");
        assert_eq!(helper.alias, Some("h".to_string()));

        assert!(imports
            .iter()
            .any(|i| i.module_path == "pkg.auth::AuthService"));
        assert!(imports.iter().any(|i| i.module_path == "typing::*"));
    }

    #[test]
    fn extracts_call_and_method_references() {
        let plugin = PythonPlugin;
        let refs = plugin.extract_references(test_path(), SOURCE).unwrap();

        assert!(refs
            .iter()
            .any(|r| r.name == "create_order" && r.kind == ReferenceKind::Call));
        assert!(refs
            .iter()
            .any(|r| r.name == "refresh" && r.kind == ReferenceKind::Method));
        assert!(refs.iter().any(|r| r.name == "h"));
        assert!(refs
            .iter()
            .any(|r| r.name == "AuthService" && r.kind == ReferenceKind::Call));
    }

    #[test]
    fn python_module_identity_uses_dots() {
        assert_eq!(
            python_module_identity(Path::new("pkg/auth/service.py")),
            "pkg.auth.service"
        );
    }

    #[test]
    fn pyi_stubs_are_supported() {
        let plugin = PythonPlugin;
        assert_eq!(plugin.extensions(), &["py", "pyi"]);
        let syms = plugin
            .extract_symbols(Path::new("pkg/types.pyi"), "class Widget: ...\n")
            .unwrap();
        assert_eq!(syms[0].name, "Widget");
    }
}
