//! Python language plugin: Tree-sitter based symbol and reference extraction.
//!
//! `module_path` is the source path with the extension stripped and separators
//! converted to dots (e.g. `pkg/auth/service.py` → `pkg.auth.service`).
//!
//! Handles `.py` and `.pyi`. Class bases are recorded as trait
//! implementations (`extract_impls` walks `class_definition` bases).

use super::{file_path_key, LanguagePlugin};
use crate::error::{Result, KeelError};
use crate::graph::types::{ImplRecord, Import, Reference, ReferenceKind, Symbol, SymbolKind};
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
    fn has_syntax_errors(&self, source_code: &str) -> bool {
        Self::parse(source_code)
            .map(|tree| tree.root_node().has_error())
            .unwrap_or(false)
    }
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

    fn extract_imports(&self, path: &Path, source_code: &str) -> Result<Vec<Import>> {
        let tree = Self::parse(source_code)?;
        let src = source_code.as_bytes();
        let mut out = Vec::new();
        walk_imports(tree.root_node(), src, &mut out)?;
        for imp in &mut out {
            imp.module_path = resolve_python_import_module(path, &imp.module_path);
        }
        Ok(out)
    }

    fn extract_impls(&self, _path: &Path, source_code: &str) -> Result<Vec<ImplRecord>> {
        let tree = Self::parse(source_code)?;
        let src = source_code.as_bytes();
        let mut out = Vec::new();
        walk_impls(tree.root_node(), src, &mut out)?;
        Ok(out)
    }
}

/// Collect (class, base) pairs from `class C(Base, ...)` lists.
///
/// Bare-identifier bases count, as do generic aliases by outer name
/// (`Base[int]` → `Base`, matching the TS/Rust rule). Keyword arguments
/// (`metaclass=M`) and dotted bases (`mod.Base`, including `mod.Base[T]`)
/// are skipped, matching the direct-only rule used for arguments elsewhere.
fn walk_impls(node: Node, src: &[u8], out: &mut Vec<ImplRecord>) -> Result<()> {
    if node.kind() == "class_definition" {
        if let Some(name) = node.child_by_field_name("name") {
            if name.kind() == "identifier" {
                let type_name = node_text(name, src)?.to_string();
                if let Some(bases) = node.child_by_field_name("superclasses") {
                    let mut cursor = bases.walk();
                    for base in bases.children(&mut cursor) {
                        if let Some(base_name) = impl_base_name(base, src)? {
                            let pos = node.start_position();
                            out.push(ImplRecord {
                                type_name: type_name.clone(),
                                trait_name: Some(base_name),
                                file: PathBuf::new(),
                                start_line: pos.row as u32 + 1,
                                start_col: pos.column as u32 + 1,
                            });
                        }
                    }
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_impls(child, src, out)?;
    }
    Ok(())
}

/// Outer name of a class base: a bare identifier, or the value of a
/// subscript over one (`Base[int]` → `Base`). Anything else (dotted
/// names, keywords) stays out.
fn impl_base_name(node: Node, src: &[u8]) -> Result<Option<String>> {
    match node.kind() {
        "identifier" => Ok(Some(node_text(node, src)?.to_string())),
        "subscript" => match node.child_by_field_name("value") {
            Some(v) if v.kind() == "identifier" => {
                Ok(Some(node_text(v, src)?.to_string()))
            }
            _ => Ok(None),
        },
        _ => Ok(None),
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
        "assignment" if module_scope => {
            emit_module_variable(node, module_path, out, src)?;
        }
        // Class attributes are `field` symbols: plain bindings directly
        // under a class body (covering dataclass fields and enum
        // members) and `self`/`cls` attribute writes in methods.
        "assignment" if !module_scope => {
            emit_field_symbol(node, module_path, out, src)?;
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

fn emit_module_variable(
    node: Node,
    module_path: &str,
    out: &mut Vec<Symbol>,
    src: &[u8],
) -> Result<()> {
    let Some(left) = node.child_by_field_name("left") else {
        return Ok(());
    };
    // Only simple identifiers (destructured names are not simple
    // bindings, mirroring TS/JS); annotations ride the `type` field
    // and chained targets recurse as nested assignments. Augmented
    // assignment reads its target and never defines.
    if left.kind() != "identifier" {
        return Ok(());
    }
    let name = node_text(left, src)?;
    let kind = if is_constant_like(name) {
        SymbolKind::Const
    } else {
        SymbolKind::Other("variable".into())
    };
    push_symbol(left, kind, module_path, out, src)
}

fn is_constant_like(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
        && name.chars().any(|c| c.is_ascii_alphabetic())
}

/// Emit class attributes as `field` symbols: a plain binding directly
/// under a class body (`x = 1`), or a `self`/`cls` attribute write
/// anywhere (`self.x = …` in methods, the idiomatic instance
/// attributes). Writes to any other receiver (`config.debug = True`)
/// are not definitions.
fn emit_field_symbol(
    node: Node,
    module_path: &str,
    out: &mut Vec<Symbol>,
    src: &[u8],
) -> Result<()> {
    let Some(left) = node.child_by_field_name("left") else {
        return Ok(());
    };
    if left.kind() == "identifier" {
        if is_direct_class_body_statement(node) {
            push_symbol(left, SymbolKind::Other("field".into()), module_path, out, src)?;
        }
        return Ok(());
    }
    if left.kind() != "attribute" {
        return Ok(());
    }
    let is_self = left
        .child_by_field_name("object")
        .and_then(|o| {
            if o.kind() == "identifier" {
                node_text(o, src).ok()
            } else {
                None
            }
        })
        .is_some_and(|t| matches!(t, "self" | "cls"));
    if !is_self {
        return Ok(());
    }
    if let Some(attr) = left.child_by_field_name("attribute") {
        if attr.kind() == "identifier" {
            push_symbol(attr, SymbolKind::Other("field".into()), module_path, out, src)?;
        }
    }
    Ok(())
}

/// True when `node` is a statement directly under a class body block
/// (assignments wrap one `expression_statement` level).
fn is_direct_class_body_statement(node: Node) -> bool {
    node.parent().is_some_and(|p| {
        let block = if p.kind() == "expression_statement" {
            p.parent()
        } else {
            Some(p)
        };
        block.is_some_and(|b| {
            b.kind() == "block" && b.parent().is_some_and(|g| g.kind() == "class_definition")
        })
    })
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
        // Attribute reads (`cfg.port`) read the receiver, mirroring
        // the receiver rule for calls, and the attribute itself.
        // Attributes owned by another arm (call callees, decorator
        // applications, annotation and handler type subtrees) are
        // skipped so no site emits twice; attributes in write
        // positions (plain-assignment LHS, loop targets, `with`/`as`
        // targets, `del` targets, dict keys) are writes, never reads.
        "attribute" => {
            emit_attribute_receiver(node, src, file_key, module_path, scope, out)?;
            emit_attribute_property(node, src, file_key, module_path, scope, out)?;
        }
        // Match value patterns (`case Color.RED`, `case Point(x=0)`) read
        // the dotted name. A bare name in pattern position is always a
        // capture binding, so single-identifier `dotted_name` reads only
        // in class-pattern position; import statements reuse the node for
        // module paths and are excluded.
        "dotted_name" if inside_case_pattern(node) => {
            emit_pattern_name(node, src, file_key, module_path, scope, out)?;
        }
        // F-string interpolations (`f"hi {name}"`) read directly embedded
        // identifiers; richer expressions keep the normal rules via the
        // recursion below.
        "interpolation" => {
            if let Some(expr) = node.child_by_field_name("expression") {
                if expr.kind() == "identifier" {
                    push_reference(
                        node_text(expr, src)?.to_string(),
                        expr,
                        ReferenceKind::Value,
                        file_key,
                        module_path,
                        scope,
                        out,
                    );
                }
            }
        }
        // Decorators apply the named decorator (`@auth`, `@app.route`), so
        // the decorator name is a reference attributed to the decorated
        // definition. Children are walked under the decorated name so
        // call-form decorators (`@app.route("/x")`) attribute there too.
        "decorator" => {
            if emit_decorator_reference(node, src, file_key, module_path, scope, out)? {
                return Ok(());
            }
        }
        // Bare identifiers in value positions: call/decorator/class-base
        // arguments share the `argument_list` shape. Keyword values use the
        // `keyword_argument` arm below; keyword names are labels, never
        // references.
        "argument_list" => {
            emit_argument_values(node, src, file_key, module_path, scope, out)?;
        }
        // Value reads in statement/expression positions (`return x`,
        // `x + y`, `-x`, `a < b`, `a and b`, `not x`, `[x]`, `(x,)`,
        // `{x}`, `a if c else b`, `lambda: x`, `a[b:c]`, `yield x`,
        // `await x`, `raise E`, `assert x`, bare `x`, `*a`, `**d`, `(x)`,
        // comprehension bodies and `if` filters, `print x`): only DIRECT
        // identifier children emit here; nested composites fall through
        // to the recursion below so each site emits exactly once.
        "return_statement" | "binary_operator" | "unary_operator"
        | "comparison_operator" | "boolean_operator" | "not_operator"
        | "list" | "set" | "conditional_expression" | "lambda"
        | "slice" | "yield" | "await" | "raise_statement"
        | "assert_statement" | "expression_statement" | "list_splat"
        | "dictionary_splat" | "parenthesized_expression" | "if_clause"
        | "print_statement" | "list_comprehension" | "set_comprehension"
        | "dictionary_comprehension" | "generator_expression" => {
            emit_direct_identifiers(node, src, file_key, module_path, scope, out)?;
        }
        // Tuples read (`(a, b)`) — except inside `except (A, B):`,
        // where the except arm owns the handler types as Type uses.
        "tuple" if node.parent().is_none_or(|p| p.kind() != "except_clause") => {
            emit_direct_identifiers(node, src, file_key, module_path, scope, out)?;
        }
        // Assignment reads only the RHS (`x = v`, `x: T = v`); the LHS
        // is a write, never a read. Compound assignment (`x += v`) reads
        // its target too.
        "assignment" => {
            emit_field_identifier(node, "right", src, file_key, module_path, scope, out)?;
        }
        "augmented_assignment" => {
            emit_field_identifier(node, "left", src, file_key, module_path, scope, out)?;
            emit_field_identifier(node, "right", src, file_key, module_path, scope, out)?;
        }
        // Computed member access reads both sides (`a[b]`).
        "subscript" => {
            emit_field_identifier(node, "value", src, file_key, module_path, scope, out)?;
            emit_field_identifier(node, "subscript", src, file_key, module_path, scope, out)?;
        }
        // `for x in ITER` (and comprehension `for` clauses) read the
        // iterable; the loop target is a binding/write.
        "for_statement" | "for_in_clause" => {
            emit_field_identifier(node, "right", src, file_key, module_path, scope, out)?;
        }
        // Conditions read (`if x:`, `while x:`).
        "if_statement" | "while_statement" => {
            emit_field_identifier(node, "condition", src, file_key, module_path, scope, out)?;
        }
        // Dict values (`{k: v}`); keys are labels, never reads.
        // (Binding patterns use distinct `*_pattern` nodes.)
        "pair" => {
            emit_field_identifier(node, "value", src, file_key, module_path, scope, out)?;
        }
        // Walrus reads its value (`if (x := f())`); the name binds.
        "named_expression" => {
            emit_field_identifier(node, "value", src, file_key, module_path, scope, out)?;
        }
        // `with` items read (`with lock:`, `with open(f) as h:`); the
        // `as` alias binds. The aliased form wraps the value in
        // `as_pattern`, so the alias is excluded by id (match-case
        // `as_pattern` nodes are untouched — this arm only unwraps
        // `with_item` values).
        "with_item" => {
            if let Some(value) = node.child_by_field_name("value") {
                if value.kind() == "identifier" {
                    push_reference(
                        node_text(value, src)?.to_string(),
                        value,
                        ReferenceKind::Value,
                        file_key,
                        module_path,
                        scope,
                        out,
                    );
                } else if value.kind() == "as_pattern" {
                    let alias = value.child_by_field_name("alias").map(|n| n.id());
                    let mut cursor = value.walk();
                    for child in value.children(&mut cursor) {
                        if child.is_named()
                            && child.kind() == "identifier"
                            && Some(child.id()) != alias
                        {
                            push_reference(
                                node_text(child, src)?.to_string(),
                                child,
                                ReferenceKind::Value,
                                file_key,
                                module_path,
                                scope,
                                out,
                            );
                        }
                    }
                }
            }
        }
        // Keyword-argument values (`f(k=v)`); names are labels.
        "keyword_argument" => {
            emit_field_identifier(node, "value", src, file_key, module_path, scope, out)?;
        }
        // Parameter defaults read (`def f(x=DEFAULT)`); the name binds.
        "default_parameter" | "typed_default_parameter" => {
            emit_field_identifier(node, "value", src, file_key, module_path, scope, out)?;
        }
        // `match SUBJECT:` reads the subject.
        "match_statement" => {
            emit_field_identifier(node, "subject", src, file_key, module_path, scope, out)?;
        }
        // Type annotations (`x: T`, `-> R`). Nested `type` nodes handle their
        // own level; `call` subtrees (e.g. `Annotated` metadata) fall through
        // to the normal call/argument rules via the recursion below.
        "type" if !is_alias_target(node) => {
            emit_type_identifiers(node, src, file_key, module_path, scope, out)?;
        }
        // Except handlers name the caught exception type(s)
        // (`except AppError`, `except (A, B)`). The `as` alias is a fresh
        // local binding, not a reference, so only `value` is emitted.
        "except_clause" => {
            let alias = node.child_by_field_name("alias").map(|n| n.id());
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.is_named() && child.kind() != "block" && Some(child.id()) != alias {
                    emit_except_type(child, src, file_key, module_path, scope, out)?;
                }
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

/// Emit identifiers in an `except` handler value as Type references,
/// mirroring annotation rules (tuples recurse; `as` aliases arrive
/// wrapped in `as_pattern` and their `as_pattern_target` is skipped).
fn emit_except_type(
    root: Node,
    src: &[u8],
    file_key: &str,
    module_path: &str,
    scope: &[String],
    out: &mut Vec<Reference>,
) -> Result<()> {
    if root.kind() == "call" {
        // `except factory()`: the `call` arm owns it via normal recursion.
        return Ok(());
    }
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        // `except E as e` parses the handler as an `as_pattern` whose
        // alias is an `as_pattern_target`: the alias binds a fresh local,
        // it is never a caught type.
        if node.kind() == "as_pattern_target" {
            continue;
        }
        if node.id() != root.id() && node.kind() == "call" {
            continue;
        }
        if node.kind() == "identifier" {
            push_reference(
                node_text(node, src)?.to_string(),
                node,
                ReferenceKind::Type,
                file_key,
                module_path,
                scope,
                out,
            );
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    Ok(())
}

/// True when `node` is the defined name of a `type X = …` alias statement.
/// The alias target is a definition, not a reference to itself.
fn is_alias_target(node: Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if parent.kind() != "type_alias_statement" {
        return false;
    }
    parent
        .child_by_field_name("left")
        .is_some_and(|left| left.id() == node.id())
}

/// Emit identifiers directly under an `argument_list`: positional values and
/// keyword-argument values. Keyword names and nested structures are left to
/// the normal recursion (nested calls) or skipped (labels); receivers are
/// captured with the callee.
/// Emit direct identifier arguments as Value references. Nested
/// structures fall through to the normal recursion (nested calls, keyword
/// values via the `keyword_argument` arm); receivers are captured with
/// the callee.
fn emit_argument_values(
    args: Node,
    src: &[u8],
    file_key: &str,
    module_path: &str,
    scope: &[String],
    out: &mut Vec<Reference>,
) -> Result<()> {
    emit_direct_identifiers(args, src, file_key, module_path, scope, out)
}

/// Emit DIRECT identifier children as Value references. Nested
/// composites are left to the normal recursion so each site emits exactly
/// once (no double emits: every identifier has exactly one parent arm).
fn emit_direct_identifiers(
    node: Node,
    src: &[u8],
    file_key: &str,
    module_path: &str,
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
                module_path,
                scope,
                out,
            );
        }
    }
    Ok(())
}

/// Emit the `field` child as a Value reference when it is a bare
/// identifier (composite values fall through to the normal recursion).
fn emit_field_identifier(
    node: Node,
    field: &str,
    src: &[u8],
    file_key: &str,
    module_path: &str,
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
                module_path,
                scope,
                out,
            );
        }
    }
    Ok(())
}

/// Emit identifiers in a `type` subtree as Type references, skipping nested
/// `type` nodes (their own arm level handles them) and `call` subtrees
/// (handled by the call/argument rules through normal recursion).
fn emit_type_identifiers(
    root: Node,
    src: &[u8],
    file_key: &str,
    module_path: &str,
    scope: &[String],
    out: &mut Vec<Reference>,
) -> Result<()> {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.id() != root.id() && matches!(node.kind(), "type" | "call") {
            continue;
        }
        if node.kind() == "identifier" {
            push_reference(
                node_text(node, src)?.to_string(),
                node,
                ReferenceKind::Type,
                file_key,
                module_path,
                scope,
                out,
            );
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    Ok(())
}

/// Emit the decorator name as a reference scoped to the decorated
/// definition (`module::login` for `@auth` on `login`), so impact analysis
/// attributes decorator applications to the definition they wrap. Returns
/// true when the decorator was fully handled (children walked under the
/// decorated name); false falls back to the normal recursion.
fn emit_decorator_reference(
    decorator: Node,
    src: &[u8],
    file_key: &str,
    module_path: &str,
    scope: &mut Vec<String>,
    out: &mut Vec<Reference>,
) -> Result<bool> {
    let mut cursor = decorator.walk();
    let Some(expr) = decorator.children(&mut cursor).find(|c| c.is_named()) else {
        return Ok(false);
    };
    let decorated = decorator
        .parent()
        .filter(|p| p.kind() == "decorated_definition")
        .and_then(|p| p.child_by_field_name("definition"))
        .and_then(|d| d.child_by_field_name("name"))
        .filter(|n| n.kind() == "identifier");
    let Some(decorated) = decorated else {
        return Ok(false);
    };
    let name = node_text(decorated, src)?.to_string();
    scope.push(name);
    if expr.kind() == "call" {
        // `@app.route("/x")`: walk the call under the decorated name so
        // the `call`/`argument_list` arms attribute it there.
        let mut inner = decorator.walk();
        for child in decorator.children(&mut inner) {
            walk_references(child, src, file_key, module_path, scope, out)?;
        }
    } else {
        emit_call_reference(expr, src, file_key, module_path, scope, out)?;
    }
    scope.pop();
    Ok(true)
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
                    let qualifier = func
                        .child_by_field_name("object")
                        .map(|o| node_text(o, src))
                        .transpose()?
                        .unwrap_or("")
                        .to_string();
                    push_method_reference(
                        text.to_string(),
                        attr,
                        qualifier,
                        file_key,
                        module_path,
                        scope,
                        out,
                    );
                }
            }
            // The receiver (`db` in `db.get()`) is a value use, unless it
            // is the conventional method receiver (`self`/`cls`), which
            // would drown real reads in noise.
            if let Some(recv) = func.child_by_field_name("object") {
                if recv.kind() == "identifier" {
                    let text = node_text(recv, src)?;
                    if !matches!(text, "self" | "cls") {
                        push_reference(
                            text.to_string(),
                            recv,
                            ReferenceKind::Value,
                            file_key,
                            module_path,
                            scope,
                            out,
                        );
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Emit the receiver of a value-position attribute read (`items` in
/// `items.length`). Call callees, decorator applications, and annotation
/// / handler type subtrees are owned by other arms and skipped.
fn emit_attribute_receiver(
    node: Node,
    src: &[u8],
    file_key: &str,
    module_path: &str,
    scope: &[String],
    out: &mut Vec<Reference>,
) -> Result<()> {
    let Some(recv) = node.child_by_field_name("object") else {
        return Ok(());
    };
    if recv.kind() != "identifier" {
        return Ok(());
    }
    let text = node_text(recv, src)?;
    if matches!(text, "self" | "cls") {
        return Ok(());
    }
    // Call callees are owned by the call arm; the check is direct so
    // chains still read their base (`a.b.c()` reads `a` via the inner
    // attribute). Annotation and handler type subtrees own every
    // attribute beneath them.
    if let Some(parent) = node.parent() {
        if parent.kind() == "call" {
            return Ok(());
        }
    }
    if inside_except_value(node) || inside_annotation(node) {
        return Ok(());
    }
    push_reference(
        text.to_string(),
        recv,
        ReferenceKind::Value,
        file_key,
        module_path,
        scope,
        out,
    );
    Ok(())
}

/// Emit the attribute of a value-position attribute read (`port` in
/// `cfg.port`) as a Value reference, keeping the receiver text as the
/// qualifier exactly like method calls. Call callees, decorator
/// applications, and annotation/handler-type subtrees are owned by
/// other arms; attributes in write positions (plain-assignment LHS,
/// loop targets, `with`/`as` targets, `del` targets, dict keys) are
/// writes. Augmented targets read, so they emit.
fn emit_attribute_property(
    node: Node,
    src: &[u8],
    file_key: &str,
    module_path: &str,
    scope: &[String],
    out: &mut Vec<Reference>,
) -> Result<()> {
    let Some(attr) = node.child_by_field_name("attribute") else {
        return Ok(());
    };
    if attr.kind() != "identifier" {
        return Ok(());
    }
    let Some(object) = node.child_by_field_name("object") else {
        return Ok(());
    };
    // Call callees are owned by the call arm; the check is direct so
    // chains still read their middle (`a.b.c()` reads `b` via the
    // inner attribute). (Parenthesized calls still read: the call arm
    // owns only direct member callees.)
    if node.parent().is_some_and(|p| p.kind() == "call") {
        return Ok(());
    }
    // Plain-assignment LHS, loop targets, `with`/`as` targets, and dict
    // keys write the attribute (or label it); tuple/list/paren targets
    // nest, so the check climbs through transparent wrappers first.
    // Augmented targets read, so they emit.
    let target = climb_target_wrappers(node);
    if let Some(parent) = target.parent() {
        if matches!(
            parent.kind(),
            "assignment" | "for_statement" | "for_in_clause"
        ) && parent
            .child_by_field_name("left")
            .is_some_and(|left| is_within(target, left))
        {
            return Ok(());
        }
        if parent.kind() == "as_pattern"
            && parent
                .child_by_field_name("alias")
                .is_some_and(|alias| is_within(target, alias))
        {
            return Ok(());
        }
        // Dict keys stay labels, matching the bare-identifier rule.
        if parent.kind() == "pair"
            && parent
                .child_by_field_name("key")
                .is_some_and(|key| is_within(target, key))
        {
            return Ok(());
        }
    }
    if is_delete_target(node) {
        return Ok(());
    }
    if inside_except_value(node) || inside_annotation(node) {
        return Ok(());
    }
    let qualifier = node_text(object, src)?.to_string();
    push_value_reference(
        node_text(attr, src)?.to_string(),
        attr,
        qualifier,
        file_key,
        module_path,
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

/// True when `node` is itself a `del` target (`del a.b`, `del a.b, c`).
/// Attributes nested under a subscript (`del a[b.c]`) are index reads
/// and still emit.
fn is_delete_target(node: Node) -> bool {
    let mut current = node;
    while let Some(parent) = current.parent() {
        match parent.kind() {
            "delete_statement" => return true,
            "tuple" | "expression_list" | "pattern_list" | "tuple_pattern" | "list_pattern"
            | "list" | "parenthesized_expression" => current = parent,
            _ => return false,
        }
    }
    false
}

/// Climb from `node` through transparent target wrappers (tuple, list,
/// pattern, paren, and `as`-alias forms) to the enclosing target
/// itself, so `(a, self.x) = v` and `with f() as self.x` detect the
/// write position. Value wrappers climb harmlessly: the landing slot
/// check decides.
fn climb_target_wrappers(mut node: Node) -> Node {
    while let Some(parent) = node.parent() {
        if matches!(
            parent.kind(),
            "tuple"
                | "expression_list"
                | "pattern_list"
                | "tuple_pattern"
                | "list_pattern"
                | "list"
                | "list_splat"
                | "parenthesized_expression"
                | "as_pattern_target"
        ) {
            node = parent;
        } else {
            break;
        }
    }
    node
}

/// True when `node` sits in an `except` handler's caught-type list. The
/// handler body is a `block`, which cannot appear in the header, so the
/// first of the two ancestors decides.
fn inside_except_value(mut node: Node) -> bool {
    while let Some(parent) = node.parent() {
        if parent.kind() == "block" {
            return false;
        }
        if parent.kind() == "except_clause" {
            return true;
        }
        node = parent;
    }
    false
}

/// Emit a pattern `dotted_name` as value reads when it is a real read:
/// multi-identifier names (`Color.RED`) always, single names (`Point`)
/// only in class-pattern position (`case Point(x=0)`).
fn emit_pattern_name(
    node: Node,
    src: &[u8],
    file_key: &str,
    module_path: &str,
    scope: &[String],
    out: &mut Vec<Reference>,
) -> Result<()> {
    let mut cursor = node.walk();
    let mut names = Vec::new();
    for child in node.children(&mut cursor) {
        if child.kind() == "identifier" {
            names.push(child);
        }
    }
    if names.len() < 2
        && !node.parent().is_some_and(|p| p.kind() == "class_pattern")
    {
        return Ok(());
    }
    for name in names {
        push_reference(
            node_text(name, src)?.to_string(),
            name,
            ReferenceKind::Value,
            file_key,
            module_path,
            scope,
            out,
        );
    }
    Ok(())
}

/// True when `node` sits in a `match` case pattern. Patterns never
/// cross definition boundaries, so the first interesting ancestor
/// decides; import statements reuse `dotted_name` for module paths.
fn inside_case_pattern(mut node: Node) -> bool {
    while let Some(parent) = node.parent() {
        match parent.kind() {
            "case_clause" => return true,
            "import_statement" | "import_from_statement" | "function_definition"
            | "class_definition" | "module" => return false,
            _ => node = parent,
        }
    }
    false
}

/// True when `node` sits under a `type` annotation subtree, whose
/// identifiers the `type` arm already emits.
fn inside_annotation(mut node: Node) -> bool {
    while let Some(parent) = node.parent() {
        if parent.kind() == "type" {
            return true;
        }
        node = parent;
    }
    false
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
        qualifier: String::new(),
    });
}

/// Push a value reference with an explicit qualifier (the receiver text
/// for attribute reads, `cfg` in `cfg.port`).
fn push_value_reference(
    name: String,
    node: Node,
    qualifier: String,
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
        kind: ReferenceKind::Value,
        container: qualify_scope(file_key, module_path, scope),
        qualifier,
    });
}

/// Push a method-call reference, keeping the receiver text (`db` in
/// `db.get()`) so the resolver can break module ties for module-qualified
/// calls (`mod.func()`).
fn push_method_reference(
    name: String,
    node: Node,
    qualifier: String,
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
        kind: ReferenceKind::Method,
        container: qualify_scope(file_key, module_path, scope),
        qualifier,
    });
}

fn walk_imports(node: Node, src: &[u8], out: &mut Vec<Import>) -> Result<()> {
    match node.kind() {
        "import_statement" => emit_import_statement(node, src, out)?,
        "import_from_statement" => emit_import_from(node, src, out)?,
        "future_import_statement" => {}
        "assignment" => {
            emit_loader_import(node, src, out)?;
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                walk_imports(child, src, out)?;
            }
        }
        "call" => {
            emit_bare_loader_import(node, src, out)?;
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                walk_imports(child, src, out)?;
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                walk_imports(child, src, out)?;
            }
        }
    }
    Ok(())
}

/// `mod = importlib.import_module("pkg.m")` / `__import__("pkg.m")`:
/// a dynamic loader edge aliased to the assigned name.
fn emit_loader_import(assign: Node, src: &[u8], out: &mut Vec<Import>) -> Result<()> {
    let (Some(left), Some(right)) = (
        assign.child_by_field_name("left"),
        assign.child_by_field_name("right"),
    ) else {
        return Ok(());
    };
    if left.kind() != "identifier" || !is_loader_call(right, src)? {
        return Ok(());
    }
    let Some(module) = loader_module(right, src)? else {
        return Ok(());
    };
    out.push(Import {
        module_path: module,
        alias: Some(node_text(left, src)?.to_string()),
        file: PathBuf::new(),
    });
    Ok(())
}

/// Bare loader calls (statement or nested-expression position): a
/// binding-free module edge. Simply-assigned values are skipped — the
/// assignment arm owns those with their alias.
fn emit_bare_loader_import(call: Node, src: &[u8], out: &mut Vec<Import>) -> Result<()> {
    if !is_loader_call(call, src)? || is_assigned_value(call) {
        return Ok(());
    }
    let Some(module) = loader_module(call, src)? else {
        return Ok(());
    };
    out.push(Import {
        module_path: module,
        alias: None,
        file: PathBuf::new(),
    });
    Ok(())
}

/// `importlib.import_module(…)` or the `__import__(…)` builtin.
fn is_loader_call(call: Node, src: &[u8]) -> Result<bool> {
    if call.kind() != "call" {
        return Ok(false);
    }
    let Some(func) = call.child_by_field_name("function") else {
        return Ok(false);
    };
    if func.kind() == "identifier" {
        return Ok(node_text(func, src)? == "__import__");
    }
    if func.kind() == "attribute" {
        let object = func
            .child_by_field_name("object")
            .map(|o| node_text(o, src))
            .transpose()?
            .unwrap_or("");
        let attr = func
            .child_by_field_name("attribute")
            .map(|a| node_text(a, src))
            .transpose()?
            .unwrap_or("");
        return Ok(object == "importlib" && attr == "import_module");
    }
    Ok(false)
}

/// True when `call` is the direct `right` side of a simple-identifier
/// assignment.
fn is_assigned_value(call: Node) -> bool {
    call.parent().is_some_and(|p| {
        p.kind() == "assignment"
            && p.child_by_field_name("right").is_some_and(|r| r == call)
            && p
                .child_by_field_name("left")
                .is_some_and(|l| l.kind() == "identifier")
    })
}

/// First-argument module of a loader call: plain strings only —
/// interpolated, empty, and non-module-shaped targets stay out.
fn loader_module(call: Node, src: &[u8]) -> Result<Option<String>> {
    let mut cursor = call.walk();
    let args = call
        .children(&mut cursor)
        .find(|c| c.kind() == "argument_list");
    let Some(args) = args else {
        return Ok(None);
    };
    let mut cursor = args.walk();
    let first = args.named_children(&mut cursor).next();
    let Some(arg) = first.filter(|a| a.kind() == "string") else {
        return Ok(None);
    };
    let mut content = String::new();
    let mut cursor = arg.walk();
    for child in arg.named_children(&mut cursor) {
        match child.kind() {
            "string_content" => content.push_str(node_text(child, src)?),
            "string_start" | "string_end" => {}
            _ => return Ok(None),
        }
    }
    if content.is_empty()
        || !content
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
    {
        return Ok(None);
    }
    Ok(Some(content))
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

/// Resolve a stored Python import module (possibly relative, possibly with
/// `::name` suffix) against the importing file's package identity.
fn resolve_python_import_module(from_file: &Path, stored: &str) -> String {
    let (mod_part, suffix) = match stored.split_once("::") {
        Some((m, rest)) => (m, Some(rest)),
        None => (stored, None),
    };
    let resolved = if mod_part.starts_with('.') {
        resolve_python_relative(from_file, mod_part)
    } else {
        mod_part.to_string()
    };
    match suffix {
        Some(rest) => format!("{resolved}::{rest}"),
        None => resolved,
    }
}

/// Expand leading-dot relative modules (PEP 328) against the importing package.
fn resolve_python_relative(from_file: &Path, module: &str) -> String {
    let package = python_module_identity(from_file);
    let mut parts: Vec<&str> = package.split('.').filter(|s| !s.is_empty()).collect();
    if !parts.is_empty() {
        parts.pop();
    }
    let dots = module.chars().take_while(|c| *c == '.').count();
    let levels_up = dots.saturating_sub(1);
    for _ in 0..levels_up {
        parts.pop();
    }
    let rest = &module[dots..];
    if rest.is_empty() {
        parts.join(".")
    } else if parts.is_empty() {
        rest.to_string()
    } else {
        format!("{}.{}", parts.join("."), rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_annotation_and_argument_value_references() {
        let plugin = PythonPlugin;
        let src = "def f(x: MyType) -> Ret:\n    pass\n\
             y: MyType = make()\n\
             run(callback_fn, key=value)\n\
             asyncio.to_thread(check, url)\n\
             class C(Base, metaclass=M):\n    pass\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();
        let has = |name: &str, kind: ReferenceKind| {
            refs.iter().any(|r| r.name == name && r.kind == kind)
        };

        // Annotations (param, return, variable) are Type references.
        assert!(has("MyType", ReferenceKind::Type));
        assert!(has("Ret", ReferenceKind::Type));
        // Call + keyword values are Value references; keyword names are not.
        assert!(has("callback_fn", ReferenceKind::Value));
        assert!(has("value", ReferenceKind::Value));
        assert!(!refs.iter().any(|r| r.name == "key"));
        // Attribute calls keep the method and gain the argument values.
        assert!(has("to_thread", ReferenceKind::Method));
        assert!(has("check", ReferenceKind::Value));
        assert!(has("url", ReferenceKind::Value));
        // Class bases share the argument shape.
        assert!(has("Base", ReferenceKind::Value));
        assert!(has("M", ReferenceKind::Value));
        // Binding sites are definitions, never references.
        assert!(!refs.iter().any(|r| r.name == "x"));
        assert!(!refs.iter().any(|r| r.name == "y"));
        // Receivers are value uses, even module ones: `references asyncio`
        // finding `asyncio.to_thread(...)` is recall, and imports still
        // cover the module dependency edge (modules are not symbols).
        assert!(has("asyncio", ReferenceKind::Value));
    }

    #[test]
    fn extract_impls_records_bare_bases_only() {
        let plugin = PythonPlugin;
        let src = "class C(Base, Mixin):\n    pass\nclass D(mod.Base):\n    pass\nclass E(Base, metaclass=M):\n    pass\n";
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
        assert_eq!(pairs, vec![("C", "Base"), ("C", "Mixin"), ("E", "Base")]);
    }

    #[test]
    fn extract_impls_records_generic_aliases_by_outer_name() {
        let plugin = PythonPlugin;
        let src = "class R(Base[int]):\n    pass\nclass G(Generic[T]):\n    pass\nclass D(mod.Base[int]):\n    pass\n";
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
        // Subscripts over bare names record the outer name; dotted
        // values stay out, matching the bare-bases rule.
        assert_eq!(pairs, vec![("R", "Base"), ("G", "Generic")]);
    }

    #[test]
    fn alias_target_is_not_a_self_reference() {
        let plugin = PythonPlugin;
        let refs = plugin
            .extract_references(test_path(), "type X = list[int]\n")
            .unwrap();
        assert!(!refs.iter().any(|r| r.name == "X"));
        assert!(refs.iter().any(|r| r.name == "list"));
        assert!(refs.iter().any(|r| r.name == "int"));
    }

    const SOURCE: &str = r#"
import os
import sys as system
from .util import helper as h
from pkg.auth import AuthService
from typing import *

MAX_RETRIES = 3
module_var = 1

class AuthService:
    def login(self):
        h()
        self.refresh()
        AuthService()

    def refresh(self):
        local_var = 1

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
        assert_eq!(
            find("module_var").unwrap().kind,
            SymbolKind::Other("variable".into())
        );
        assert!(find("local_var").is_none());
    }

    #[test]
    fn extracts_module_variables_with_const_and_variable_kinds() {
        let plugin = PythonPlugin;
        let syms = plugin
            .extract_symbols(
                test_path(),
                "MAX = 3\nplain = 1\nname: str = \"x\"\nfirst = second = 2\npair_a, pair_b = 1, 2\nTOTAL += 1\n__version__ = \"1.0\"\n",
            )
            .unwrap();
        let find = |n: &str| syms.iter().find(|s| s.name == n).cloned();

        assert_eq!(find("MAX").unwrap().kind, SymbolKind::Const);
        for v in ["plain", "name", "first", "second", "__version__"] {
            assert_eq!(
                find(v).unwrap().kind,
                SymbolKind::Other("variable".into()),
                "{v}"
            );
        }
        // Destructured names are not simple bindings; augmented
        // assignment reads its target and never defines.
        assert!(find("pair_a").is_none());
        assert!(find("pair_b").is_none());
        assert!(find("TOTAL").is_none());
        assert_eq!(syms.len(), 6, "syms: {syms:?}");
    }

    #[test]
    fn value_positions_read_bare_identifiers_once() {
        let plugin = PythonPlugin;
        let refs = plugin
            .extract_references(
                test_path(),
                "G = 1\nH = 2\ndef use():\n    return G\nt = G\ns = G + H\nn = -G\nc = G < H\nb = G and H\nu = not G\nl = [G]\ntp = (G, H)\nst = {G}\nd = {\"k\": G}\ncc = G if H else 0\nfn = lambda: G\nsb = G[H]\nsl = vals[G:H]\nfor q in G:\n    pass\nwhile G:\n    break\nif G:\n    pass\nit = [G for _x in H]\ngen = (w3 for w3 in G if H)\ndef genfn():\n    yield G\nasync def aw():\n    await G\ndef raiser():\n    raise G\ndef asserter():\n    assert G\nwith G as h:\n    pass\nmatch G:\n    case _:\n        pass\ndef sink(v):\n    return v\nsink(k=G)\ndef with_default(x=H):\n    return x\nw = (w2 := G)\nG\n",
            )
            .unwrap();
        let named = |n: &str| -> Vec<_> { refs.iter().filter(|r| r.name == n).collect() };
        // return, init, binary, unary, comparison, boolean, not, list,
        // tuple, set, dict-value, conditional, lambda, subscript-value,
        // slice-bound, for-iterable, while/if conditions, comprehension
        // body, comprehension iterable, yield, await, raise, assert, with
        // item, match subject, keyword value, walrus value, bare statement
        // — each exactly once.
        let g = named("G");
        assert_eq!(g.len(), 29, "all G reads, no doubles: {g:?}");
        assert!(g.iter().all(|r| r.kind == ReferenceKind::Value));
        // Binary/comparison/boolean RHS, conditional-alternative,
        // tuple-second, subscript-index, slice-upper, comprehension
        // iterable, `if`-filter, parameter default.
        let h = named("H");
        assert_eq!(h.len(), 10, "all H reads: {h:?}");
        // Single-occurrence reads: subscript container, comprehension
        // element, parameter reads at use sites.
        assert_eq!(named("vals").len(), 1);
        assert_eq!(named("w3").len(), 1);
        assert_eq!(named("v").len(), 1);
        assert_eq!(named("x").len(), 1);
        let sink = named("sink");
        assert_eq!(sink.len(), 1);
        assert_eq!(sink[0].kind, ReferenceKind::Call);
        // Writes, bindings, and labels are never reads.
        assert!(named("t").is_empty(), "assign-LHS is a write");
        assert!(named("k").is_empty(), "dict keys are labels");
        assert!(named("q").is_empty(), "for-targets bind");
        assert!(named("_x").is_empty(), "comprehension targets bind");
        assert!(named("h").is_empty(), "`as` aliases bind");
        assert!(named("w2").is_empty(), "walrus names bind");
        // No strays anywhere: 29 + 10 + 1 + 1 + 1 + 1 + 1 rows total.
        assert_eq!(refs.len(), 44, "unexpected rows: {refs:?}");
        // Container attribution survives: the return sits in `use`.
        assert!(g.iter().any(|r| r.container == "pkg.auth.service::use"));
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
            .find(|i| i.module_path == "pkg.auth.util::helper")
            .expect("relative helper");
        assert_eq!(helper.alias, Some("h".to_string()));

        assert!(imports
            .iter()
            .any(|i| i.module_path == "pkg.auth::AuthService"));
        assert!(imports.iter().any(|i| i.module_path == "typing::*"));
    }

    #[test]
    fn extracts_dynamic_loader_edges() {
        let plugin = PythonPlugin;
        let imports = plugin
            .extract_imports(
                test_path(),
                "import importlib\nmod = importlib.import_module(\"pkg.real\")\nlegacy = __import__(\"pkg.old\")\nimportlib.import_module(\"pkg.side\")\nimportlib.import_module(name)\nimportlib.import_module(f\"pkg.{name}\")\n",
            )
            .unwrap();
        let find = |m: &str| imports.iter().find(|i| i.module_path == m).cloned();

        let aliased = find("pkg.real").expect("aliased loader edge");
        assert_eq!(aliased.alias, Some("mod".to_string()));
        let builtin = find("pkg.old").expect("builtin loader edge");
        assert_eq!(builtin.alias, Some("legacy".to_string()));
        let bare = find("pkg.side").expect("bare loader edge");
        assert_eq!(bare.alias, None);
        // Non-literal and interpolated targets stay out; assigned
        // values emit once (3 loader edges + the importlib import).
        assert_eq!(imports.len(), 4, "imports: {imports:?}");
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
    fn method_calls_keep_receiver_qualifier() {
        let plugin = PythonPlugin;
        let refs = plugin
            .extract_references(test_path(), "import a.b\n\na.b.serve()\ndb.get()\n")
            .unwrap();
        let methods: Vec<(&str, &str)> = refs
            .iter()
            .filter(|r| r.kind == ReferenceKind::Method)
            .map(|r| (r.name.as_str(), r.qualifier.as_str()))
            .collect();
        assert_eq!(methods, vec![("serve", "a.b"), ("get", "db")]);
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

    #[test]
    fn decorators_emit_references_scoped_to_decorated_def() {
        let plugin = PythonPlugin;
        let src = "import app\n\ndef auth(fn):\n    return fn\n\n@auth\ndef login():\n    return 1\n\n@app.route(\"/x\")\ndef index():\n    return 2\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();

        let auth = refs
            .iter()
            .find(|r| r.name == "auth" && r.kind == ReferenceKind::Call)
            .expect("auth decorator ref");
        assert_eq!(auth.container, "pkg.auth.service::login");
        // Call-form decorators attribute to the decorated def, once only.
        let routes: Vec<&Reference> = refs.iter().filter(|r| r.name == "route").collect();
        assert_eq!(routes.len(), 1, "refs: {refs:?}");
        assert_eq!(routes[0].kind, ReferenceKind::Method);
        assert_eq!(routes[0].qualifier, "app");
        assert_eq!(routes[0].container, "pkg.auth.service::index");
    }

    #[test]
    fn except_handlers_emit_caught_types_not_aliases() {
        let plugin = PythonPlugin;
        let src = "class AppError(Exception):\n    pass\n\nclass OtherError(Exception):\n    pass\n\ndef run():\n    try:\n        work()\n    except AppError as err:\n        fix(err)\n    except (AppError, OtherError):\n        retry()\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();

        let app: Vec<&Reference> = refs.iter().filter(|r| r.name == "AppError").collect();
        assert_eq!(app.len(), 2, "refs: {refs:?}");
        assert!(app.iter().all(|r| r.kind == ReferenceKind::Type));
        assert!(app.iter().all(|r| r.container == "pkg.auth.service::run"));
        assert!(refs
            .iter()
            .any(|r| r.name == "OtherError" && r.kind == ReferenceKind::Type));
        // The `as` alias (line 10) is a binding, never a caught
        // type; its later use still reads.
        assert!(!refs.iter().any(|r| r.name == "err" && r.start_line == 10));
        assert!(!refs.iter().any(|r| r.name == "err" && r.kind == ReferenceKind::Type));
        assert!(refs
            .iter()
            .any(|r| r.name == "err" && r.kind == ReferenceKind::Value && r.start_line == 11));
    }

    #[test]
    fn attribute_reads_emit_receivers_once() {
        let plugin = PythonPlugin;
        let src = "def total(items):\n    return items.length\n\ndef chain(a):\n    return a.b.c()\n\nclass Svc:\n    def run(self):\n        return self.check()\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();

        let items: Vec<&Reference> = refs.iter().filter(|r| r.name == "items").collect();
        assert_eq!(items.len(), 1, "refs: {refs:?}");
        assert_eq!(items[0].kind, ReferenceKind::Value);
        // Chains read their base once; the callee keeps call rules.
        let a: Vec<&Reference> = refs.iter().filter(|r| r.name == "a").collect();
        assert_eq!(a.len(), 1, "refs: {refs:?}");
        assert!(refs.iter().any(|r| r.name == "c" && r.kind == ReferenceKind::Method));
        // Conventional receivers are noise, never rows.
        assert!(!refs.iter().any(|r| r.name == "self"));
    }

    #[test]
    fn match_value_patterns_read_but_captures_bind() {
        let plugin = PythonPlugin;
        let src = "import logging.config\n\nclass Point:\n    pass\n\ndef name(c):\n    match c:\n        case Color.RED:\n            return \"red\"\n        case Point(x=0):\n            return \"origin\"\n        case other:\n            return other\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();

        // Value and class patterns read their names, once each.
        for name in ["Color", "Point"] {
            let rows: Vec<&Reference> = refs.iter().filter(|r| r.name == name).collect();
            assert_eq!(rows.len(), 1, "{name}: {refs:?}");
            assert_eq!(rows[0].kind, ReferenceKind::Value);
        }
        // Capture patterns bind; import paths are not reads.
        assert!(!refs.iter().any(|r| r.name == "other" && r.start_line == 12));
        assert!(!refs.iter().any(|r| r.name == "logging"));
        assert!(!refs.iter().any(|r| r.name == "config"));
    }

    #[test]
    fn fstring_interpolations_emit_direct_identifier_reads() {
        let plugin = PythonPlugin;
        let src = "def greet(name):\n    return f\"hello {name}, {format(name)}!\"\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();

        let name: Vec<&Reference> = refs.iter().filter(|r| r.name == "name").collect();
        // Direct interpolation is a Value read; the call argument keeps the
        // argument rules — one row per site, no doubles.
        assert_eq!(name.len(), 2, "refs: {refs:?}");
        assert!(name.iter().all(|r| r.kind == ReferenceKind::Value));
        assert!(refs.iter().any(|r| r.name == "format" && r.kind == ReferenceKind::Call));
    }

    #[test]
    fn member_definitions_are_field_symbols() {
        let plugin = PythonPlugin;
        let src = "class Config:\n    port = 8080\n    host: str = \"x\"\n\n    def __init__(self):\n        self.retries = 3\n        other.timeout = 5\n\n    def serve(self):\n        pass\n\n\nclass Color:\n    RED = 1\n    BLUE = 2\n";
        let syms = plugin.extract_symbols(test_path(), src).unwrap();
        let find = |n: &str| syms.iter().find(|s| s.name == n).cloned();
        let field = SymbolKind::Other("field".into());
        assert_eq!(find("port").unwrap().kind, field);
        assert_eq!(find("port").unwrap().start_line, 2);
        assert_eq!(find("host").unwrap().kind, field);
        assert_eq!(find("retries").unwrap().kind, field);
        assert_eq!(find("RED").unwrap().kind, field);
        assert_eq!(find("BLUE").unwrap().kind, field);
        // Methods keep their kinds; other-receiver writes stay out.
        assert_eq!(find("serve").unwrap().kind, SymbolKind::Function);
        assert!(find("timeout").is_none());
        assert!(find("other").is_none());
    }

    #[test]
    fn member_reads_emit_attributes_once_with_qualifier() {
        let plugin = PythonPlugin;
        let src = "def f(cfg):\n    p = cfg.port\n    cfg.port = 1\n    cfg.count += 1\n    for cfg.key in items:\n        pass\n    with open(path) as cfg.handle:\n        pass\n    del cfg.gone\n    del cfg.old, junk\n    q = cfg.a.b\n    cfg.serve()\n    r = Color.RED\n    d = {cfg.tag: 1}\n    first, cfg.multi = pair\n";
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
        let red = named("RED");
        assert_eq!(red.len(), 1);
        assert_eq!(red[0].qualifier, "Color");
        // Writes and labels never read; the call keeps Method rules.
        for bound in ["key", "handle", "gone", "old", "tag", "multi"] {
            assert!(named(bound).is_empty(), "{bound} must not read: {refs:?}");
        }
        assert_eq!(named("serve").len(), 1);
        assert_eq!(named("serve")[0].kind, ReferenceKind::Method);
        // Receivers still read at every site, including writes.
        assert_eq!(named("cfg").len(), 11, "refs: {refs:?}");
        assert!(named("cfg").iter().all(|r| r.kind == ReferenceKind::Value));
        // No strays: 11 receivers + port/count/a/b/serve + Color/RED +
        // items/open/path/pair.
        assert_eq!(refs.len(), 22, "unexpected rows: {refs:?}");
    }
}
