//! TypeScript/TSX language plugin: Tree-sitter based symbol and reference extraction.
//!
//! `module_path` is derived from the file path with the extension stripped
//! (e.g. `src/auth/service`), using forward slashes.
//!
//! `.ts`/`.mts`/`.cts` parse with the TypeScript grammar; `.tsx` uses the TSX
//! grammar. Extraction walks are shared.

use super::{file_path_key, path_module_identity, LanguagePlugin};
use crate::error::{Result, KeelError};
use crate::graph::types::{Import, Reference, ReferenceKind, Symbol, SymbolKind};
use std::path::{Path, PathBuf};
use tree_sitter::{Language, Node, Parser, Tree};

/// Extractor for TypeScript and TSX source using Tree-sitter.
pub struct TypeScriptPlugin;

impl TypeScriptPlugin {
    fn parse_ts(source: &str) -> Result<Tree> {
        Self::parse_with(source, tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
    }

    fn parse_tsx(source: &str) -> Result<Tree> {
        Self::parse_with(source, tree_sitter_typescript::LANGUAGE_TSX.into())
    }

    fn parse_with(source: &str, language: Language) -> Result<Tree> {
        let mut parser = Parser::new();
        parser
            .set_language(&language)
            .map_err(|e| KeelError::TreeSitter(e.to_string()))?;
        parser.parse(source, None).ok_or(KeelError::Parse)
    }
}

/// Internal TSX-only plugin so `.tsx` uses the TSX grammar while sharing walks.
struct TsxPlugin;

impl LanguagePlugin for TypeScriptPlugin {
    fn extensions(&self) -> &[&str] {
        &["ts", "mts", "cts"]
    }

    fn extract_symbols(&self, path: &Path, source_code: &str) -> Result<Vec<Symbol>> {
        let tree = Self::parse_ts(source_code)?;
        let src = source_code.as_bytes();
        let module_path = path_module_identity(path);
        let mut out = Vec::new();
        walk_symbols(tree.root_node(), src, &module_path, &mut out)?;
        Ok(out)
    }

    fn extract_references(&self, path: &Path, source_code: &str) -> Result<Vec<Reference>> {
        let tree = Self::parse_ts(source_code)?;
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

    fn extract_imports(&self, _path: &Path, source_code: &str) -> Result<Vec<Import>> {
        let tree = Self::parse_ts(source_code)?;
        let src = source_code.as_bytes();
        let mut out = Vec::new();
        walk_imports(tree.root_node(), src, &mut out)?;
        Ok(out)
    }
}

impl LanguagePlugin for TsxPlugin {
    fn extensions(&self) -> &[&str] {
        &["tsx"]
    }

    fn extract_symbols(&self, path: &Path, source_code: &str) -> Result<Vec<Symbol>> {
        let tree = TypeScriptPlugin::parse_tsx(source_code)?;
        let src = source_code.as_bytes();
        let module_path = path_module_identity(path);
        let mut out = Vec::new();
        walk_symbols(tree.root_node(), src, &module_path, &mut out)?;
        Ok(out)
    }

    fn extract_references(&self, path: &Path, source_code: &str) -> Result<Vec<Reference>> {
        let tree = TypeScriptPlugin::parse_tsx(source_code)?;
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

    fn extract_imports(&self, _path: &Path, source_code: &str) -> Result<Vec<Import>> {
        let tree = TypeScriptPlugin::parse_tsx(source_code)?;
        let src = source_code.as_bytes();
        let mut out = Vec::new();
        walk_imports(tree.root_node(), src, &mut out)?;
        Ok(out)
    }
}

/// Register both the TypeScript and TSX plugins into `plugins`.
pub(crate) fn register(plugins: &mut Vec<Box<dyn LanguagePlugin>>) {
    plugins.push(Box::new(TypeScriptPlugin));
    plugins.push(Box::new(TsxPlugin));
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
        "function_declaration" | "generator_function_declaration" => {
            emit_named_symbol(node, SymbolKind::Function, module_path, out, src)?;
        }
        "class_declaration" | "abstract_class_declaration" => {
            emit_named_symbol(node, SymbolKind::Struct, module_path, out, src)?;
        }
        "interface_declaration" => {
            emit_named_symbol(node, SymbolKind::Trait, module_path, out, src)?;
        }
        "type_alias_declaration" => {
            emit_named_symbol(node, SymbolKind::Other("type".into()), module_path, out, src)?;
        }
        "enum_declaration" => {
            emit_named_symbol(node, SymbolKind::Enum, module_path, out, src)?;
        }
        "method_definition" => {
            // Constructors are still useful as Function symbols for name lookup.
            emit_named_symbol(node, SymbolKind::Function, module_path, out, src)?;
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_symbols(child, src, module_path, out)?;
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
        // Method names may be `property_identifier` or `identifier`.
        if matches!(
            name.kind(),
            "identifier" | "property_identifier" | "type_identifier"
        ) {
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
        | "abstract_class_declaration"
        | "method_definition" => {
            if let Some(name) = node.child_by_field_name("name") {
                if matches!(
                    name.kind(),
                    "identifier" | "property_identifier" | "type_identifier"
                ) {
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
        "call_expression" => {
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
    if node.kind() == "import_statement" {
        let source = match node.child_by_field_name("source") {
            Some(s) => strip_quotes(node_text(s, src)?),
            None => {
                // Still walk children for nested forms; nothing to emit without source.
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    walk_imports(child, src, out)?;
                }
                return Ok(());
            }
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
            // Side-effect import: `import './polyfill'`.
            out.push(Import {
                module_path: source,
                alias: None,
                file: PathBuf::new(),
            });
        }
        return Ok(());
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_imports(child, src, out)?;
    }
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
                // Default import: `import Foo from '…'`.
                let alias = node_text(child, src)?.to_string();
                out.push(Import {
                    module_path: source.to_string(),
                    alias: Some(alias),
                    file: PathBuf::new(),
                });
            }
            "namespace_import" => {
                // `import * as ns from '…'`.
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
                        // module_path is the source module; name is recoverable
                        // via alias when present, else the imported binding.
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

    const SOURCE: &str = "\
import { helper as h } from './util';
import Auth from './auth';

export interface Storage {
  get(key: string): string;
}

export type UserId = string;

export enum Role {
  Admin,
  User,
}

export class AuthService {
  login(): void {
    h();
    this.refresh();
  }
  refresh(): void {}
}

export function createOrder(): void {}

function run(): void {
  createOrder();
}
";

    fn test_path() -> &'static Path {
        Path::new("src/auth/service.ts")
    }

    #[test]
    fn extracts_class_interface_function_type_enum_and_methods() {
        let plugin = TypeScriptPlugin;
        let syms = plugin.extract_symbols(test_path(), SOURCE).unwrap();
        let find = |n: &str| syms.iter().find(|s| s.name == n).cloned();

        let auth = find("AuthService").expect("AuthService");
        assert_eq!(auth.kind, SymbolKind::Struct);
        assert_eq!(auth.module_path, "src/auth/service");

        assert_eq!(find("Storage").unwrap().kind, SymbolKind::Trait);
        assert_eq!(
            find("UserId").unwrap().kind,
            SymbolKind::Other("type".into())
        );
        assert_eq!(find("Role").unwrap().kind, SymbolKind::Enum);
        assert_eq!(find("createOrder").unwrap().kind, SymbolKind::Function);
        assert_eq!(find("login").unwrap().kind, SymbolKind::Function);
        assert_eq!(find("refresh").unwrap().kind, SymbolKind::Function);
    }

    #[test]
    fn different_files_get_different_module_paths() {
        let plugin = TypeScriptPlugin;
        let a = plugin
            .extract_symbols(Path::new("src/a.ts"), "export function f() {}\n")
            .unwrap();
        let b = plugin
            .extract_symbols(Path::new("src/b.ts"), "export function f() {}\n")
            .unwrap();
        assert_eq!(a[0].module_path, "src/a");
        assert_eq!(b[0].module_path, "src/b");
        assert_ne!(a[0].module_path, b[0].module_path);
    }

    #[test]
    fn extracts_named_and_default_imports() {
        let plugin = TypeScriptPlugin;
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
    }

    #[test]
    fn extracts_call_and_method_references() {
        let plugin = TypeScriptPlugin;
        let refs = plugin.extract_references(test_path(), SOURCE).unwrap();

        let call = refs
            .iter()
            .find(|r| r.name == "createOrder" && r.kind == ReferenceKind::Call)
            .expect("createOrder call");
        assert!(call.start_line > 0);

        let method = refs
            .iter()
            .find(|r| r.name == "refresh" && r.kind == ReferenceKind::Method)
            .expect("refresh method call");
        assert_eq!(method.kind, ReferenceKind::Method);

        let helper = refs.iter().find(|r| r.name == "h").expect("h call");
        assert_eq!(helper.kind, ReferenceKind::Call);
    }

    #[test]
    fn extensions_cover_typescript_variants() {
        let plugin = TypeScriptPlugin;
        assert_eq!(plugin.extensions(), &["ts", "mts", "cts"]);
        let tsx = TsxPlugin;
        assert_eq!(tsx.extensions(), &["tsx"]);
    }
}
