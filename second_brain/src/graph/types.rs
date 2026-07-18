//! Core domain types shared across the engine.

use std::path::PathBuf;

/// The kind of a source symbol. Extensible via `Other` for future languages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Struct,
    Trait,
    Enum,
    Impl,
    Module,
    Const,
    Other(String),
}

impl SymbolKind {
    /// Serialize to the string stored in the `symbols.kind` column.
    pub fn as_db(&self) -> String {
        match self {
            SymbolKind::Function => "function".to_string(),
            SymbolKind::Struct => "struct".to_string(),
            SymbolKind::Trait => "trait".to_string(),
            SymbolKind::Enum => "enum".to_string(),
            SymbolKind::Impl => "impl".to_string(),
            SymbolKind::Module => "module".to_string(),
            SymbolKind::Const => "const".to_string(),
            SymbolKind::Other(s) => s.clone(),
        }
    }

    /// Parse from the string stored in the `symbols.kind` column.
    pub fn from_db(s: &str) -> SymbolKind {
        match s {
            "function" => SymbolKind::Function,
            "struct" => SymbolKind::Struct,
            "trait" => SymbolKind::Trait,
            "enum" => SymbolKind::Enum,
            "impl" => SymbolKind::Impl,
            "module" => SymbolKind::Module,
            "const" => SymbolKind::Const,
            other => SymbolKind::Other(other.to_string()),
        }
    }
}

/// A defined symbol. `file` is empty during extraction and populated on read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub file: PathBuf,
    pub start_line: u32,
    pub start_col: u32,
}

/// A reference (call or macro invocation) to a name. `file` is empty during
/// extraction and populated on read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub name: String,
    pub file: PathBuf,
    pub start_line: u32,
    pub start_col: u32,
}

/// An indexed source file and its content hash (hash consumed in v0.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileNode {
    pub path: PathBuf,
    pub content_hash: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_kind_round_trips_through_db_string() {
        let kinds = [
            SymbolKind::Function,
            SymbolKind::Struct,
            SymbolKind::Trait,
            SymbolKind::Enum,
            SymbolKind::Impl,
            SymbolKind::Module,
            SymbolKind::Const,
        ];
        for k in kinds {
            assert_eq!(SymbolKind::from_db(&k.as_db()), k);
        }
        assert_eq!(SymbolKind::from_db("weird"), SymbolKind::Other("weird".to_string()));
    }
}
