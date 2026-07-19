//! Core domain types shared across the engine.

use std::path::PathBuf;

/// The kind of a source symbol. Extensible via `Other` for future languages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolKind {
    /// A function or method definition.
    Function,
    /// A `struct` definition.
    Struct,
    /// A `trait` definition.
    Trait,
    /// An `enum` definition.
    Enum,
    /// An `impl` block.
    Impl,
    /// A module (`mod`) definition.
    Module,
    /// A `const` (or `static`) definition.
    Const,
    /// Any other kind, identified by its raw string label.
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

/// The kind of a reference site. Mirrors [`SymbolKind`]'s DB round-tripping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReferenceKind {
    /// A function/free-call reference (the default).
    Call,
    /// A macro invocation.
    Macro,
    /// A method call on a receiver.
    Method,
    /// A type usage (e.g. in a signature or annotation).
    Type,
    /// A path segment reference (e.g. `a::b::c`).
    Path,
}

impl ReferenceKind {
    /// Serialize to the string stored in the `references.kind` column.
    pub fn as_db(&self) -> String {
        match self {
            ReferenceKind::Call => "call".to_string(),
            ReferenceKind::Macro => "macro".to_string(),
            ReferenceKind::Method => "method".to_string(),
            ReferenceKind::Type => "type".to_string(),
            ReferenceKind::Path => "path".to_string(),
        }
    }

    /// Parse from the string stored in the `references.kind` column.
    ///
    /// Unknown values fall back to [`ReferenceKind::Call`].
    pub fn from_db(s: &str) -> ReferenceKind {
        match s {
            "macro" => ReferenceKind::Macro,
            "method" => ReferenceKind::Method,
            "type" => ReferenceKind::Type,
            "path" => ReferenceKind::Path,
            _ => ReferenceKind::Call,
        }
    }
}

/// A defined symbol. `file` is empty during extraction and populated on read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    /// The symbol's identifier name.
    pub name: String,
    /// The kind of symbol (function, struct, etc.).
    pub kind: SymbolKind,
    /// Path to the file that defines the symbol.
    pub file: PathBuf,
    /// 1-based line of the definition.
    pub start_line: u32,
    /// 1-based column of the definition.
    pub start_col: u32,
    /// Fully-qualified module path of the symbol (empty when unresolved).
    pub module_path: String,
}

/// A reference (call or macro invocation) to a name. `file` is empty during
/// extraction and populated on read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// The referenced identifier name.
    pub name: String,
    /// Path to the file containing the reference.
    pub file: PathBuf,
    /// 1-based line of the reference.
    pub start_line: u32,
    /// 1-based column of the reference.
    pub start_col: u32,
    /// The kind of reference site (call, macro, method, ...).
    pub kind: ReferenceKind,
    /// Enclosing symbol name for the reference (empty when unknown).
    pub container: String,
}

/// An `impl` block record, linking a type (and optional trait) to its location.
///
/// Populated by later extraction tasks; defined now so the storage layer can
/// persist it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImplRecord {
    /// The name of the type the `impl` block is for.
    pub type_name: String,
    /// The trait being implemented, if this is a trait impl.
    pub trait_name: Option<String>,
    /// Path to the file containing the `impl` block.
    pub file: PathBuf,
    /// 1-based line of the `impl` block.
    pub start_line: u32,
    /// 1-based column of the `impl` block.
    pub start_col: u32,
}

/// An `use`/import record within a file.
///
/// Populated by later extraction tasks; defined now so the storage layer can
/// persist it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    /// The imported module path.
    pub module_path: String,
    /// The local alias, if the import was renamed (`as`).
    pub alias: Option<String>,
    /// Path to the file containing the import.
    pub file: PathBuf,
}

/// An indexed source file and its content hash (hash consumed in v0.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileNode {
    /// Path to the indexed source file.
    pub path: PathBuf,
    /// SHA-256 hash of the file contents (consumed for incremental indexing in v0.2).
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

    #[test]
    fn reference_kind_round_trips_through_db_string() {
        let kinds = [
            ReferenceKind::Call,
            ReferenceKind::Macro,
            ReferenceKind::Method,
            ReferenceKind::Type,
            ReferenceKind::Path,
        ];
        for k in kinds {
            assert_eq!(ReferenceKind::from_db(&k.as_db()), k);
        }
        assert_eq!(ReferenceKind::from_db("weird"), ReferenceKind::Call);
    }
}
