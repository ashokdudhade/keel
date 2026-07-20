//! JavaScript/JSX language plugin: Tree-sitter based symbol and reference extraction.
//!
//! `module_path` is derived from the file path with the extension stripped
//! (e.g. `src/auth/service`), using forward slashes.
//!
//! `.js`/`.mjs`/`.cjs` and `.jsx` share the JavaScript grammar (which includes
//! JSX). Separate plugin registrations keep extension dispatch explicit.

use super::{file_path_key, path_module_identity, LanguagePlugin};
use crate::error::{Result, KeelError};
use crate::graph::types::{Import, Reference, ReferenceKind, Symbol, SymbolKind};
use std::path::{Path, PathBuf};
use tree_sitter::{Node, Parser, Tree};

/// Extractor for JavaScript source using Tree-sitter.
pub struct JavaScriptPlugin;

impl JavaScriptPlugin {
    fn parse(source: &str) -> Result<Tree> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_javascript::LANGUAGE.into())
            .map_err(|e| KeelError::TreeSitter(e.to_string()))?;
        parser.parse(source, None).ok_or(KeelError::Parse)
    }
}

/// Internal JSX-only plugin so `.jsx` is registered separately.
struct JsxPlugin;

impl LanguagePlugin for JavaScriptPlugin {
    fn extensions(&self) -> &[&str] {
        &["js", "mjs", "cjs"]
    }

    fn extract_symbols(&self, path: &Path, source_code: &str) -> Result<Vec<Symbol>> {
        extract_symbols(path, source_code)
    }

    fn extract_references(&self, path: &Path, source_code: &str) -> Result<Vec<Reference>> {
        extract_references(path, source_code)
    }

    fn extract_imports(&self, path: &Path, source_code: &str) -> Result<Vec<Import>> {
        extract_imports(path, source_code)
    }
}

impl LanguagePlugin for JsxPlugin {
    fn extensions(&self) -> &[&str] {
        &["jsx"]
    }

    fn extract_symbols(&self, path: &Path, source_code: &str) -> Result<Vec<Symbol>> {
        extract_symbols(path, source_code)
    }

    fn extract_references(&self, path: &Path, source_code: &str) -> Result<Vec<Reference>> {
        extract_references(path, source_code)
    }

    fn extract_imports(&self, path: &Path, source_code: &str) -> Result<Vec<Import>> {
        extract_imports(path, source_code)
    }
}

/// Register both the JavaScript and JSX plugins into `plugins`.
pub(crate) fn register(plugins: &mut Vec<Box<dyn LanguagePlugin>>) {
    plugins.push(Box::new(JavaScriptPlugin));
    plugins.push(Box::new(JsxPlugin));
}

fn extract_symbols(path: &Path, source_code: &str) -> Result<Vec<Symbol>> {
    let tree = JavaScriptPlugin::parse(source_code)?;
    let src = source_code.as_bytes();
    let module_path = path_module_identity(path);
    let mut out = Vec::new();
    walk_symbols(tree.root_node(), src, &module_path, &mut out)?;
    Ok(out)
}

fn extract_references(path: &Path, source_code: &str) -> Result<Vec<Reference>> {
    let tree = JavaScriptPlugin::parse(source_code)?;
    let src = source_code.as_bytes();
    let file_key = file_path_key(path);
    let module_path = path_module_identity(path);
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

fn extract_imports(_path: &Path, source_code: &str) -> Result<Vec<Import>> {
    let tree = JavaScriptPlugin::parse(source_code)?;
    let src = source_code.as_bytes();
    let mut out = Vec::new();
    walk_imports(tree.root_node(), src, &mut out)?;
    Ok(out)
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
    out: &mut Vec<Symbol>,
) -> Result<()> {
    match node.kind() {
        "function_declaration" | "generator_function_declaration" | "class_declaration" => {
            let kind = if node.kind() == "class_declaration" {
                SymbolKind::Struct
            } else {
                SymbolKind::Function
            };
            emit_named_symbol(node, kind, module_path, out, src)?;
        }
        "method_definition" => {
            emit_named_symbol(node, SymbolKind::Function, module_path, out, src)?;
        }
        "variable_declarator" => {
            emit_bound_function(node, module_path, out, src)?;
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_symbols(child, src, module_path, out)?;
    }
    Ok(())
}

fn emit_bound_function(
    declarator: Node,
    module_path: &str,
    out: &mut Vec<Symbol>,
    src: &[u8],
) -> Result<()> {
    let Some(value) = declarator.child_by_field_name("value") else {
        return Ok(());
    };
    if !matches!(
        value.kind(),
        "arrow_function" | "function_expression" | "generator_function"
    ) {
        return Ok(());
    }
    let Some(name) = declarator.child_by_field_name("name") else {
        return Ok(());
    };
    if name.kind() == "identifier" {
        push_symbol(name, SymbolKind::Function, module_path, out, src)?;
    }
    Ok(())
}

fn emit_named_symbol(
    node: Node,
    kind: SymbolKind,
    module_path: &str,
    out: &mut Vec<Symbol>,
    src: &[u8],
) -> Result<()> {
    if let Some(name) = node.child_by_field_name("name") {
        if matches!(name.kind(), "identifier" | "property_identifier") {
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
        "function_declaration"
        | "generator_function_declaration"
        | "class_declaration"
        | "method_definition" => {
            if let Some(name) = node.child_by_field_name("name") {
                if matches!(name.kind(), "identifier" | "property_identifier") {
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
        "variable_declarator" => {
            if let Some(value) = node.child_by_field_name("value") {
                if matches!(
                    value.kind(),
                    "arrow_function" | "function_expression" | "generator_function"
                ) {
                    if let Some(name) = node.child_by_field_name("name") {
                        if name.kind() == "identifier" {
                            let text = node_text(name, src)?;
                            scope.push(text.to_string());
                            walk_references(value, src, file_key, module_path, scope, out)?;
                            scope.pop();
                            return Ok(());
                        }
                    }
                }
            }
        }
        "call_expression" => {
            if let Some(func) = node.child_by_field_name("function") {
                emit_call_reference(func, src, file_key, module_path, scope, out)?;
            }
        }
        "new_expression" => {
            if let Some(ctor) = node.child_by_field_name("constructor") {
                emit_call_reference(ctor, src, file_key, module_path, scope, out)?;
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
        "member_expression" => {
            if let Some(prop) = func.child_by_field_name("property") {
                if matches!(prop.kind(), "property_identifier" | "identifier") {
                    let text = node_text(prop, src)?;
                    push_reference(
                        text.to_string(),
                        prop,
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
        "import_statement" => {
            emit_esm_import(node, src, out)?;
            return Ok(());
        }
        "variable_declarator" => {
            emit_require_import(node, src, out)?;
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_imports(child, src, out)?;
    }
    Ok(())
}

fn emit_esm_import(node: Node, src: &[u8], out: &mut Vec<Import>) -> Result<()> {
    let source = match node.child_by_field_name("source") {
        Some(s) => strip_quotes(node_text(s, src)?),
        None => return Ok(()),
    };
    let mut cursor = node.walk();
    let mut emitted = false;
    for child in node.named_children(&mut cursor) {
        if child.kind() == "import_clause" {
            emit_import_clause(child, &source, src, out)?;
            emitted = true;
        }
    }
    if !emitted {
        out.push(Import {
            module_path: source,
            alias: None,
            file: PathBuf::new(),
        });
    }
    Ok(())
}

fn emit_require_import(declarator: Node, src: &[u8], out: &mut Vec<Import>) -> Result<()> {
    let Some(value) = declarator.child_by_field_name("value") else {
        return Ok(());
    };
    if value.kind() != "call_expression" {
        return Ok(());
    }
    let Some(func) = value.child_by_field_name("function") else {
        return Ok(());
    };
    if func.kind() != "identifier" || node_text(func, src)? != "require" {
        return Ok(());
    }
    let Some(args) = value.child_by_field_name("arguments") else {
        return Ok(());
    };
    let mut cursor = args.walk();
    let mut module_path = None;
    for child in args.named_children(&mut cursor) {
        if child.kind() == "string" {
            module_path = Some(strip_quotes(node_text(child, src)?));
            break;
        }
    }
    let Some(module_path) = module_path else {
        return Ok(());
    };
    let alias = match declarator.child_by_field_name("name") {
        Some(name) if name.kind() == "identifier" => Some(node_text(name, src)?.to_string()),
        _ => None,
    };
    out.push(Import {
        module_path,
        alias,
        file: PathBuf::new(),
    });
    Ok(())
}

fn emit_import_clause(
    clause: Node,
    source: &str,
    src: &[u8],
    out: &mut Vec<Import>,
) -> Result<()> {
    let mut cursor = clause.walk();
    for child in clause.named_children(&mut cursor) {
        match child.kind() {
            "identifier" => {
                let alias = node_text(child, src)?.to_string();
                out.push(Import {
                    module_path: source.to_string(),
                    alias: Some(alias),
                    file: PathBuf::new(),
                });
            }
            "namespace_import" => {
                let mut nc = child.walk();
                for id in child.named_children(&mut nc) {
                    if id.kind() == "identifier" {
                        let alias = node_text(id, src)?.to_string();
                        out.push(Import {
                            module_path: source.to_string(),
                            alias: Some(alias),
                            file: PathBuf::new(),
                        });
                    }
                }
            }
            "named_imports" => {
                let mut nc = child.walk();
                for spec in child.named_children(&mut nc) {
                    if spec.kind() == "import_specifier" {
                        let name = match spec.child_by_field_name("name") {
                            Some(n) => node_text(n, src)?,
                            None => continue,
                        };
                        let alias = match spec.child_by_field_name("alias") {
                            Some(a) => Some(node_text(a, src)?.to_string()),
                            None => None,
                        };
                        out.push(Import {
                            module_path: format!("{source}::{name}"),
                            alias,
                            file: PathBuf::new(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn strip_quotes(s: &str) -> String {
    s.trim_matches(|c| c == '\'' || c == '"' || c == '`')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = r#"
import { helper as h } from './util';
import Auth from './auth';
import './polyfill';
const fs = require('fs');

export class AuthService {
  login() {
    h();
    this.refresh();
    new Auth();
  }
  refresh() {}
}

export function createOrder() {}

const run = () => {
  createOrder();
};
"#;

    fn test_path() -> &'static Path {
        Path::new("src/auth/service.js")
    }

    #[test]
    fn extracts_class_function_methods_and_arrows() {
        let plugin = JavaScriptPlugin;
        let syms = plugin.extract_symbols(test_path(), SOURCE).unwrap();
        let find = |n: &str| syms.iter().find(|s| s.name == n).cloned();

        assert_eq!(find("AuthService").unwrap().kind, SymbolKind::Struct);
        assert_eq!(find("AuthService").unwrap().module_path, "src/auth/service");
        assert_eq!(find("createOrder").unwrap().kind, SymbolKind::Function);
        assert_eq!(find("login").unwrap().kind, SymbolKind::Function);
        assert_eq!(find("refresh").unwrap().kind, SymbolKind::Function);
        assert_eq!(find("run").unwrap().kind, SymbolKind::Function);
    }

    #[test]
    fn extracts_esm_and_commonjs_imports() {
        let plugin = JavaScriptPlugin;
        let imports = plugin.extract_imports(test_path(), SOURCE).unwrap();

        let named = imports
            .iter()
            .find(|i| i.module_path == "./util::helper")
            .expect("named helper import");
        assert_eq!(named.alias, Some("h".to_string()));

        let default = imports
            .iter()
            .find(|i| i.module_path == "./auth")
            .expect("default Auth import");
        assert_eq!(default.alias, Some("Auth".to_string()));

        let side = imports
            .iter()
            .find(|i| i.module_path == "./polyfill" && i.alias.is_none())
            .expect("side-effect import");
        assert!(side.alias.is_none());

        let cjs = imports
            .iter()
            .find(|i| i.module_path == "fs")
            .expect("require import");
        assert_eq!(cjs.alias, Some("fs".to_string()));
    }

    #[test]
    fn extracts_call_method_and_constructor_references() {
        let plugin = JavaScriptPlugin;
        let refs = plugin.extract_references(test_path(), SOURCE).unwrap();

        assert!(refs
            .iter()
            .any(|r| r.name == "createOrder" && r.kind == ReferenceKind::Call));
        assert!(refs
            .iter()
            .any(|r| r.name == "refresh" && r.kind == ReferenceKind::Method));
        assert!(refs
            .iter()
            .any(|r| r.name == "Auth" && r.kind == ReferenceKind::Call));
        assert!(refs.iter().any(|r| r.name == "h"));
    }

    #[test]
    fn jsx_plugin_parses_jsx_component() {
        let plugin = JsxPlugin;
        let source = "export function Widget() { return <div/>; }\n";
        let syms = plugin
            .extract_symbols(Path::new("src/Widget.jsx"), source)
            .unwrap();
        assert_eq!(syms[0].name, "Widget");
        assert_eq!(plugin.extensions(), &["jsx"]);
    }

    #[test]
    fn extensions_cover_javascript_variants() {
        assert_eq!(JavaScriptPlugin.extensions(), &["js", "mjs", "cjs"]);
    }
}
