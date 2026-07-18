# SecondBrain v0.1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the v0.1 foundation of SecondBrain — a local-first Rust engine that indexes a Rust repository into SQLite and answers `definition`/`references`/`callers` queries deterministically by name.

**Architecture:** A language-agnostic core (SQLite storage, parallel indexer, CLI) with Rust-specific logic isolated behind a `LanguagePlugin` trait. Tree-sitter extracts symbols and call/macro references; results are stored in SQLite and looked up by name. No LLMs, embeddings, or semantic search anywhere.

**Tech Stack:** Rust (edition 2021), `tree-sitter` + `tree-sitter-rust`, `rusqlite` (bundled SQLite), `rayon`, `ignore`/`walkdir`, `clap` (derive), `thiserror` + `anyhow`, `sha2` + `hex`, `streaming-iterator`.

## Global Constraints

- Rust edition 2021. Crate name `second_brain`; binary name `sb`.
- No `.unwrap()` / `.expect()` outside `#[cfg(test)]` code. Propagate with `?`.
- No `unsafe` blocks anywhere.
- Strong types for domain concepts: file paths are `std::path::PathBuf`, never `String`.
- `thiserror` for all library error types; `anyhow` only at the binary (`main.rs`) boundary.
- Rustdoc (`///`) on every public module, trait, and function.
- `db/` and `index/` must never hardcode language specifics; all language logic lives behind the `LanguagePlugin` trait.
- Determinism: identical repo state must produce identical query output. Query results are ordered by `(path, start_line, start_col)`.
- Position convention: `start_line` and `start_col` are **1-based** (tree-sitter row/column + 1).
- The SQL word `references` is reserved; it MUST always be quoted as `"references"` in SQL.
- The on-disk index lives at `.secondbrain/index.db` relative to the current working directory.

---

## File Structure

```text
second_brain/
├── Cargo.toml              # Task 1
├── README.md               # Task 6
├── .gitignore              # already present at repo root
├── src/
│   ├── lib.rs              # Task 1 (grown each task)
│   ├── main.rs             # Task 5
│   ├── error.rs            # Task 1
│   ├── graph/
│   │   ├── mod.rs          # Task 1
│   │   └── types.rs        # Task 1
│   ├── db/
│   │   ├── mod.rs          # Task 2
│   │   ├── schema.rs       # Task 2
│   │   └── queries.rs      # Task 2
│   ├── languages/
│   │   ├── mod.rs          # Task 3
│   │   └── rust.rs         # Task 3
│   ├── index/
│   │   ├── mod.rs          # Task 4
│   │   └── worker.rs       # Task 4
│   └── cli/
│       ├── mod.rs          # Task 5
│       └── commands.rs     # Task 5
└── tests/
    └── integration.rs      # Task 4 / Task 5
```

> **Working directory note:** All `cargo` commands below run from the `second_brain/` package directory created in Task 1. The design docs live one level up in `docs/`, so from the repo root you will `cd second_brain` after Task 1, Step 1.

---

## Task 1: Scaffolding, Error Types, Domain Types

**Files:**
- Create: `second_brain/Cargo.toml`
- Create: `second_brain/src/lib.rs`
- Create: `second_brain/src/error.rs`
- Create: `second_brain/src/graph/mod.rs`
- Create: `second_brain/src/graph/types.rs`

**Interfaces:**
- Produces:
  - `second_brain::error::SecondBrainError` (enum) and `second_brain::error::Result<T> = std::result::Result<T, SecondBrainError>`.
  - `second_brain::graph::types::{Symbol, Reference, FileNode, SymbolKind}`.
  - `Symbol { name: String, kind: SymbolKind, file: PathBuf, start_line: u32, start_col: u32 }`.
  - `Reference { name: String, file: PathBuf, start_line: u32, start_col: u32 }`.
  - `FileNode { path: PathBuf, content_hash: String }`.
  - `SymbolKind` with `fn as_db(&self) -> String` and `fn from_db(s: &str) -> SymbolKind`.

- [ ] **Step 1: Create the Cargo package and add dependencies**

Run from the repo root (`/Users/ashokdudhade/os/second-brain`):

```bash
cargo new second_brain --lib
cd second_brain
cargo add rusqlite --features bundled
cargo add tree-sitter@0.25
cargo add tree-sitter-rust@0.24
cargo add streaming-iterator@0.1
cargo add rayon ignore walkdir sha2 hex anyhow
cargo add clap --features derive
cargo add thiserror
cargo add --dev tempfile
```

Then set the binary name and edition. Edit `second_brain/Cargo.toml` so the `[package]` uses `edition = "2021"` and append:

```toml
[[bin]]
name = "sb"
path = "src/main.rs"
```

- [ ] **Step 2: Write the failing test for `SymbolKind` DB round-trip**

Create `second_brain/src/graph/types.rs` with only this test module for now:

```rust
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
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test --lib symbol_kind_round_trips`
Expected: FAIL to compile — `SymbolKind` not found.

- [ ] **Step 4: Implement domain types and error module**

Prepend to `second_brain/src/graph/types.rs` (above the test module):

```rust
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
```

Create `second_brain/src/graph/mod.rs`:

```rust
//! Domain graph types and (later) query surface.

pub mod types;
```

Create `second_brain/src/error.rs`:

```rust
//! Core error type for the SecondBrain library.

use std::path::PathBuf;
use thiserror::Error;

/// All errors produced by the SecondBrain library.
#[derive(Debug, Error)]
pub enum SecondBrainError {
    #[error("I/O error for {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("database error")]
    Database(#[from] rusqlite::Error),

    #[error("failed to parse source code")]
    Parse,

    #[error("tree-sitter error: {0}")]
    TreeSitter(String),

    #[error("no language plugin registered for extension {0:?}")]
    UnsupportedExtension(String),
}

/// Convenience `Result` alias used throughout the library.
pub type Result<T> = std::result::Result<T, SecondBrainError>;
```

Replace `second_brain/src/lib.rs` with:

```rust
//! SecondBrain: deterministic, local-first code intelligence engine.

pub mod error;
pub mod graph;

pub use error::{Result, SecondBrainError};
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test --lib symbol_kind_round_trips`
Expected: PASS (1 passed).

- [ ] **Step 6: Verify lint cleanliness**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: no warnings/errors.

- [ ] **Step 7: Commit**

```bash
git add second_brain
git commit -m "feat: scaffold second_brain crate with error and domain types"
```

---

## Task 2: SQLite Storage Layer

**Files:**
- Create: `second_brain/src/db/mod.rs`
- Create: `second_brain/src/db/schema.rs`
- Create: `second_brain/src/db/queries.rs`
- Modify: `second_brain/src/lib.rs` (add `pub mod db;`)

**Interfaces:**
- Consumes: `Symbol`, `Reference`, `FileNode`, `SymbolKind` from `graph::types`; `Result` from `error`.
- Produces:
  - `db::schema::initialize(conn: &rusqlite::Connection) -> Result<()>`
  - `db::queries::insert_file(conn: &rusqlite::Connection, node: &FileNode) -> Result<i64>`
  - `db::queries::insert_symbols(conn: &rusqlite::Connection, file_id: i64, symbols: &[Symbol]) -> Result<()>`
  - `db::queries::insert_references(conn: &rusqlite::Connection, file_id: i64, references: &[Reference]) -> Result<()>`
  - `db::queries::find_definition(conn: &rusqlite::Connection, name: &str) -> Result<Vec<Symbol>>`
  - `db::queries::find_references(conn: &rusqlite::Connection, name: &str) -> Result<Vec<Reference>>`

- [ ] **Step 1: Write the failing tests (in-memory SQLite)**

Create `second_brain/src/db/queries.rs` with only this test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;
    use crate::graph::types::{FileNode, Reference, Symbol, SymbolKind};
    use rusqlite::Connection;
    use std::path::PathBuf;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        schema::initialize(&conn).expect("init schema");
        conn
    }

    #[test]
    fn insert_and_find_definition_by_name() {
        let conn = setup();
        let file_id = insert_file(
            &conn,
            &FileNode { path: PathBuf::from("src/a.rs"), content_hash: "h".to_string() },
        )
        .unwrap();
        insert_symbols(
            &conn,
            file_id,
            &[Symbol {
                name: "AuthService".to_string(),
                kind: SymbolKind::Struct,
                file: PathBuf::new(),
                start_line: 10,
                start_col: 1,
            }],
        )
        .unwrap();

        let defs = find_definition(&conn, "AuthService").unwrap();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "AuthService");
        assert_eq!(defs[0].kind, SymbolKind::Struct);
        assert_eq!(defs[0].file, PathBuf::from("src/a.rs"));
        assert_eq!(defs[0].start_line, 10);
    }

    #[test]
    fn insert_and_find_references_by_name() {
        let conn = setup();
        let file_id = insert_file(
            &conn,
            &FileNode { path: PathBuf::from("src/b.rs"), content_hash: "h".to_string() },
        )
        .unwrap();
        insert_references(
            &conn,
            file_id,
            &[Reference { name: "create_order".to_string(), file: PathBuf::new(), start_line: 5, start_col: 9 }],
        )
        .unwrap();

        let refs = find_references(&conn, "create_order").unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].start_line, 5);
        assert_eq!(refs[0].file, PathBuf::from("src/b.rs"));
    }

    #[test]
    fn reindexing_same_path_updates_hash_not_duplicates() {
        let conn = setup();
        let a = insert_file(&conn, &FileNode { path: PathBuf::from("src/a.rs"), content_hash: "h1".into() }).unwrap();
        let b = insert_file(&conn, &FileNode { path: PathBuf::from("src/a.rs"), content_hash: "h2".into() }).unwrap();
        assert_eq!(a, b);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib db::queries`
Expected: FAIL to compile — `insert_file`, `schema`, etc. not found.

- [ ] **Step 3: Implement the schema**

Create `second_brain/src/db/schema.rs`:

```rust
//! SQLite schema definition and initialization.

use crate::error::Result;
use rusqlite::Connection;

/// Create all tables and indexes if they do not already exist. Idempotent.
///
/// `references` is a reserved SQL keyword, so it is always quoted.
pub fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS files (
            id           INTEGER PRIMARY KEY,
            path         TEXT UNIQUE NOT NULL,
            content_hash TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS symbols (
            id         INTEGER PRIMARY KEY,
            file_id    INTEGER NOT NULL REFERENCES files(id),
            name       TEXT NOT NULL,
            kind       TEXT NOT NULL,
            start_line INTEGER NOT NULL,
            start_col  INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS "references" (
            id         INTEGER PRIMARY KEY,
            file_id    INTEGER NOT NULL REFERENCES files(id),
            name       TEXT NOT NULL,
            start_line INTEGER NOT NULL,
            start_col  INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
        CREATE INDEX IF NOT EXISTS idx_references_name ON "references"(name);
        "#,
    )?;
    Ok(())
}
```

- [ ] **Step 4: Implement the queries**

Prepend to `second_brain/src/db/queries.rs` (above the test module):

```rust
//! Type-safe insert/select wrappers over the SQLite tables.

use crate::error::Result;
use crate::graph::types::{FileNode, Reference, Symbol, SymbolKind};
use rusqlite::{params, Connection};
use std::path::PathBuf;

/// Insert a file (or update its hash on conflict) and return its row id.
pub fn insert_file(conn: &Connection, node: &FileNode) -> Result<i64> {
    let path = node.path.to_string_lossy();
    let id: i64 = conn.query_row(
        r#"INSERT INTO files (path, content_hash) VALUES (?1, ?2)
           ON CONFLICT(path) DO UPDATE SET content_hash = excluded.content_hash
           RETURNING id"#,
        params![path, node.content_hash],
        |row| row.get(0),
    )?;
    Ok(id)
}

/// Insert all symbols for a file.
pub fn insert_symbols(conn: &Connection, file_id: i64, symbols: &[Symbol]) -> Result<()> {
    let mut stmt = conn.prepare(
        "INSERT INTO symbols (file_id, name, kind, start_line, start_col) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for s in symbols {
        stmt.execute(params![file_id, s.name, s.kind.as_db(), s.start_line as i64, s.start_col as i64])?;
    }
    Ok(())
}

/// Insert all references for a file.
pub fn insert_references(conn: &Connection, file_id: i64, references: &[Reference]) -> Result<()> {
    let mut stmt = conn.prepare(
        r#"INSERT INTO "references" (file_id, name, start_line, start_col) VALUES (?1, ?2, ?3, ?4)"#,
    )?;
    for r in references {
        stmt.execute(params![file_id, r.name, r.start_line as i64, r.start_col as i64])?;
    }
    Ok(())
}

/// Find all symbol definitions matching `name`, ordered deterministically.
pub fn find_definition(conn: &Connection, name: &str) -> Result<Vec<Symbol>> {
    let mut stmt = conn.prepare(
        "SELECT s.name, s.kind, f.path, s.start_line, s.start_col
         FROM symbols s JOIN files f ON s.file_id = f.id
         WHERE s.name = ?1
         ORDER BY f.path, s.start_line, s.start_col",
    )?;
    let rows = stmt.query_map(params![name], |row| {
        Ok(Symbol {
            name: row.get::<_, String>(0)?,
            kind: SymbolKind::from_db(&row.get::<_, String>(1)?),
            file: PathBuf::from(row.get::<_, String>(2)?),
            start_line: row.get::<_, i64>(3)? as u32,
            start_col: row.get::<_, i64>(4)? as u32,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Find all references matching `name`, ordered deterministically.
pub fn find_references(conn: &Connection, name: &str) -> Result<Vec<Reference>> {
    let mut stmt = conn.prepare(
        r#"SELECT r.name, f.path, r.start_line, r.start_col
           FROM "references" r JOIN files f ON r.file_id = f.id
           WHERE r.name = ?1
           ORDER BY f.path, r.start_line, r.start_col"#,
    )?;
    let rows = stmt.query_map(params![name], |row| {
        Ok(Reference {
            name: row.get::<_, String>(0)?,
            file: PathBuf::from(row.get::<_, String>(1)?),
            start_line: row.get::<_, i64>(2)? as u32,
            start_col: row.get::<_, i64>(3)? as u32,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}
```

Create `second_brain/src/db/mod.rs`:

```rust
//! SQLite storage layer: schema and type-safe queries.

pub mod queries;
pub mod schema;
```

Add `pub mod db;` to `second_brain/src/lib.rs` (below `pub mod error;`):

```rust
pub mod db;
pub mod error;
pub mod graph;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib db::`
Expected: PASS (3 passed).

- [ ] **Step 6: Lint**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add second_brain
git commit -m "feat: add SQLite schema and type-safe query layer"
```

---

## Task 3: Language Plugin Trait + Rust Extraction

**Files:**
- Create: `second_brain/src/languages/mod.rs`
- Create: `second_brain/src/languages/rust.rs`
- Modify: `second_brain/src/lib.rs` (add `pub mod languages;`)

**Interfaces:**
- Consumes: `Symbol`, `Reference`, `SymbolKind` from `graph::types`; `Result`, `SecondBrainError` from `error`.
- Produces:
  - `trait languages::LanguagePlugin: Sync` with `fn extensions(&self) -> &[&str]`, `fn extract_symbols(&self, source_code: &str) -> Result<Vec<Symbol>>`, `fn extract_references(&self, source_code: &str) -> Result<Vec<Reference>>`.
  - `languages::Registry` with `fn with_defaults() -> Self` and `fn for_extension(&self, ext: &str) -> Option<&dyn LanguagePlugin>`.
  - `languages::rust::RustPlugin` (unit struct implementing `LanguagePlugin`).
- Note: extracted `Symbol`/`Reference` have `file == PathBuf::new()`; the indexer (Task 4) associates them with a file id.

- [ ] **Step 1: Write the failing extraction tests**

Create `second_brain/src/languages/rust.rs` with only this test module for now:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib languages::rust`
Expected: FAIL to compile — `RustPlugin` not found.

- [ ] **Step 3: Define the trait and registry**

Create `second_brain/src/languages/mod.rs`:

```rust
//! Language plugin trait and registry. The core dispatches to plugins by file
//! extension without knowing any language specifics.

pub mod rust;

use crate::error::Result;
use crate::graph::types::{Reference, Symbol};

/// A language-specific extractor. Must be `Sync` so plugins can be shared across
/// Rayon worker threads during parallel indexing.
pub trait LanguagePlugin: Sync {
    /// File extensions (without dot) this plugin handles, e.g. `["rs"]`.
    fn extensions(&self) -> &[&str];

    /// Extract defined symbols from source. Returned symbols have an empty `file`.
    fn extract_symbols(&self, source_code: &str) -> Result<Vec<Symbol>>;

    /// Extract references (call/macro sites) from source. Returned references have
    /// an empty `file`.
    fn extract_references(&self, source_code: &str) -> Result<Vec<Reference>>;
}

/// Holds the set of available language plugins.
pub struct Registry {
    plugins: Vec<Box<dyn LanguagePlugin>>,
}

impl Registry {
    /// A registry with all built-in plugins (Rust in v0.1).
    pub fn with_defaults() -> Self {
        Registry { plugins: vec![Box::new(rust::RustPlugin)] }
    }

    /// The first plugin registered for `ext`, if any.
    pub fn for_extension(&self, ext: &str) -> Option<&dyn LanguagePlugin> {
        self.plugins
            .iter()
            .map(|b| b.as_ref())
            .find(|p| p.extensions().contains(&ext))
    }
}
```

- [ ] **Step 4: Implement the Rust plugin**

Prepend to `second_brain/src/languages/rust.rs` (above the test module):

```rust
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
```

Add `pub mod languages;` to `second_brain/src/lib.rs`:

```rust
pub mod db;
pub mod error;
pub mod graph;
pub mod languages;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib languages::rust`
Expected: PASS (2 passed).

> If `matches.next()` does not resolve, ensure `use streaming_iterator::StreamingIterator;` is present and `streaming-iterator` is a dependency (added in Task 1).

- [ ] **Step 6: Lint**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add second_brain
git commit -m "feat: add LanguagePlugin trait and Rust tree-sitter extraction"
```

---

## Task 4: Indexing Engine

**Files:**
- Create: `second_brain/src/index/mod.rs`
- Create: `second_brain/src/index/worker.rs`
- Create: `second_brain/tests/integration.rs`
- Modify: `second_brain/src/lib.rs` (add `pub mod index;`)

**Interfaces:**
- Consumes: `Registry` from `languages`; `insert_file`/`insert_symbols`/`insert_references` from `db::queries`; `schema::initialize`; `FileNode`/`Symbol`/`Reference` from `graph::types`.
- Produces:
  - `index::worker::collect_rust_files(root: &Path) -> Vec<PathBuf>`
  - `index::worker::parse_file(path: &Path, registry: &Registry) -> Result<ParsedFile>`
  - `index::worker::parse_all(files: &[PathBuf], registry: &Registry) -> Result<Vec<ParsedFile>>`
  - `struct index::worker::ParsedFile { node: FileNode, symbols: Vec<Symbol>, references: Vec<Reference> }`
  - `index::index_repository(root: &Path, conn: &mut rusqlite::Connection) -> Result<usize>` (returns number of files indexed).

- [ ] **Step 1: Write the failing integration test**

Create `second_brain/tests/integration.rs`:

```rust
use rusqlite::Connection;
use second_brain::db::queries;
use second_brain::graph::types::SymbolKind;
use second_brain::index;
use std::fs;

#[test]
fn indexes_and_queries_a_fixture_repo() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub struct AuthService;\nfn create_order() {}\nfn caller() { create_order(); }\n",
    )
    .unwrap();
    // A file that should be ignored via .gitignore semantics.
    fs::write(root.join(".gitignore"), "ignored.rs\n").unwrap();
    fs::write(root.join("ignored.rs"), "fn should_not_index() {}\n").unwrap();

    let mut conn = Connection::open_in_memory().unwrap();
    let count = index::index_repository(root, &mut conn).unwrap();
    assert_eq!(count, 1, "only src/lib.rs should be indexed");

    let defs = queries::find_definition(&conn, "AuthService").unwrap();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].kind, SymbolKind::Struct);

    let refs = queries::find_references(&conn, "create_order").unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].start_line, 3);

    let ignored = queries::find_definition(&conn, "should_not_index").unwrap();
    assert!(ignored.is_empty(), "ignored.rs must be skipped");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test integration`
Expected: FAIL to compile — `second_brain::index` not found.

- [ ] **Step 3: Implement the worker**

Create `second_brain/src/index/worker.rs`:

```rust
//! Parallel file parsing pipeline. CPU-bound parse/extract runs in parallel;
//! database writes are serialized by the orchestrator in `index::index_repository`.

use crate::error::{Result, SecondBrainError};
use crate::graph::types::{FileNode, Reference, Symbol};
use crate::languages::Registry;
use ignore::WalkBuilder;
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

/// A parsed file with its extracted symbols and references.
pub struct ParsedFile {
    pub node: FileNode,
    pub symbols: Vec<Symbol>,
    pub references: Vec<Reference>,
}

/// Collect all `.rs` files under `root`, honoring `.gitignore` (via `ignore`).
pub fn collect_rust_files(root: &Path) -> Vec<PathBuf> {
    WalkBuilder::new(root)
        .standard_filters(true)
        .build()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|entry| entry.into_path())
        .filter(|path| path.extension().and_then(|s| s.to_str()) == Some("rs"))
        .collect()
}

/// Read, parse, and extract a single file.
pub fn parse_file(path: &Path, registry: &Registry) -> Result<ParsedFile> {
    let source = fs::read_to_string(path)
        .map_err(|source| SecondBrainError::Io { path: path.to_path_buf(), source })?;
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    let plugin = registry
        .for_extension(ext)
        .ok_or_else(|| SecondBrainError::UnsupportedExtension(ext.to_string()))?;

    let symbols = plugin.extract_symbols(&source)?;
    let references = plugin.extract_references(&source)?;
    let content_hash = hex::encode(Sha256::digest(source.as_bytes()));

    Ok(ParsedFile {
        node: FileNode { path: path.to_path_buf(), content_hash },
        symbols,
        references,
    })
}

/// Parse every file in parallel. Fails fast on the first error.
pub fn parse_all(files: &[PathBuf], registry: &Registry) -> Result<Vec<ParsedFile>> {
    files.par_iter().map(|path| parse_file(path, registry)).collect()
}
```

- [ ] **Step 4: Implement the orchestrator**

Create `second_brain/src/index/mod.rs`:

```rust
//! Indexing orchestration: crawl, parse in parallel, then persist in one
//! transaction.

pub mod worker;

use crate::db::{queries, schema};
use crate::error::Result;
use crate::languages::Registry;
use rusqlite::Connection;
use std::path::Path;

/// Index every `.rs` file under `root` into `conn`. Returns the number of files
/// indexed.
pub fn index_repository(root: &Path, conn: &mut Connection) -> Result<usize> {
    schema::initialize(conn)?;
    let registry = Registry::with_defaults();
    let files = worker::collect_rust_files(root);
    let parsed = worker::parse_all(&files, &registry)?;

    let tx = conn.transaction()?;
    let mut count = 0usize;
    for pf in &parsed {
        let file_id = queries::insert_file(&tx, &pf.node)?;
        queries::insert_symbols(&tx, file_id, &pf.symbols)?;
        queries::insert_references(&tx, file_id, &pf.references)?;
        count += 1;
    }
    tx.commit()?;
    Ok(count)
}
```

Add `pub mod index;` to `second_brain/src/lib.rs`:

```rust
pub mod db;
pub mod error;
pub mod graph;
pub mod index;
pub mod languages;
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test --test integration`
Expected: PASS (1 passed).

- [ ] **Step 6: Run the full suite and lint**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: all tests pass, no warnings.

- [ ] **Step 7: Commit**

```bash
git add second_brain
git commit -m "feat: add parallel indexing engine with .gitignore-aware crawl"
```

---

## Task 5: CLI

**Files:**
- Create: `second_brain/src/cli/mod.rs`
- Create: `second_brain/src/cli/commands.rs`
- Create: `second_brain/src/main.rs`
- Modify: `second_brain/src/lib.rs` (add `pub mod cli;`)
- Modify: `second_brain/tests/integration.rs` (add a CLI end-to-end test)

**Interfaces:**
- Consumes: `index::index_repository`; `db::queries::{find_definition, find_references}`; `db::schema::initialize`; `Symbol`/`Reference` from `graph::types`.
- Produces:
  - `cli::Cli` (clap `Parser`) and `cli::Commands` (subcommands `Index { path }`, `Definition { name }`, `References { name }`, `Callers { name }`).
  - `cli::commands::run_index(path: &Path) -> Result<usize>`
  - `cli::commands::run_definition(name: &str) -> Result<Vec<Symbol>>`
  - `cli::commands::run_references(name: &str) -> Result<Vec<Reference>>`
  - The `sb` binary (`main.rs`) that parses args and prints results.

- [ ] **Step 1: Write the failing CLI end-to-end test**

Append to `second_brain/tests/integration.rs`:

```rust
#[test]
fn cli_binary_indexes_and_queries() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("main.rs"), "fn create_order() {}\nfn run() { create_order(); }\n").unwrap();

    let sb = env!("CARGO_BIN_EXE_sb");

    let index_out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["index", "."])
        .output()
        .unwrap();
    assert!(index_out.status.success(), "index failed: {:?}", index_out);

    let def_out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["definition", "create_order"])
        .output()
        .unwrap();
    assert!(def_out.status.success());
    let stdout = String::from_utf8(def_out.stdout).unwrap();
    assert!(stdout.contains("create_order"), "got: {stdout}");
    assert!(stdout.contains(":1:"), "expected line 1 in: {stdout}");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test integration cli_binary_indexes_and_queries`
Expected: FAIL — the `sb` binary does not compile yet (no `main.rs`/`cli`).

- [ ] **Step 3: Define the CLI structs**

Create `second_brain/src/cli/mod.rs`:

```rust
//! Command-line interface definitions.

pub mod commands;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Top-level CLI parser for the `sb` binary.
#[derive(Parser)]
#[command(name = "sb", about = "SecondBrain: deterministic code intelligence")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

/// Available subcommands.
#[derive(Subcommand)]
pub enum Commands {
    /// Index a repository at PATH into `.secondbrain/index.db`.
    Index { path: PathBuf },
    /// Print definition location(s) for a symbol name.
    Definition { name: String },
    /// Print reference location(s) for a name.
    References { name: String },
    /// Print call/use sites of a function name (name-based in v0.1).
    Callers { name: String },
}
```

- [ ] **Step 4: Implement the command layer**

Create `second_brain/src/cli/commands.rs`:

```rust
//! Execution logic behind each CLI subcommand.

use crate::db::{queries, schema};
use crate::error::{Result, SecondBrainError};
use crate::graph::types::{Reference, Symbol};
use crate::index;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

const DB_DIR: &str = ".secondbrain";
const DB_FILE: &str = "index.db";

fn db_path() -> PathBuf {
    Path::new(DB_DIR).join(DB_FILE)
}

/// Open (creating the directory if needed) the on-disk index database.
fn open_db() -> Result<Connection> {
    std::fs::create_dir_all(DB_DIR)
        .map_err(|source| SecondBrainError::Io { path: PathBuf::from(DB_DIR), source })?;
    let conn = Connection::open(db_path())?;
    Ok(conn)
}

/// Index the repository at `path`. Returns the number of files indexed.
pub fn run_index(path: &Path) -> Result<usize> {
    let mut conn = open_db()?;
    index::index_repository(path, &mut conn)
}

/// Look up definitions by name.
pub fn run_definition(name: &str) -> Result<Vec<Symbol>> {
    let conn = open_db()?;
    schema::initialize(&conn)?;
    queries::find_definition(&conn, name)
}

/// Look up references by name (also used for `callers` in v0.1).
pub fn run_references(name: &str) -> Result<Vec<Reference>> {
    let conn = open_db()?;
    schema::initialize(&conn)?;
    queries::find_references(&conn, name)
}
```

Create `second_brain/src/main.rs`:

```rust
//! `sb` binary entry point. Uses `anyhow` for context-rich top-level errors.

use anyhow::{Context, Result};
use clap::Parser;
use second_brain::cli::{commands, Cli, Commands};

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Index { path } => {
            let n = commands::run_index(&path)
                .with_context(|| format!("indexing {}", path.display()))?;
            println!("Indexed {n} file(s).");
        }
        Commands::Definition { name } => {
            let defs = commands::run_definition(&name).context("querying definition")?;
            if defs.is_empty() {
                println!("No definition found for {name}");
            }
            for s in defs {
                println!(
                    "{}:{}:{}\t{}\t{}",
                    s.file.display(),
                    s.start_line,
                    s.start_col,
                    s.kind.as_db(),
                    s.name
                );
            }
        }
        Commands::References { name } => {
            let refs = commands::run_references(&name).context("querying references")?;
            if refs.is_empty() {
                println!("No references found for {name}");
            }
            for r in refs {
                println!("{}:{}:{}\t{}", r.file.display(), r.start_line, r.start_col, r.name);
            }
        }
        Commands::Callers { name } => {
            let refs = commands::run_references(&name).context("querying callers")?;
            if refs.is_empty() {
                println!("No callers found for {name}");
            }
            for r in refs {
                println!("{}:{}:{}\t{}", r.file.display(), r.start_line, r.start_col, r.name);
            }
        }
    }
    Ok(())
}
```

Add `pub mod cli;` to `second_brain/src/lib.rs`:

```rust
pub mod cli;
pub mod db;
pub mod error;
pub mod graph;
pub mod index;
pub mod languages;

pub use error::{Result, SecondBrainError};
```

- [ ] **Step 5: Run the CLI test to verify it passes**

Run: `cargo test --test integration cli_binary_indexes_and_queries`
Expected: PASS.

- [ ] **Step 6: Manual smoke test**

```bash
cargo run --quiet -- index .
cargo run --quiet -- definition RustPlugin
cargo run --quiet -- callers find_definition
```

Expected: `index` prints an indexed-file count; `definition RustPlugin` prints a `src/languages/rust.rs:LINE:COL	struct	RustPlugin` line; `callers` prints call sites. Then clean up: `rm -rf .secondbrain`.

- [ ] **Step 7: Full suite + lint**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: all pass, no warnings.

- [ ] **Step 8: Commit**

```bash
git add second_brain
git commit -m "feat: add sb CLI with index/definition/references/callers"
```

---

## Task 6: Documentation & Release Polish

**Files:**
- Create: `second_brain/README.md`
- Modify: any source file missing Rustdoc on a public item.

**Interfaces:** none (docs only).

- [ ] **Step 1: Verify docs build and public API is documented**

Run: `cargo doc --no-deps` and `cargo clippy --all-targets -- -D warnings -W missing_docs`
Expected: docs build; address any `missing_docs` warnings by adding `///` to the flagged public items.

- [ ] **Step 2: Write the README**

Create `second_brain/README.md`:

```markdown
# SecondBrain

Deterministic, local-first code intelligence for AI coding agents. SecondBrain
indexes a Rust repository with Tree-sitter and answers structural queries from a
local SQLite database — no LLMs, embeddings, or semantic search.

## Install / Build

```bash
cargo build --release
```

The binary is `sb` (target/release/sb).

## Usage

```bash
sb index <path>            # index a repository into ./.secondbrain/index.db
sb definition <name>       # where a symbol is defined
sb references <name>       # where a name is referenced (call/macro sites in v0.1)
sb callers <name>          # call/use sites of a function (name-based in v0.1)
```

Output is `path:line:col` (1-based), tab-separated, stable and script-friendly.

## Scope (v0.1)

- Rust only; name-based resolution (same-named matches are all reported).
- Precise, scope-aware resolution via Stack Graphs is planned for v0.2.

## License

Open source (see repository).
```

- [ ] **Step 3: Final full verification**

Run: `cargo test && cargo clippy --all-targets -- -D warnings && cargo build --release`
Expected: all tests pass, no warnings, release binary builds.

- [ ] **Step 4: Commit**

```bash
git add second_brain
git commit -m "docs: add README and complete rustdoc coverage"
```

---

## Self-Review

**Spec coverage:**
- Deterministic, no-LLM engine → Global Constraints + Task 3 (Tree-sitter only). ✓
- Modular language-agnostic core / `LanguagePlugin` → Task 3. ✓
- SQLite schema (`files`, `symbols`, `references` + name indexes) → Task 2. ✓
- Rust plugin with Tree-sitter S-expression queries → Task 3. ✓
- Parallel, `.gitignore`-aware indexing (`rayon`/`ignore`/`walkdir`) → Task 4. ✓
- CLI (`index`, `definition`, `references`, `callers`) → Task 5. ✓
- Name-index resolution for v0.1; Stack Graphs deferred to v0.2 → reflected in scope notes and README. ✓
- Coding standards (no unwrap/expect/unsafe, thiserror/anyhow split, Rustdoc) → Global Constraints + Task 6. ✓
- Testing (in-memory SQLite, dummy-source extraction, fixture repo, CLI e2e) → Tasks 2–5. ✓
- `content_hash` populated in v0.1, consumed in v0.2 → Task 4 (`parse_file`). ✓

**Type consistency:** `Symbol`/`Reference`/`FileNode`/`SymbolKind` signatures are identical across Tasks 1–5; query function names (`insert_file`, `insert_symbols`, `insert_references`, `find_definition`, `find_references`) match between Task 2 (definition), Task 4 (indexer), and Task 5 (CLI). `index_repository` signature matches between Task 4 and Task 5. ✓

**Placeholder scan:** No TBD/TODO; every code step contains complete code and exact commands with expected output. ✓

**Known v0.1 approximations (intentional, documented):** `references`/`callers` return call and macro sites only; name-based lookup reports all same-named matches. These are resolved by the v0.2 Stack Graphs precision layer.
