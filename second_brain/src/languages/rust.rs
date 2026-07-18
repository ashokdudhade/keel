//! Rust language plugin: Tree-sitter based symbol and reference extraction.

use super::LanguagePlugin;
use crate::error::{Result, SecondBrainError};
use crate::graph::types::{Reference, Symbol, SymbolKind};
use std::path::PathBuf;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor, Tree};

// Capture name === SymbolKind name, so the mapping in `capture_kind` is trivial.
const SYMBOL_QUERY: &str = r#"
(function_item name: (identifier) @function)
(struct_item name: (type_identifier) @struct)
(trait_item name: (type_identifier) @trait)
(enum_item name: (type_identifier) @enum)
(mod_item name: (identifier) @module)
(const_item name: (identifier) @const)
(impl_item type: (type_identifier) @impl)
"#;

// v0.1 references are call and macro sites. `scoped_identifier name:` captures the
// final segment (e.g. `foo` in `a::b::foo()`), which is what name-based lookup needs.
const REFERENCE_QUERY: &str = r#"
(call_expression function: (identifier) @ref)
(call_expression function: (scoped_identifier name: (identifier) @ref))
(macro_invocation macro: (identifier) @ref)
"#;

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

    fn extract_symbols(&self, source_code: &str) -> Result<Vec<Symbol>> {
        let tree = Self::parse(source_code)?;
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, SYMBOL_QUERY)
            .map_err(|e| SecondBrainError::TreeSitter(e.to_string()))?;
        let names = query.capture_names();

        let mut cursor = QueryCursor::new();
        let mut out = Vec::new();
        let mut matches = cursor.matches(&query, tree.root_node(), source_code.as_bytes());
        while let Some(m) = matches.next() {
            for cap in m.captures {
                let node = cap.node;
                let text = node
                    .utf8_text(source_code.as_bytes())
                    .map_err(|e| SecondBrainError::TreeSitter(e.to_string()))?;
                let pos = node.start_position();
                out.push(Symbol {
                    name: text.to_string(),
                    kind: capture_kind(names[cap.index as usize]),
                    file: PathBuf::new(),
                    start_line: pos.row as u32 + 1,
                    start_col: pos.column as u32 + 1,
                });
            }
        }
        Ok(out)
    }

    fn extract_references(&self, source_code: &str) -> Result<Vec<Reference>> {
        let tree = Self::parse(source_code)?;
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, REFERENCE_QUERY)
            .map_err(|e| SecondBrainError::TreeSitter(e.to_string()))?;

        let mut cursor = QueryCursor::new();
        let mut out = Vec::new();
        let mut matches = cursor.matches(&query, tree.root_node(), source_code.as_bytes());
        while let Some(m) = matches.next() {
            for cap in m.captures {
                let node = cap.node;
                let text = node
                    .utf8_text(source_code.as_bytes())
                    .map_err(|e| SecondBrainError::TreeSitter(e.to_string()))?;
                let pos = node.start_position();
                out.push(Reference {
                    name: text.to_string(),
                    file: PathBuf::new(),
                    start_line: pos.row as u32 + 1,
                    start_col: pos.column as u32 + 1,
                });
            }
        }
        Ok(out)
    }
}

fn capture_kind(capture_name: &str) -> SymbolKind {
    match capture_name {
        "function" => SymbolKind::Function,
        "struct" => SymbolKind::Struct,
        "trait" => SymbolKind::Trait,
        "enum" => SymbolKind::Enum,
        "module" => SymbolKind::Module,
        "const" => SymbolKind::Const,
        "impl" => SymbolKind::Impl,
        other => SymbolKind::Other(other.to_string()),
    }
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

    #[test]
    fn extracts_struct_trait_and_functions() {
        let plugin = RustPlugin;
        let syms = plugin.extract_symbols(SOURCE).unwrap();
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
        let refs = plugin.extract_references(SOURCE).unwrap();
        let names: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"create_order"));
        assert!(names.contains(&"println"));

        let call = refs.iter().find(|r| r.name == "create_order").unwrap();
        assert_eq!(call.start_line, 5);
    }
}
