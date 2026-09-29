//! Rust language plugin: Tree-sitter based symbol and reference extraction.
//!
//! Symbols and references use manual, pre-order tree walks so we can track the
//! enclosing `mod`/`fn` scope for qualified `module_path` and reference
//! containers. `impl` extraction uses a compiled Tree-sitter [`Query`] cached
//! in a [`OnceLock`] so query compilation runs once per process.
//!
//! ## Known limitations
//!
//! - File-module paths are derived from a `src/` layout (`src/mcp/mod.rs` →
//!   `crate::mcp`). Non-`src` layouts fall back to path-relative segments.
//! - Impact analysis uses qualified identity with resolve-aware expansion; see
//!   `graph::impact`.
//! - Go has no trait-impl form; `implementations` stays empty for Go sources.

use super::{file_path_key, DepthGuard, LanguagePlugin, WalkBudget, MAX_WALK_DEPTH};
use crate::error::{Result, KeelError};
use crate::graph::types::{ImplRecord, Import, Reference, ReferenceKind, Symbol, SymbolKind};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor, Tree};

/// Compiled once: match `impl` blocks whose type target is a plain identifier.
const IMPL_QUERY_SRC: &str = r#"
(impl_item type: (type_identifier) @type)
(impl_item type: (generic_type) @type)
(impl_item type: (scoped_type_identifier) @type)
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
            .map_err(|e| KeelError::TreeSitter(e.to_string()))?;
        parser.parse(source, None).ok_or(KeelError::Parse)
    }
}

impl LanguagePlugin for RustPlugin {
    fn has_syntax_errors(&self, source_code: &str) -> bool {
        Self::parse(source_code)
            .map(|tree| tree.root_node().has_error())
            .unwrap_or(false)
    }

    fn extensions(&self) -> &[&str] {
        &["rs"]
    }

    fn extract_symbols(&self, path: &Path, source_code: &str) -> Result<Vec<Symbol>> {
        let tree = Self::parse(source_code)?;
        let src = source_code.as_bytes();
        let mut mods = rust_file_module_segments(path);
        let mut out = Vec::new();
        let budget = WalkBudget::new();
        walk_symbols(tree.root_node(), src, &mut mods, "", &mut out, &budget)?;
        Ok(out)
    }

    fn extract_references(&self, path: &Path, source_code: &str) -> Result<Vec<Reference>> {
        let tree = Self::parse(source_code)?;
        let src = source_code.as_bytes();
        let file_key = file_path_key(path);
        let file_mods = rust_file_module_segments(path);
        let mut scope: Vec<String> = Vec::new();
        let mut out = Vec::new();
        let budget = WalkBudget::new();
        walk_references(
            tree.root_node(),
            src,
            &file_key,
            &file_mods,
            &mut scope,
            &mut out,
            &budget,
        )?;
        Ok(out)
    }

    fn extract_imports(&self, _path: &Path, source_code: &str) -> Result<Vec<Import>> {
        let tree = Self::parse(source_code)?;
        let src = source_code.as_bytes();
        let mut out = Vec::new();
        let budget = WalkBudget::new();
        walk_imports(tree.root_node(), src, &mut out, &budget)?;
        Ok(out)
    }

    fn extract_impls(&self, _path: &Path, source_code: &str) -> Result<Vec<ImplRecord>> {
        let tree = Self::parse(source_code)?;
        let src = source_code.as_bytes();
        let query = impl_query();
        let mut cursor = QueryCursor::new();
        let mut out = Vec::new();
        let budget = WalkBudget::new();
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
                let Some(type_name) = impl_type_name(ty, src, &budget)? else {
                    continue;
                };
                let trait_name = match impl_item.child_by_field_name("trait") {
                    Some(t) => impl_type_name(t, src, &budget)?,
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

/// Outer name of an `impl` trait or type: generics erase (`Store<T>` →
/// `Store`, matching the TS heritage rule) and scopes collapse to the
/// final segment (`m::Store` → `Store`), since implementation lookup
/// matches bare names. Tuples, arrays, and `dyn` name no type and stay out.
fn impl_type_name(node: Node, src: &[u8], budget: &WalkBudget) -> Result<Option<String>> {
    let _guard = DepthGuard::enter(budget)
        .ok_or_else(|| KeelError::TooDeeplyNested { limit: MAX_WALK_DEPTH })?;
    match node.kind() {
        "type_identifier" | "identifier" => Ok(Some(node_text(node, src)?.to_string())),
        "generic_type" => match node.child_by_field_name("type") {
            Some(t) => impl_type_name(t, src, budget),
            None => Ok(None),
        },
        "scoped_type_identifier" => match node.child_by_field_name("name") {
            Some(n) if matches!(n.kind(), "type_identifier" | "identifier") => {
                Ok(Some(node_text(n, src)?.to_string()))
            }
            _ => Ok(None),
        },
        _ => Ok(None),
    }
}

/// UTF-8 text of a node, surfacing decode errors as `TreeSitter`.
fn node_text<'a>(node: Node, src: &'a [u8]) -> Result<&'a str> {
    node.utf8_text(src)
        .map_err(|e| KeelError::TreeSitter(e.to_string()))
}

/// Module path segments for a Rust source file from its path relative to a
/// crate `src/` root.
///
/// Examples: `src/lib.rs` → `[]`, `src/mcp/mod.rs` → `["mcp"]`,
/// `keel/src/mcp/wire.rs` → `["mcp", "wire"]`.
fn rust_file_module_segments(path: &Path) -> Vec<String> {
    let parts: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();

    let after_src = match parts.iter().position(|p| p == "src") {
        Some(idx) => &parts[idx + 1..],
        None => {
            // No `src/` — treat bare lib/main as crate root; otherwise use
            // parent directories + file stem (drop mod.rs).
            return segments_without_src_root(&parts);
        }
    };

    segments_from_src_relative(after_src)
}

fn segments_without_src_root(parts: &[String]) -> Vec<String> {
    if parts.is_empty() {
        return Vec::new();
    }
    let file = parts.last().map(|s| s.as_str()).unwrap_or("");
    if matches!(file, "lib.rs" | "main.rs") && parts.len() == 1 {
        return Vec::new();
    }
    segments_from_src_relative(parts)
}

fn segments_from_src_relative(after_src: &[String]) -> Vec<String> {
    let mut segs = Vec::new();
    for (i, part) in after_src.iter().enumerate() {
        let is_last = i + 1 == after_src.len();
        if is_last {
            if matches!(part.as_str(), "lib.rs" | "main.rs" | "mod.rs") {
                continue;
            }
            let stem = Path::new(part)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| part.clone());
            if !stem.is_empty() {
                segs.push(stem);
            }
        } else {
            segs.push(part.clone());
        }
    }
    segs
}

/// Top-scope `crate::` module identity for a Rust file, matching the
/// `module_path` symbols from that file carry.
pub fn rust_file_module_identity(path: &Path) -> String {
    qualify_mod(&rust_file_module_segments(path))
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
///
/// `file_mods` are path-derived segments (e.g. `mcp` for `src/mcp/mod.rs`);
/// `scope` is the inline `mod`/`fn` stack inside that file.
fn qualify_scope(file_key: &str, file_mods: &[String], scope: &[String]) -> String {
    let mut parts: Vec<&str> = Vec::with_capacity(file_mods.len() + scope.len());
    parts.extend(file_mods.iter().map(|s| s.as_str()));
    parts.extend(scope.iter().map(|s| s.as_str()));
    if parts.is_empty() {
        file_key.to_string()
    } else {
        format!("crate::{}", parts.join("::"))
    }
}

/// Pre-order walk emitting a [`Symbol`] per definition, tracking the enclosing
/// `mod` chain so each symbol records its qualified `module_path`.
fn walk_symbols(
    node: Node,
    src: &[u8],
    mods: &mut Vec<String>,
    container: &str,
    out: &mut Vec<Symbol>,
    budget: &WalkBudget,
) -> Result<()> {
    let _guard = DepthGuard::enter(budget)
        .ok_or_else(|| KeelError::TooDeeplyNested { limit: MAX_WALK_DEPTH })?;
    match node.kind() {
        "mod_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                let text = node_text(name, src)?;
                push_symbol(name, SymbolKind::Module, mods, container, out, src)?;
                mods.push(text.to_string());
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    walk_symbols(child, src, mods, container, out, budget)?;
                }
                mods.pop();
                return Ok(());
            }
        }
        "function_item" => emit_named_symbol(node, SymbolKind::Function, mods, container, out, src)?,
        // Bodiless trait declarations are definitions; metavariable
        // names (`$m`) are template placeholders, not symbols.
        "function_signature_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                if name.kind() == "identifier" {
                    push_symbol(name, SymbolKind::Function, mods, container, out, src)?;
                }
            }
        }
        "struct_item" => emit_named_symbol(node, SymbolKind::Struct, mods, container, out, src)?,
        "union_item" => emit_named_symbol(node, SymbolKind::Struct, mods, container, out, src)?,
        "trait_item" => emit_named_symbol(node, SymbolKind::Trait, mods, container, out, src)?,
        "enum_item" => emit_named_symbol(node, SymbolKind::Enum, mods, container, out, src)?,
        "const_item" => emit_named_symbol(node, SymbolKind::Const, mods, container, out, src)?,
        "static_item" => emit_named_symbol(node, SymbolKind::Const, mods, container, out, src)?,
        "macro_definition" => {
            emit_named_symbol(node, SymbolKind::Other("macro".into()), mods, container, out, src)?;
        }
        // Members are indexed items too: struct/union fields and enum
        // variants (`field` kind), so field reads resolve to a
        // definition. Tuple fields have no names and stay out.
        "field_declaration" | "enum_variant" => {
            emit_named_symbol(node, SymbolKind::Other("field".into()), mods, container, out, src)?;
        }
        "impl_item" => {
            // Match the historical behavior: only plain `type_identifier` impl
            // targets become an `Impl` symbol (skip `impl Vec<T>` etc.).
            if let Some(ty) = node.child_by_field_name("type") {
                if ty.kind() == "type_identifier" {
                    push_symbol(ty, SymbolKind::Impl, mods, container, out, src)?;
                }
            }
        }
        _ => {}
    }
    // Members of a type/impl/trait body carry the type name; anything
    // else inherits the enclosing container unchanged.
    let owned: Option<String> = match node.kind() {
        "impl_item" => match node.child_by_field_name("type") {
            Some(ty) => impl_type_name(ty, src, budget)?,
            None => None,
        },
        "struct_item" | "union_item" | "trait_item" | "enum_item" => {
            match node.child_by_field_name("name") {
                Some(name) if matches!(name.kind(), "type_identifier" | "identifier") => {
                    Some(node_text(name, src)?.to_string())
                }
                _ => None,
            }
        }
        _ => None,
    };
    let next_container = owned.as_deref().unwrap_or(container);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_symbols(child, src, mods, next_container, out, budget)?;
    }
    Ok(())
}

/// Emit a symbol from a node's `name` field child, if present.
fn emit_named_symbol(
    node: Node,
    kind: SymbolKind,
    mods: &[String],
    container: &str,
    out: &mut Vec<Symbol>,
    src: &[u8],
) -> Result<()> {
    if let Some(name) = node.child_by_field_name("name") {
        push_symbol(name, kind, mods, container, out, src)?;
    }
    Ok(())
}

/// Push a symbol for the identifier `name_node` with the current module path.
fn push_symbol(
    name_node: Node,
    kind: SymbolKind,
    mods: &[String],
    container: &str,
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
        container: container.to_string(),
    });
    Ok(())
}

/// Pre-order walk emitting a [`Reference`] per call/macro/method/type site,
/// tracking the enclosing `fn`/`mod` chain for each reference's `container`.
fn walk_references(
    node: Node,
    src: &[u8],
    file_key: &str,
    file_mods: &[String],
    scope: &mut Vec<String>,
    out: &mut Vec<Reference>,
    budget: &WalkBudget,
) -> Result<()> {
    let _guard = DepthGuard::enter(budget)
        .ok_or_else(|| KeelError::TooDeeplyNested { limit: MAX_WALK_DEPTH })?;
    match node.kind() {
        "mod_item" | "function_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                let text = node_text(name, src)?;
                scope.push(text.to_string());
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    walk_references(child, src, file_key, file_mods, scope, out, budget)?;
                }
                scope.pop();
                return Ok(());
            }
        }
        "call_expression" => {
            if let Some(func) = node.child_by_field_name("function") {
                emit_call_reference(func, src, file_key, file_mods, scope, out)?;
            }
            if let Some(args) = node.child_by_field_name("arguments") {
                emit_argument_values(args, src, file_key, file_mods, scope, out)?;
            }
        }
        "macro_invocation" => {
            if let Some(mac) = node.child_by_field_name("macro") {
                if let Some((name, target)) = final_segment(mac, src)? {
                    push_reference(
                        name.clone(),
                        target,
                        ReferenceKind::Macro,
                        file_key,
                        file_mods,
                        scope,
                        out,
                    );
                    if !inside_macro_definition(node) {
                        emit_format_holes(
                            node, &name, src, file_key, file_mods, scope, out,
                        )?;
                    }
                }
            }
        }
        // Macro arguments are token soup, but bare identifiers in them
        // are real reads (`vec![seed]`, `assert_eq!(a, b)`, `#[derive(
        // Debug, Clone)]` — attribute arguments are token trees too).
        // Only DIRECT identifier children emit here; nested token trees
        // fall through to the recursion below so each site emits exactly
        // once. `macro_rules!` bodies declare patterns and templates,
        // never uses, so token trees under `macro_definition` stay out.
        // (Plain string contents stay opaque; format-macro holes like
        // `format!("{x}")` are owned by the `macro_invocation` arm, and
        // DSL labels like `rename` in `#[serde(rename = ...)]` read as
        // mentions.)
        "token_tree" if !inside_macro_definition(node) => {
            emit_direct_identifiers(node, src, file_key, file_mods, scope, out)?;
        }
        // Field reads (`cfg.port`) read the receiver, mirroring the
        // receiver rule for calls, and the field itself. Call callees
        // are owned by the call arm and skipped so no site emits
        // twice; fields in plain-assignment LHS position (through
        // tuple/array/paren targets) are writes, never reads.
        "field_expression" => {
            emit_field_receiver(node, src, file_key, file_mods, scope, out)?;
            emit_field_property(node, src, file_key, file_mods, scope, out)?;
        }
        // Enum discriminants (`A = LIMIT`) read.
        "enum_variant" => {
            emit_field_identifier(node, "value", src, file_key, file_mods, scope, out)?;
        }
        // Struct literal shorthand (`Point { x }`) reads the variable.
        // Pattern shorthand uses the distinct `shorthand_field_identifier`
        // node and stays a binding.
        "shorthand_field_initializer" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "identifier" {
                    push_reference(
                        node_text(child, src)?.to_string(),
                        child,
                        ReferenceKind::Value,
                        file_key,
                        file_mods,
                        scope,
                        out,
                    );
                }
            }
        }
        // A type usage, unless this identifier is the *name* of its parent
        // definition (struct/enum/trait/union/type alias or a generic type
        // parameter), which would double-count the definition site — or a
        // segment of a path, which the scoped-path arm owns with
        // its qualifier.
        "type_identifier"
            if !is_definition_name(node) && !inside_scoped_path(node) =>
        {
            let text = node_text(node, src)?;
            push_reference(
                text.to_string(),
                node,
                ReferenceKind::Type,
                file_key,
                file_mods,
                scope,
                out,
            );
        }
        // Path uses (`crate::m::C` in expressions, `m::S` in type
        // positions) read their final segment with the qualifier
        // attached for tie-breaking. Only the outermost path emits
        // (nested prefixes are modules, not uses); `use` paths are
        // imports, call callees and macro paths belong to their arms,
        // and macro bodies stay opaque token soup.
        "scoped_identifier" | "scoped_type_identifier" => {
            emit_path_use(node, src, file_key, file_mods, scope, out)?;
        }
        // Value reads in statement/expression positions (`return x`,
        // `a + b`, `-x`, block tails, bare `x;`, `(a, b)`, `a[b]`,
        // `a..b`, `x?`, `&x`, `break x`, `(x)`, array elements and
        // repeat lengths): only DIRECT identifier children emit here;
        // nested composites fall through to the recursion below so each
        // site emits exactly once.
        "return_expression" | "binary_expression" | "unary_expression"
        | "block" | "expression_statement" | "tuple_expression"
        | "index_expression" | "range_expression" | "try_expression"
        | "reference_expression" | "break_expression"
        | "parenthesized_expression" | "array_expression" => {
            emit_direct_identifiers(node, src, file_key, file_mods, scope, out)?;
        }
        // `let`/`if-let` initializers read; patterns bind.
        "let_declaration" | "let_condition" => {
            emit_field_identifier(node, "value", src, file_key, file_mods, scope, out)?;
        }
        // Plain assignment reads only the RHS (`x = v`); the LHS is a
        // write, never a read. Compound assignment (`x += v`) reads its
        // target too.
        "assignment_expression" => {
            emit_field_identifier(node, "right", src, file_key, file_mods, scope, out)?;
        }
        "compound_assignment_expr" => {
            emit_field_identifier(node, "left", src, file_key, file_mods, scope, out)?;
            emit_field_identifier(node, "right", src, file_key, file_mods, scope, out)?;
        }
        // Conditions, iterables, and scrutinees read (`if x`, `while x`,
        // `for _ in ITER`, `match SUBJ`); block arm bodies recurse
        // normally while bare ones (`_ => x`) read here. Patterns bind.
        "for_expression" | "match_expression" | "match_arm" => {
            emit_field_identifier(node, "value", src, file_key, file_mods, scope, out)?;
        }
        "while_expression" | "if_expression" => {
            emit_field_identifier(node, "condition", src, file_key, file_mods, scope, out)?;
        }
        // Struct literal fields (`Point { x: v }`): the key names a
        // declared field, so it reads; the value reads as usual.
        // Pattern fields (`field_pattern`) and declarations
        // (`field_declaration`) use distinct nodes and stay out.
        "field_initializer" => {
            emit_field_identifier(node, "value", src, file_key, file_mods, scope, out)?;
            if let Some(field) = node.child_by_field_name("field") {
                if field.kind() == "field_identifier" {
                    push_reference(
                        node_text(field, src)?.to_string(),
                        field,
                        ReferenceKind::Value,
                        file_key,
                        file_mods,
                        scope,
                        out,
                    );
                }
            }
        }
        // Casts (`x as T`) read the value side; the type side owns
        // itself via the type arm.
        "type_cast_expression" => {
            emit_field_identifier(node, "value", src, file_key, file_mods, scope, out)?;
        }
        // Static and const initializers (`static L: T = BASE`); the
        // bound name is a definition, never a reference.
        "static_item" | "const_item" => {
            emit_field_identifier(node, "value", src, file_key, file_mods, scope, out)?;
        }
        // Bare closure bodies (`|| x`); block bodies recurse normally
        // and parameters bind.
        "closure_expression" => {
            emit_field_identifier(node, "body", src, file_key, file_mods, scope, out)?;
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_references(child, src, file_key, file_mods, scope, out, budget)?;
    }
    Ok(())
}

/// Emit a reference for the `function` child of a call expression.
fn emit_call_reference(
    func: Node,
    src: &[u8],
    file_key: &str,
    file_mods: &[String],
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
                file_mods,
                scope,
                out,
            );
        }
        "scoped_identifier" => {
            if let Some((name, target)) = final_segment(func, src)? {
                push_path_reference(
                    name,
                    target,
                    qualifier_of(func, src)?,
                    file_key,
                    file_mods,
                    scope,
                    out,
                );
            }
        }
        "field_expression" => {
            if let Some(field) = func.child_by_field_name("field") {
                if field.kind() == "field_identifier" {
                    let text = node_text(field, src)?;
                    let qualifier = func
                        .child_by_field_name("value")
                        .map(|v| node_text(v, src).map(str::to_string))
                        .transpose()?
                        .unwrap_or_default();
                    push_method_reference(
                        text.to_string(),
                        field,
                        qualifier,
                        file_key,
                        file_mods,
                        scope,
                        out,
                    );
                }
            }
            // The receiver (`db` in `db.get()`) is a value use.
            if let Some(recv) = func.child_by_field_name("value") {
                if recv.kind() == "identifier" {
                    push_reference(
                        node_text(recv, src)?.to_string(),
                        recv,
                        ReferenceKind::Value,
                        file_key,
                        file_mods,
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

/// Bare identifiers passed directly as call arguments are value references
/// (`handler` in `spawn(handler)`), matching Python/TypeScript/JavaScript.
/// Only direct children count: nested calls keep their own callee references
/// via the generic recursion, receivers are captured with the callee, and
/// binding sites stay untouched.
fn emit_argument_values(
    args: Node,
    src: &[u8],
    file_key: &str,
    file_mods: &[String],
    scope: &[String],
    out: &mut Vec<Reference>,
) -> Result<()> {
    let mut cursor = args.walk();
    for child in args.children(&mut cursor) {
        if child.kind() == "identifier" {
            push_reference(
                node_text(child, src)?.to_string(),
                child,
                ReferenceKind::Value,
                file_key,
                file_mods,
                scope,
                out,
            );
        }
    }
    Ok(())
}

/// Emit the receiver of a field read (`cfg` in `cfg.port`) as a Value
/// reference. Call callees (`svc.run()`) are owned by the call arm and
/// skipped; the check is direct so chains still read their base.
fn emit_field_receiver(
    node: Node,
    src: &[u8],
    file_key: &str,
    file_mods: &[String],
    scope: &[String],
    out: &mut Vec<Reference>,
) -> Result<()> {
    let Some(recv) = node.child_by_field_name("value") else {
        return Ok(());
    };
    if recv.kind() != "identifier" {
        return Ok(());
    }
    if let Some(parent) = node.parent() {
        if parent.kind() == "call_expression" {
            return Ok(());
        }
    }
    push_reference(
        node_text(recv, src)?.to_string(),
        recv,
        ReferenceKind::Value,
        file_key,
        file_mods,
        scope,
        out,
    );
    Ok(())
}

/// Emit the field of a value-position field read (`port` in `cfg.port`)
/// as a Value reference, keeping the receiver text as the qualifier
/// exactly like method calls. Call callees are owned by the call arm;
/// fields in plain-assignment LHS position (through tuple/array/paren
/// targets) are writes. Compound targets read, so they emit. Tuple
/// indices (`a.0`) have no name and stay out.
fn emit_field_property(
    node: Node,
    src: &[u8],
    file_key: &str,
    file_mods: &[String],
    scope: &[String],
    out: &mut Vec<Reference>,
) -> Result<()> {
    let Some(field) = node.child_by_field_name("field") else {
        return Ok(());
    };
    if field.kind() != "field_identifier" {
        return Ok(());
    }
    let Some(object) = node.child_by_field_name("value") else {
        return Ok(());
    };
    // Call callees are owned by the call arm; the check is direct so
    // chains still read their middle (`a.b.c()` reads `b` via the
    // inner field).
    if node.parent().is_some_and(|p| p.kind() == "call_expression") {
        return Ok(());
    }
    // Plain-assignment LHS writes the field; destructuring targets
    // nest, so the check climbs through transparent wrappers first.
    let target = climb_target_wrappers(node);
    if let Some(parent) = target.parent() {
        if parent.kind() == "assignment_expression"
            && parent
                .child_by_field_name("left")
                .is_some_and(|left| is_within(target, left))
        {
            return Ok(());
        }
    }
    let qualifier = node_text(object, src)?.to_string();
    push_value_reference(
        node_text(field, src)?.to_string(),
        field,
        qualifier,
        file_key,
        file_mods,
        scope,
        out,
    );
    Ok(())
}

/// True when `node` sits at or under `ancestor` (inclusive).
fn is_within(mut node: Node, ancestor: Node) -> bool {
    loop {
        if node.id() == ancestor.id() {
            return true;
        }
        match node.parent() {
            Some(parent) => node = parent,
            None => return false,
        }
    }
}

/// Climb from `node` through transparent target wrappers (tuple, array,
/// and paren forms) to the enclosing target itself, so `(a.b, c) = v`
/// detects the write position. Value wrappers climb harmlessly: the
/// landing slot check decides.
fn climb_target_wrappers(mut node: Node) -> Node {
    while let Some(parent) = node.parent() {
        if matches!(
            parent.kind(),
            "tuple_expression" | "array_expression" | "parenthesized_expression"
        ) {
            node = parent;
        } else {
            break;
        }
    }
    node
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

/// Emit a scoped-path use (`m::C`, `crate::m::C`, `m::S`) as a qualified
/// `Path` reference on its final segment. Nested prefixes are modules,
/// not uses; call callees and macro paths belong to their arms; `use`
/// paths are imports; macro bodies stay opaque. Paths nested anywhere
/// else — including call *arguments* — still read.
fn emit_path_use(
    node: Node,
    src: &[u8],
    file_key: &str,
    file_mods: &[String],
    scope: &[String],
    out: &mut Vec<Reference>,
) -> Result<()> {
    if let Some(parent) = node.parent() {
        // A nested path prefix (`a::b` in `a::b::C`) is a module, not a use.
        if parent.kind() == "scoped_identifier" || parent.kind() == "scoped_type_identifier" {
            return Ok(());
        }
        // A call callee (`m::serve()`); the call arm owns it. Argument
        // paths (`f(m::C)`) have an `arguments` parent and still emit.
        if parent.kind() == "call_expression"
            && parent
                .child_by_field_name("function")
                .is_some_and(|f| f == node)
        {
            return Ok(());
        }
        // A macro path (`mac!`); the macro arm owns it.
        if parent.kind() == "macro_invocation" {
            return Ok(());
        }
        // `use` paths are imports; scoped paths in macro bodies stay
        // out (bare identifiers there emit via the `token_tree` arm).
        let mut current = parent;
        loop {
            if current.kind() == "use_declaration" || current.kind() == "token_tree" {
                return Ok(());
            }
            match current.parent() {
                Some(p) => current = p,
                None => break,
            }
        }
    }
    let (name, target) = match node.child_by_field_name("name") {
        Some(name) => (node_text(name, src)?.to_string(), name),
        None => return Ok(()),
    };
    push_path_reference(
        name,
        target,
        qualifier_of(node, src)?,
        file_key,
        file_mods,
        scope,
        out,
    );
    Ok(())
}

/// True when `node` is a direct segment of a scoped path (the path arm
/// owns it with its qualifier).
fn inside_scoped_path(node: Node) -> bool {
    node.parent().is_some_and(|p| {
        p.kind() == "scoped_identifier" || p.kind() == "scoped_type_identifier"
    })
}

/// True when `node` sits inside a `macro_rules!` definition, whose token
/// trees declare match patterns and expansion templates rather than
/// using names.
fn inside_macro_definition(mut node: Node) -> bool {
    while let Some(parent) = node.parent() {
        if parent.kind() == "macro_definition" {
            return true;
        }
        node = parent;
    }
    false
}

/// Emit DIRECT identifier children as Value references. Nested
/// composites are left to the normal recursion so each site emits exactly
/// once (no double emits: every identifier has exactly one parent arm).
fn emit_direct_identifiers(
    node: Node,
    src: &[u8],
    file_key: &str,
    file_mods: &[String],
    scope: &[String],
    out: &mut Vec<Reference>,
) -> Result<()> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "identifier" {
            push_reference(
                node_text(child, src)?.to_string(),
                child,
                ReferenceKind::Value,
                file_key,
                file_mods,
                scope,
                out,
            );
        }
    }
    Ok(())
}

/// std format-macro names with the 1-based position of the format
/// string among the top-level arguments (`write!` takes the destination
/// first; `assert_eq!`/`assert_ne!` take two compared values first).
/// Macros outside this table (`vec!`, `concat!`, `compile_error!`,
/// `include_str!`) never interpolate, so their strings stay opaque.
const FORMAT_MACRO_ARG: &[(&str, usize)] = &[
    ("format", 1),
    ("format_args", 1),
    ("print", 1),
    ("println", 1),
    ("eprint", 1),
    ("eprintln", 1),
    ("panic", 1),
    ("assert", 1),
    ("assert_eq", 3),
    ("assert_ne", 3),
    ("debug_assert", 1),
    ("debug_assert_eq", 3),
    ("debug_assert_ne", 3),
    ("write", 2),
    ("writeln", 2),
    ("unreachable", 1),
    ("todo", 1),
    ("unimplemented", 1),
];

/// Emit `{hole}` reads from a format macro's format string
/// (`format!("hello {name}")` reads `name`). Only the argument at the
/// macro's format position is scanned, and only when it is a string
/// literal — compared values (`assert_eq!(a, "{b}")`) and non-literal
/// format arguments stay out.
fn emit_format_holes(
    node: Node,
    macro_name: &str,
    src: &[u8],
    file_key: &str,
    file_mods: &[String],
    scope: &[String],
    out: &mut Vec<Reference>,
) -> Result<()> {
    let Some(arg_pos) = FORMAT_MACRO_ARG
        .iter()
        .find(|(name, _)| *name == macro_name)
        .map(|(_, pos)| *pos)
    else {
        return Ok(());
    };
    let mut cursor = node.walk();
    let token_tree = node
        .children(&mut cursor)
        .find(|c| c.kind() == "token_tree");
    let Some(tree) = token_tree else {
        return Ok(());
    };
    // Top-level arguments: direct children split on anonymous commas
    // (nested groups are opaque single children, so no depth tracking).
    let mut cursor = tree.walk();
    let mut segment = 1usize;
    let mut literal: Option<Node> = None;
    for child in tree.children(&mut cursor) {
        if !child.is_named() && node_text(child, src).is_ok_and(|t| t == ",") {
            segment += 1;
            continue;
        }
        if segment == arg_pos && child.is_named() && child.kind() == "string_literal" {
            literal = Some(child);
            break;
        }
    }
    let Some(lit) = literal else {
        return Ok(());
    };
    let text = node_text(lit, src)?;
    for (name, offset) in format_holes(text) {
        let (line, col) = line_col_at(src, lit.start_byte() + offset);
        push_reference_at(
            name, line, col, ReferenceKind::Value, file_key, file_mods, scope, out,
        );
    }
    Ok(())
}

/// Named holes in a Rust format-string literal: each `(name, byte offset
/// within the literal)`. Escaped braces (`{{`, `}}`), positional and
/// empty holes (`{}`, `{0}`, `{:?}`), and spec text stay out; `$`
/// width/precision parameters (`{v:>w$}`, `{:.p$}`) read.
fn format_holes(literal: &str) -> Vec<(String, usize)> {
    let bytes = literal.as_bytes();
    let Some(content_start) = bytes.iter().position(|b| *b == b'"').map(|i| i + 1) else {
        return Vec::new();
    };
    let mut holes = Vec::new();
    let mut i = content_start;
    while i < bytes.len() {
        match bytes[i] {
            b'{' if bytes.get(i + 1) == Some(&b'{') => i += 2,
            b'}' => i += 1,
            b'{' => {
                i += 1;
                if let Some((ident, end)) = parse_hole_ident(bytes, i) {
                    holes.push((ident, i));
                    i = end;
                }
                i = scan_hole_spec(bytes, i, &mut holes);
            }
            _ => i += 1,
        }
    }
    holes
}

/// Parse `[A-Za-z_][A-Za-z0-9_]*` at `start`, returning the identifier
/// and the offset one past its end.
fn parse_hole_ident(bytes: &[u8], start: usize) -> Option<(String, usize)> {
    let first = *bytes.get(start)?;
    if !(first == b'_' || first.is_ascii_alphabetic()) {
        return None;
    }
    let mut end = start + 1;
    while end < bytes.len() && (bytes[end] == b'_' || bytes[end].is_ascii_alphanumeric()) {
        end += 1;
    }
    Some((String::from_utf8_lossy(&bytes[start..end]).into_owned(), end))
}

/// Scan a hole's format spec up to (and past) its closing `}`, recording
/// `$` width/precision idents (`{v:>w$}`). Returns the offset to resume
/// the outer scan from.
fn scan_hole_spec(
    bytes: &[u8],
    mut i: usize,
    holes: &mut Vec<(String, usize)>,
) -> usize {
    while i < bytes.len() {
        match bytes[i] {
            b'}' => return i + 1,
            b'_' | b'a'..=b'z' | b'A'..=b'Z' => {
                if let Some((ident, end)) = parse_hole_ident(bytes, i) {
                    if bytes.get(end) == Some(&b'$') {
                        holes.push((ident, i));
                    }
                    i = end;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    i
}

/// 1-based (line, column) for a byte offset, with byte-based columns
/// matching tree-sitter positions.
fn line_col_at(src: &[u8], offset: usize) -> (u32, u32) {
    let mut line = 1u32;
    let mut line_start = 0usize;
    for (i, b) in src.iter().enumerate() {
        if i >= offset {
            break;
        }
        if *b == b'\n' {
            line += 1;
            line_start = i + 1;
        }
    }
    (line, (offset - line_start) as u32 + 1)
}

/// Emit the `field` child as a Value reference when it is a bare
/// identifier (composite values fall through to the normal recursion).
fn emit_field_identifier(
    node: Node,
    field: &str,
    src: &[u8],
    file_key: &str,
    file_mods: &[String],
    scope: &[String],
    out: &mut Vec<Reference>,
) -> Result<()> {
    if let Some(child) = node.child_by_field_name(field) {
        if child.kind() == "identifier" {
            push_reference(
                node_text(child, src)?.to_string(),
                child,
                ReferenceKind::Value,
                file_key,
                file_mods,
                scope,
                out,
            );
        }
    }
    Ok(())
}

/// True when `node` is the `name` field of its parent definition, i.e. a
/// declaration site rather than a use of a type. Only genuine definition
/// parents count: use-position `name` fields (struct literal paths,
/// scoped-path segments) are uses, not declarations.
fn is_definition_name(node: Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if !matches!(
        parent.kind(),
        "struct_item" | "enum_item" | "union_item" | "trait_item" | "type_item" | "type_parameter"
    ) {
        return false;
    }
    parent
        .child_by_field_name("name")
        .is_some_and(|n| n == node)
}

/// Push a reference with the current enclosing-scope container.
fn push_reference(
    name: String,
    node: Node,
    kind: ReferenceKind,
    file_key: &str,
    file_mods: &[String],
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
        container: qualify_scope(file_key, file_mods, scope),
        qualifier: String::new(),
    });
}

/// Push a reference at an explicit 1-based (line, column) for reads with
/// no identifier node of their own (format-string holes).
#[allow(clippy::too_many_arguments)]
fn push_reference_at(
    name: String,
    line: u32,
    col: u32,
    kind: ReferenceKind,
    file_key: &str,
    file_mods: &[String],
    scope: &[String],
    out: &mut Vec<Reference>,
) {
    out.push(Reference {
        name,
        file: PathBuf::new(),
        start_line: line,
        start_col: col,
        kind,
        container: qualify_scope(file_key, file_mods, scope),
        qualifier: String::new(),
    });
}

/// Push a qualified-path reference, keeping the qualifier prefix (`mcp` in
/// `mcp::serve`) so the resolver can break module ties.
fn push_path_reference(
    name: String,
    node: Node,
    qualifier: String,
    file_key: &str,
    file_mods: &[String],
    scope: &[String],
    out: &mut Vec<Reference>,
) {
    let pos = node.start_position();
    out.push(Reference {
        name,
        file: PathBuf::new(),
        start_line: pos.row as u32 + 1,
        start_col: pos.column as u32 + 1,
        kind: ReferenceKind::Path,
        container: qualify_scope(file_key, file_mods, scope),
        qualifier,
    });
}

/// Push a method reference with an explicit qualifier (the receiver text
/// for method calls, `db` in `db.get()`).
fn push_method_reference(
    name: String,
    node: Node,
    qualifier: String,
    file_key: &str,
    file_mods: &[String],
    scope: &[String],
    out: &mut Vec<Reference>,
) {
    let pos = node.start_position();
    out.push(Reference {
        name,
        file: PathBuf::new(),
        start_line: pos.row as u32 + 1,
        start_col: pos.column as u32 + 1,
        kind: ReferenceKind::Method,
        container: qualify_scope(file_key, file_mods, scope),
        qualifier,
    });
}

/// Push a value reference with an explicit qualifier (the receiver text
/// for field reads, `cfg` in `cfg.port`).
fn push_value_reference(
    name: String,
    node: Node,
    qualifier: String,
    file_key: &str,
    file_mods: &[String],
    scope: &[String],
    out: &mut Vec<Reference>,
) {
    let pos = node.start_position();
    out.push(Reference {
        name,
        file: PathBuf::new(),
        start_line: pos.row as u32 + 1,
        start_col: pos.column as u32 + 1,
        kind: ReferenceKind::Value,
        container: qualify_scope(file_key, file_mods, scope),
        qualifier,
    });
}

/// Qualifier prefix of a scoped path: the full path text minus the final
/// `::name` segment (`mcp` in `mcp::serve`, `crate::mcp` in
/// `crate::mcp::serve`). Empty when the shape is unexpected.
fn qualifier_of(path_node: Node, src: &[u8]) -> Result<String> {
    let full = node_text(path_node, src)?;
    match full.rsplit_once("::") {
        Some((prefix, _)) => Ok(prefix.trim().to_string()),
        None => Ok(String::new()),
    }
}

/// Pre-order walk emitting an [`Import`] per imported path, expanding grouped
/// `use a::{b, c as d}` lists into one record each.
fn walk_imports(
    node: Node,
    src: &[u8],
    out: &mut Vec<Import>,
    budget: &WalkBudget,
) -> Result<()> {
    let _guard = DepthGuard::enter(budget)
        .ok_or_else(|| KeelError::TooDeeplyNested { limit: MAX_WALK_DEPTH })?;
    if node.kind() == "use_declaration" {
        if let Some(arg) = node.child_by_field_name("argument") {
            expand_use(arg, "", src, out)?;
        }
        return Ok(());
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_imports(child, src, out, budget)?;
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
    fn member_symbols_carry_enclosing_type_container() {
        let plugin = RustPlugin;
        let syms = plugin
            .extract_symbols(
                test_path(),
                "fn top() {}\nstruct Store { limit: u32 }\nimpl Store {\n    fn save(&self) {}\n}\ntrait Repo {\n    fn load(&self);\n}\n",
            )
            .unwrap();
        let find = |n: &str| syms.iter().find(|s| s.name == n).cloned();
        assert_eq!(find("top").unwrap().container, "");
        assert_eq!(find("Store").unwrap().container, "");
        assert_eq!(find("limit").unwrap().container, "Store");
        assert_eq!(find("save").unwrap().container, "Store");
        assert_eq!(find("load").unwrap().container, "Repo");
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
    fn extracts_generic_and_scoped_impls_by_outer_name() {
        let plugin = RustPlugin;
        let src = "impl<T> Store<T> for Mem<T> {}\nimpl m::Logger for Svc {}\nimpl Cache for store::Lru {}\nimpl<T> Debug2 for (T, T) {}\n";
        let impls = plugin.extract_impls(test_path(), src).unwrap();
        let pairs: Vec<(&str, &str)> = impls
            .iter()
            .map(|r| {
                (
                    r.type_name.as_str(),
                    r.trait_name.as_deref().unwrap_or("<none>"),
                )
            })
            .collect();
        // Generics erase and scopes collapse on both sides; tuples name
        // no type and stay out.
        assert_eq!(
            pairs,
            vec![("Mem", "Store"), ("Svc", "Logger"), ("Lru", "Cache")]
        );
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

    #[test]
    fn file_module_path_from_src_layout() {
        let plugin = RustPlugin;
        let src = "pub fn serve() {}\n";
        let syms = plugin
            .extract_symbols(Path::new("src/mcp/mod.rs"), src)
            .unwrap();
        let serve = syms.iter().find(|s| s.name == "serve").unwrap();
        assert_eq!(serve.module_path, "crate::mcp");
    }

    #[test]
    fn nested_file_module_path() {
        let plugin = RustPlugin;
        let src = "pub struct Wire;\n";
        let syms = plugin
            .extract_symbols(Path::new("src/mcp/wire.rs"), src)
            .unwrap();
        assert_eq!(syms[0].module_path, "crate::mcp::wire");
    }

    #[test]
    fn lib_rs_stays_crate_root() {
        let plugin = RustPlugin;
        let src = "pub struct Index;\n";
        let syms = plugin
            .extract_symbols(Path::new("src/lib.rs"), src)
            .unwrap();
        assert_eq!(syms[0].module_path, "crate");
    }

    #[test]
    fn nested_crate_path_still_finds_src() {
        let plugin = RustPlugin;
        let src = "pub fn serve() {}\n";
        let syms = plugin
            .extract_symbols(Path::new("keel/src/mcp/mod.rs"), src)
            .unwrap();
        assert_eq!(syms[0].module_path, "crate::mcp");
    }

    #[test]
    fn file_module_reference_container_includes_path_mods() {
        let plugin = RustPlugin;
        let src = "fn serve() { read_message(); }\n";
        let refs = plugin
            .extract_references(Path::new("src/mcp/mod.rs"), src)
            .unwrap();
        let call = refs.iter().find(|r| r.name == "read_message").unwrap();
        assert_eq!(call.container, "crate::mcp::serve");
    }

    #[test]
    fn extracts_const_and_static_items() {
        let plugin = RustPlugin;
        let syms = plugin
            .extract_symbols(test_path(), "const LIMIT: usize = 8;\nstatic COUNT: u32 = 0;\n")
            .unwrap();
        let limit = syms.iter().find(|s| s.name == "LIMIT").expect("LIMIT symbol");
        assert_eq!(limit.kind, SymbolKind::Const);
        assert_eq!(limit.start_line, 1);
        let count = syms.iter().find(|s| s.name == "COUNT").expect("COUNT symbol");
        assert_eq!(count.kind, SymbolKind::Const);
        assert_eq!(count.start_line, 2);
    }

    #[test]
    fn bodiless_trait_declarations_are_definitions() {
        let plugin = RustPlugin;
        let syms = plugin
            .extract_symbols(test_path(), "pub trait Store {\n    fn get(&self);\n}\n")
            .unwrap();
        let get = syms.iter().find(|s| s.name == "get").expect("get symbol");
        assert_eq!(get.kind, SymbolKind::Function);
        assert_eq!(get.start_line, 2);
    }

    #[test]
    fn union_items_are_struct_symbols() {
        let plugin = RustPlugin;
        let syms = plugin
            .extract_symbols(test_path(), "pub union Word {\n    pub w: u32,\n}\n")
            .unwrap();
        let word = syms.iter().find(|s| s.name == "Word").expect("Word symbol");
        assert_eq!(word.kind, SymbolKind::Struct);
        assert_eq!(word.start_line, 1);
    }

    #[test]
    fn call_arguments_capture_bare_value_references() {
        let plugin = RustPlugin;
        let src = r#"fn main() {
    let handler = setup;
    spawn(handler, 42);
    svc.run(handler);
    outer(inner(deep));
}
"#;
        let refs = plugin.extract_references(test_path(), src).unwrap();
        let has = |name: &str, kind: ReferenceKind| {
            refs.iter().any(|r| r.name == name && r.kind == kind)
        };
        // Direct bare-identifier arguments are Value references.
        assert!(has("handler", ReferenceKind::Value));
        assert!(has("deep", ReferenceKind::Value));
        // Callees keep their own kinds; nothing double-emits as Value.
        assert!(has("spawn", ReferenceKind::Call));
        assert!(has("run", ReferenceKind::Method));
        assert!(!has("spawn", ReferenceKind::Value));
        // The method receiver is a value use and the call qualifier.
        assert!(has("svc", ReferenceKind::Value));
        let run = refs
            .iter()
            .find(|r| r.name == "run" && r.kind == ReferenceKind::Method)
            .expect("run method");
        assert_eq!(run.qualifier, "svc");
        // Let-initializers read too.
        assert!(has("setup", ReferenceKind::Value));
    }

    #[test]
    fn metavariable_declarations_are_not_symbols() {
        let plugin = RustPlugin;
        let syms = plugin
            .extract_symbols(test_path(), "pub trait Store {\n    fn $get(&self);\n}\n")
            .unwrap();
        assert!(syms.iter().all(|s| s.kind != SymbolKind::Function), "syms: {syms:?}");
    }

    #[test]
    fn macro_definitions_are_symbols() {
        let plugin = RustPlugin;
        let syms = plugin
            .extract_symbols(test_path(), "macro_rules! say {\n    ($x:expr) => { $x };\n}\n")
            .unwrap();
        let mac = syms.iter().find(|s| s.name == "say").expect("say symbol");
        assert_eq!(mac.kind, SymbolKind::Other("macro".to_string()));
        assert_eq!(mac.start_line, 1);
    }

    #[test]
    fn struct_literal_shorthand_reads_but_patterns_bind() {
        let plugin = RustPlugin;
        let refs = plugin
            .extract_references(
                test_path(),
                "struct Point { x: f64 }\nfn make(x: f64) -> Point {\n    Point { x }\n}\nfn get(p: Point) -> f64 {\n    let Point { x } = p;\n    x\n}\n",
            )
            .unwrap();

        let x: Vec<&Reference> = refs.iter().filter(|r| r.name == "x").collect();
        // Literal shorthand and the tail read of the binding are Value
        // reads; the pattern binding itself is not.
        assert_eq!(x.len(), 2, "refs: {refs:?}");
        assert!(x.iter().all(|r| r.kind == ReferenceKind::Value));
        let mut lines: Vec<u32> = x.iter().map(|r| r.start_line).collect();
        lines.sort_unstable();
        assert_eq!(lines, vec![3, 7]);
    }

    #[test]
    fn struct_literal_keys_read_but_patterns_and_decls_stay_out() {
        let plugin = RustPlugin;
        let refs = plugin
            .extract_references(
                test_path(),
                "struct Point { x: f64 }\nfn make(v: f64) -> Point {\n    Point { x: v }\n}\nfn get(p: Point) -> f64 {\n    let Point { x: w } = p;\n    w\n}\n",
            )
            .unwrap();

        // Exactly the literal key reads; the declaration and the
        // pattern field bind, never read.
        let x: Vec<&Reference> = refs.iter().filter(|r| r.name == "x").collect();
        assert_eq!(x.len(), 1, "refs: {refs:?}");
        assert_eq!(x[0].kind, ReferenceKind::Value);
        assert_eq!((x[0].start_line, x[0].start_col), (3, 13));
        let v: Vec<&Reference> = refs.iter().filter(|r| r.name == "v").collect();
        assert_eq!(v.len(), 1, "refs: {refs:?}");
        let w: Vec<&Reference> = refs.iter().filter(|r| r.name == "w").collect();
        assert_eq!(w.len(), 1, "refs: {refs:?}");
        let p: Vec<&Reference> = refs.iter().filter(|r| r.name == "p").collect();
        assert_eq!(p.len(), 1, "refs: {refs:?}");
    }

    #[test]
    fn static_and_const_initializers_read() {
        let plugin = RustPlugin;
        let refs = plugin
            .extract_references(
                test_path(),
                "const BASE: i32 = 7;\nstatic LIMIT: i32 = BASE;\nconst TWICE: i32 = BASE;\n",
            )
            .unwrap();

        let base: Vec<&Reference> = refs.iter().filter(|r| r.name == "BASE").collect();
        assert_eq!(base.len(), 2, "refs: {refs:?}");
        assert!(base.iter().all(|r| r.kind == ReferenceKind::Value));
        let mut lines: Vec<u32> = base.iter().map(|r| r.start_line).collect();
        lines.sort_unstable();
        assert_eq!(lines, vec![2, 3]);
        // Bound names are definitions, never references.
        for bound in ["LIMIT", "TWICE"] {
            assert!(refs.iter().all(|r| r.name != bound), "refs: {refs:?}");
        }
        // No strays: the two BASE reads.
        assert_eq!(refs.len(), 2, "unexpected rows: {refs:?}");
    }

    #[test]
    fn field_reads_emit_receivers_once() {
        let plugin = RustPlugin;
        let refs = plugin
            .extract_references(
                test_path(),
                "struct Cfg { port: u16 }\nfn get(cfg: Cfg) -> u16 {\n    cfg.port\n}\nfn chain(a: A) {\n    a.b.c();\n}\n",
            )
            .unwrap();

        let cfg: Vec<&Reference> = refs.iter().filter(|r| r.name == "cfg").collect();
        assert_eq!(cfg.len(), 1, "refs: {refs:?}");
        assert_eq!(cfg[0].kind, ReferenceKind::Value);
        // Chains read their base once; the callee keeps call rules.
        let a: Vec<&Reference> = refs.iter().filter(|r| r.name == "a").collect();
        assert_eq!(a.len(), 1, "refs: {refs:?}");
        assert!(refs.iter().any(|r| r.name == "c" && r.kind == ReferenceKind::Method));
        // Call chains read their middle (`b` in `a.b.c()`).
        let b: Vec<&Reference> = refs.iter().filter(|r| r.name == "b").collect();
        assert_eq!(b.len(), 1, "refs: {refs:?}");
        assert_eq!(b[0].kind, ReferenceKind::Value);
        assert_eq!(b[0].qualifier, "a");
    }

    #[test]
    fn qualified_path_calls_keep_qualifier() {
        let plugin = RustPlugin;
        let refs = plugin
            .extract_references(
                test_path(),
                "use crate::{api, mcp};\nfn run() {\n    api::serve();\n    crate::mcp::serve();\n    serve();\n}\n",
            )
            .unwrap();
        let paths: Vec<(&str, &str)> = refs
            .iter()
            .filter(|r| r.kind == ReferenceKind::Path)
            .map(|r| (r.name.as_str(), r.qualifier.as_str()))
            .collect();
        assert_eq!(
            paths,
            vec![("serve", "api"), ("serve", "crate::mcp")],
            "got {paths:?}"
        );
        // Bare calls stay unqualified.
        let bare = refs
            .iter()
            .find(|r| r.name == "serve" && r.kind == ReferenceKind::Call)
            .expect("bare serve call");
        assert_eq!(bare.start_line, 5);
        assert!(bare.qualifier.is_empty());
    }

    #[test]
    fn member_definitions_are_field_symbols() {
        let plugin = RustPlugin;
        let syms = plugin
            .extract_symbols(
                test_path(),
                "pub struct Config {\n    pub port: u16,\n    host: String,\n}\npub enum Role {\n    Admin = 1,\n    User,\n    Alias { name: String },\n}\npub struct Pair(u16, u16);\n",
            )
            .unwrap();
        let find = |n: &str| syms.iter().find(|s| s.name == n).cloned();
        let field = SymbolKind::Other("field".into());
        assert_eq!(find("port").unwrap().kind, field);
        assert_eq!(find("port").unwrap().start_line, 2);
        assert_eq!(find("host").unwrap().kind, field);
        assert_eq!(find("Admin").unwrap().kind, field);
        assert_eq!(find("User").unwrap().kind, field);
        assert_eq!(find("Alias").unwrap().kind, field);
        assert_eq!(find("name").unwrap().kind, field);
        assert_eq!(find("Config").unwrap().kind, SymbolKind::Struct);
        assert_eq!(find("Role").unwrap().kind, SymbolKind::Enum);
        assert_eq!(
            syms.iter().filter(|s| s.kind == field).count(),
            6,
            "syms: {syms:?}"
        );
    }

    #[test]
    fn member_reads_emit_fields_once_with_qualifier() {
        let plugin = RustPlugin;
        let src = "fn f(cfg: Config) -> u16 {\n    let p = cfg.port;\n    cfg.port = 1;\n    cfg.count += 1;\n    let q = cfg.a.b;\n    cfg.serve();\n    let t = cfg.pair.0;\n    let r = Role::Admin;\n    r\n}\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();
        let named = |n: &str| -> Vec<_> { refs.iter().filter(|r| r.name == n).collect() };

        // Reads carry the receiver text as qualifier.
        let port = named("port");
        assert_eq!(port.len(), 1, "refs: {refs:?}");
        assert_eq!(port[0].kind, ReferenceKind::Value);
        assert_eq!(port[0].qualifier, "cfg");
        assert_eq!(named("count").len(), 1);
        let a = named("a");
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].qualifier, "cfg");
        let b = named("b");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].qualifier, "cfg.a");
        let pair = named("pair");
        assert_eq!(pair.len(), 1);
        assert_eq!(pair[0].qualifier, "cfg");
        // Paths and calls keep their kinds with no Value echo.
        let admin = named("Admin");
        assert_eq!(admin.len(), 1);
        assert_eq!(admin[0].kind, ReferenceKind::Path);
        assert_eq!(admin[0].qualifier, "Role");
        assert_eq!(named("serve").len(), 1);
        assert_eq!(named("serve")[0].kind, ReferenceKind::Method);
        // Receivers still read at every site, including writes.
        assert_eq!(named("cfg").len(), 6, "refs: {refs:?}");
        assert!(named("cfg").iter().all(|r| r.kind == ReferenceKind::Value));
        // No strays: 6 receivers + port/count/a/b/pair/Admin/serve + r
        // + the Config annotation.
        assert_eq!(refs.len(), 15, "unexpected rows: {refs:?}");
    }

    #[test]
    fn value_positions_read_bare_identifiers_once() {
        let plugin = RustPlugin;
        let src = "use m::C;\n\
             const G: i32 = 1;\n\
             const H: i32 = 2;\n\
             struct Point {\n\
                 x: i32,\n\
             }\n\
             fn use_it() -> i32 {\n\
                 G\n\
             }\n\
             fn ret() -> i32 {\n\
                 return G;\n\
             }\n\
             fn sum() -> i32 {\n\
                 G + H\n\
             }\n\
             fn neg() -> i32 {\n\
                 -G\n\
             }\n\
             fn cond() -> i32 {\n\
                 if G > 0 {\n\
                     G\n\
                 } else {\n\
                     H\n\
                 }\n\
             }\n\
             fn wh() {\n\
                 let mut n = 0;\n\
                 while n < G {\n\
                     n += 1;\n\
                 }\n\
             }\n\
             fn lp() {\n\
                 for _i in 0..G {}\n\
                 for v in [G, H] {\n\
                     let _ = v;\n\
                 }\n\
             }\n\
             fn mtch(opt: i32) -> i32 {\n\
                 match opt {\n\
                     _ => G,\n\
                 }\n\
             }\n\
             fn lets() {\n\
                 let t = G;\n\
                 let u: i32 = H;\n\
                 let mut m = 0;\n\
                 m = G;\n\
                 m += H;\n\
                 m = m + G;\n\
             }\n\
             fn coll() {\n\
                 let a = [G, H];\n\
                 let t = (G, H);\n\
                 let s = &G;\n\
                 let r = G..H;\n\
                 let p = (G,);\n\
                 let b = G;\n\
                 let _ = b;\n\
                 G;\n\
             }\n\
             fn idx(vals: &[i32]) -> i32 {\n\
                 vals[G as usize] + G\n\
             }\n\
             fn strct() -> Point {\n\
                 Point { x: G }\n\
             }\n\
             fn clo() {\n\
                 let f = || G;\n\
                 let _ = f;\n\
             }\n\
             fn paths() -> i32 {\n\
                 m::C + crate::m::C\n\
             }\n\
             fn types(x: m::S) -> m::S {\n\
                 x\n\
             }\n\
             fn brk() -> i32 {\n\
                 loop {\n\
                     break G;\n\
                 }\n\
             }\n\
             fn tryit() -> Option<i32> {\n\
                 Some(G)\n\
             }\n\
             fn refit() -> i32 {\n\
                 *G\n\
             }\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();
        let named = |name: &str| -> Vec<&Reference> {
            refs.iter().filter(|r| r.name == name).collect()
        };

        // Block tail, return, binary, unary, if-cond, then-tail, while
        // operand, range bound, for-array element, match-arm tail, let
        // init, assign-RHS, assign-RHS binary operand, array, tuple,
        // borrow, range, paren-tuple, let init, bare statement, cast
        // value, binary operand, struct field value, closure body, break
        // value, call argument, dereference — each exactly once.
        let g = named("G");
        assert_eq!(g.len(), 27, "all G reads, no doubles: {g:?}");
        assert!(g.iter().all(|r| r.kind == ReferenceKind::Value));
        // Binary-RHS, else-tail, for-array element, let init,
        // compound-RHS, array, tuple, range.
        let h = named("H");
        assert_eq!(h.len(), 8, "all H reads: {h:?}");
        assert!(h.iter().all(|r| r.kind == ReferenceKind::Value));
        // Qualified path uses carry their qualifier.
        let c = named("C");
        assert_eq!(c.len(), 2);
        assert!(c.iter().all(|r| r.kind == ReferenceKind::Path));
        let quals: Vec<&str> = c.iter().map(|r| r.qualifier.as_str()).collect();
        assert!(quals.contains(&"m"), "quals: {quals:?}");
        assert!(quals.contains(&"crate::m"), "quals: {quals:?}");
        // Type-position paths read through their qualifier too.
        let s = named("S");
        assert_eq!(s.len(), 2);
        assert!(s.iter().all(|r| r.kind == ReferenceKind::Path));
        assert!(s.iter().all(|r| r.qualifier == "m"));
        // Bare type uses stay Type.
        let point = named("Point");
        assert_eq!(point.len(), 2, "return type + literal: {point:?}");
        assert!(point.iter().all(|r| r.kind == ReferenceKind::Type));
        let option = named("Option");
        assert_eq!(option.len(), 1);
        assert_eq!(option[0].kind, ReferenceKind::Type);
        // Primitive types (`usize`, `i32`) are not type references.
        assert!(named("usize").is_empty());
        // Single-occurrence reads and the one call.
        assert_eq!(named("vals").len(), 1);
        assert_eq!(named("opt").len(), 1);
        assert_eq!(named("n").len(), 2);
        assert_eq!(named("m").len(), 2);
        assert_eq!(named("f").len(), 1);
        // The `types` tail plus the struct-literal key.
        assert_eq!(named("x").len(), 2);
        assert_eq!(named("v").len(), 1);
        assert_eq!(named("b").len(), 1);
        let some = named("Some");
        assert_eq!(some.len(), 1);
        assert_eq!(some[0].kind, ReferenceKind::Call);
        // Writes, bindings, path segments/roots, imports, and `self`
        // never read.
        for bound in ["t", "u", "a", "s", "r", "p", "_i", "self", "crate"] {
            assert!(named(bound).is_empty(), "{bound} must not read");
        }
        // No strays: 27 + 8 + 2 + 2 + 2 + 1 + 1 + 1 + 1 + 2 + 2 + 1
        // + 1 + 1 + 1 + the struct-literal key.
        assert_eq!(refs.len(), 54, "unexpected rows: {refs:?}");
        // Container attribution survives: the return sits in `ret`.
        assert!(g.iter().any(|r| r.container == "crate::ret"));
    }

    #[test]
    fn macro_arguments_read_bare_identifiers() {
        let plugin = RustPlugin;
        let src = "#[derive(Debug, Clone)]\npub struct Item;\n\npub fn build(seed: i32) -> Vec<i32> {\n    let v = vec![seed, 1, 2];\n    let msg = format!(\"seed={seed}\");\n    assert_eq!(v.len(), 3);\n    v\n}\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();
        let named = |n: &str| -> Vec<_> { refs.iter().filter(|r| r.name == n).collect() };

        // Derive lists are attribute token trees: real macro uses.
        assert_eq!(named("Debug").len(), 1);
        assert_eq!(named("Clone").len(), 1);
        assert!(named("Debug").iter().all(|r| r.kind == ReferenceKind::Value));
        // Macro call arguments read: `seed` twice (the `vec!` argument
        // and the `format!` hole); other string contents stay opaque.
        assert_eq!(named("seed").len(), 2, "refs: {refs:?}");
        assert!(named("seed").iter().all(|r| r.kind == ReferenceKind::Value));
        assert_eq!(named("v").len(), 2);
        // Token soup is flat: `v.len()` in a macro is three tokens,
        // so the property reads as a value, not a method.
        assert_eq!(named("len").len(), 1);
        assert_eq!(named("len")[0].kind, ReferenceKind::Value);
        for mac in ["vec", "format", "assert_eq"] {
            assert_eq!(named(mac).len(), 1);
            assert_eq!(named(mac)[0].kind, ReferenceKind::Macro);
        }
        // No strays: Debug + Clone + Vec + seed*2 + v*2 + len +
        // vec + format + assert_eq.
        assert_eq!(refs.len(), 11, "unexpected rows: {refs:?}");
    }

    #[test]
    fn macro_definitions_do_not_read_their_templates() {
        let plugin = RustPlugin;
        let src = "macro_rules! wrap {\n    ($x:expr) => { helper($x) };\n}\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();
        assert!(refs.is_empty(), "unexpected rows: {refs:?}");
    }

    #[test]
    fn format_string_holes_read_as_values_with_exact_positions() {
        let plugin = RustPlugin;
        let src = "pub fn build(v: u32, w: usize, d: f64, p: usize) -> String {\n    format!(\"seed={seed} {v:>w$} {{esc}} {} {0:?} {d:.p$}\", seed, v)\n}\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();
        let named = |n: &str| -> Vec<_> { refs.iter().filter(|r| r.name == n).collect() };

        // Holes read with exact positions; escapes, positional holes,
        // and spec text stay out.
        for (name, line, col) in [
            ("seed", 2, 20),
            ("v", 2, 27),
            ("w", 2, 30),
            ("d", 2, 52),
            ("p", 2, 55),
        ] {
            let hits = named(name);
            assert!(
                hits.iter().any(|r| r.kind == ReferenceKind::Value
                    && r.start_line == line
                    && r.start_col == col),
                "{name}@({line},{col}): {refs:?}"
            );
        }
        assert!(named("esc").is_empty(), "escaped braces: {refs:?}");
        // The trailing `seed, v` arguments read as direct identifiers.
        assert_eq!(named("seed").len(), 2, "refs: {refs:?}");
        assert_eq!(named("v").len(), 2, "refs: {refs:?}");
        // No strays: String return type + format + 5 holes +
        // 2 trailing args.
        assert_eq!(refs.len(), 9, "unexpected rows: {refs:?}");
    }

    #[test]
    fn format_position_varies_by_macro() {
        let plugin = RustPlugin;
        let src = "pub fn go(buf: &mut String, v: u32) {\n    writeln!(buf, \"v={v}\").unwrap();\n    assert_eq!(v, 1, \"oops {v}\");\n}\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();
        let holes: Vec<(u32, u32)> = refs
            .iter()
            .filter(|r| r.name == "v" && r.kind == ReferenceKind::Value)
            .map(|r| (r.start_line, r.start_col))
            .collect();
        // `writeln!` scans its second argument, `assert_eq!` its third
        // (the first-argument `v` reads as a direct identifier too).
        assert!(holes.contains(&(2, 23)), "holes: {holes:?}");
        assert!(holes.contains(&(3, 16)), "holes: {holes:?}");
        assert!(holes.contains(&(3, 29)), "holes: {holes:?}");
    }

    #[test]
    fn compared_strings_and_plain_macros_stay_opaque() {
        let plugin = RustPlugin;
        let src = "pub fn go(a: i32, b: i32) {\n    assert_eq!(a, \"{b}\");\n    vec![\"{a}\"];\n    compile_error!(\"{a}\");\n}\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();
        let named = |n: &str| -> Vec<_> { refs.iter().filter(|r| r.name == n).collect() };
        // `a` reads once as a compared value; neither `{b}` (a compared
        // value, not a format string) nor plain-macro strings emit holes.
        assert_eq!(named("a").len(), 1, "refs: {refs:?}");
        assert!(named("b").is_empty(), "refs: {refs:?}");
        for mac in ["assert_eq", "vec", "compile_error"] {
            assert_eq!(named(mac).len(), 1, "refs: {refs:?}");
            assert_eq!(named(mac)[0].kind, ReferenceKind::Macro);
        }
    }

    #[test]
    fn format_holes_inside_macro_definitions_stay_out() {
        let plugin = RustPlugin;
        let src = "macro_rules! wrap {\n    () => { format!(\"{x}\") };\n}\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();
        // Templates parse as opaque token soup and declare, never use:
        // no Macro row, no hole reads.
        assert!(refs.is_empty(), "unexpected rows: {refs:?}");
    }

    #[test]
    fn format_holes_scanner_units() {
        let holes = |lit: &str| -> Vec<(String, usize)> { format_holes(lit) };
        assert_eq!(holes("\"x{a}y\""), vec![("a".to_string(), 3)]);
        assert_eq!(holes("\"{{a}}{b}\""), vec![("b".to_string(), 7)]);
        assert_eq!(
            holes("\"{v:>w$}\""),
            vec![("v".to_string(), 2), ("w".to_string(), 5)]
        );
        assert_eq!(holes("\"{:.p$}\""), vec![("p".to_string(), 4)]);
        assert_eq!(holes("\"{} {0} {x:?}\""), vec![("x".to_string(), 9)]);
        assert!(holes("\"no holes\"").is_empty());
        assert_eq!(holes("\"unterminated {x"), vec![("x".to_string(), 15)]);
        assert_eq!(holes("r#\"{raw}\"#"), vec![("raw".to_string(), 4)]);
    }
}
