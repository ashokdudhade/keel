//! JavaScript/JSX language plugin: Tree-sitter based symbol and reference extraction.
//!
//! `module_path` is derived from the file path with the extension stripped
//! (e.g. `src/auth/service`), using forward slashes.
//!
//! `.js`/`.mjs`/`.cjs` and `.jsx` share the JavaScript grammar (which includes
//! JSX). Separate plugin registrations keep extension dispatch explicit.

use super::{file_path_key, is_top_level_js_declaration, path_module_identity, resolve_relative_path_module, DepthGuard, LanguagePlugin, WalkBudget, MAX_WALK_DEPTH};
use crate::error::{Result, KeelError};
use crate::graph::types::{ImplRecord, Import, Reference, ReferenceKind, Symbol, SymbolKind};
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
    fn has_syntax_errors(&self, source_code: &str) -> bool {
        Self::parse(source_code)
            .map(|tree| tree.root_node().has_error())
            .unwrap_or(false)
    }
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

    fn extract_impls(&self, _path: &Path, source_code: &str) -> Result<Vec<ImplRecord>> {
        extract_impls(source_code)
    }
}

impl LanguagePlugin for JsxPlugin {
    fn has_syntax_errors(&self, source_code: &str) -> bool {
        JavaScriptPlugin::parse(source_code)
            .map(|tree| tree.root_node().has_error())
            .unwrap_or(false)
    }
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

    fn extract_impls(&self, _path: &Path, source_code: &str) -> Result<Vec<ImplRecord>> {
        extract_impls(source_code)
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
    let budget = WalkBudget::new();
    walk_symbols(tree.root_node(), src, &module_path, "", &mut out, &budget)?;
    Ok(out)
}

fn extract_references(path: &Path, source_code: &str) -> Result<Vec<Reference>> {
    let tree = JavaScriptPlugin::parse(source_code)?;
    let src = source_code.as_bytes();
    let file_key = file_path_key(path);
    let module_path = path_module_identity(path);
    let mut scope: Vec<String> = Vec::new();
    let mut out = Vec::new();
    let budget = WalkBudget::new();
    walk_references(
        tree.root_node(),
        src,
        &file_key,
        &module_path,
        &mut scope,
        &mut out,
        &budget,
    )?;
    Ok(out)
}

fn extract_imports(path: &Path, source_code: &str) -> Result<Vec<Import>> {
    let tree = JavaScriptPlugin::parse(source_code)?;
    let src = source_code.as_bytes();
    let mut out = Vec::new();
    let budget = WalkBudget::new();
    walk_imports(tree.root_node(), src, &mut out, &budget)?;
    for imp in &mut out {
        imp.module_path = resolve_relative_path_module(path, &imp.module_path);
    }
    Ok(out)
}

fn extract_impls(source_code: &str) -> Result<Vec<ImplRecord>> {
    let tree = JavaScriptPlugin::parse(source_code)?;
    let src = source_code.as_bytes();
    let mut out = Vec::new();
    let budget = WalkBudget::new();
    walk_impls(tree.root_node(), src, &mut out, &budget)?;
    Ok(out)
}

/// Collect (class, base) pairs from `class C extends Base`.
///
/// Bare identifiers and member final segments (`extends ns.Base` counts
/// as `Base` — the query side attributes via the file's imports) count.
/// Class expressions (`const X = class extends Base`) count too, named
/// by their declarator.
fn walk_impls(
    node: Node,
    src: &[u8],
    out: &mut Vec<ImplRecord>,
    budget: &WalkBudget,
) -> Result<()> {
    let _guard = DepthGuard::enter(budget)
        .ok_or_else(|| KeelError::TooDeeplyNested { limit: MAX_WALK_DEPTH })?;
    let type_name = match node.kind() {
        "class_declaration" => node
            .child_by_field_name("name")
            .filter(|n| n.kind() == "identifier")
            .map(|n| node_text(n, src).map(str::to_string))
            .transpose()?,
        "class" => class_expression_name(node, src)?,
        _ => None,
    };
    if let Some(type_name) = type_name {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() != "class_heritage" {
                continue;
            }
            let mut hcursor = child.walk();
            for base in child.children(&mut hcursor) {
                if let Some(base_name) = heritage_base_name(base, src)? {
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
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_impls(child, src, out, budget)?;
    }
    Ok(())
}

/// Base name of a heritage entry: a bare identifier or the final
/// segment of a member expression (`ns.Base` counts as `Base`).
/// Anything else is skipped.
fn heritage_base_name(node: Node, src: &[u8]) -> Result<Option<String>> {
    match node.kind() {
        "identifier" => Ok(Some(node_text(node, src)?.to_string())),
        "member_expression" => match node.child_by_field_name("property") {
            Some(p) if matches!(p.kind(), "identifier" | "property_identifier") => {
                Ok(Some(node_text(p, src)?.to_string()))
            }
            _ => Ok(None),
        },
        _ => Ok(None),
    }
}

/// Name a `class` expression: its own `class Y` name first, else the
/// enclosing `const X = class ...` declarator or `X = class ...`
/// assignment target (bare or `obj.X`). Anonymous exports
/// (`export default class ...`) have none of these and stay out.
fn class_expression_name(node: Node, src: &[u8]) -> Result<Option<String>> {
    if let Some(name) = node.child_by_field_name("name") {
        if name.kind() == "identifier" {
            return Ok(Some(node_text(name, src)?.to_string()));
        }
    }
    if let Some(parent) = node.parent() {
        if parent.kind() == "variable_declarator"
            && parent.child_by_field_name("value").is_some_and(|v| v == node)
        {
            if let Some(name) = parent.child_by_field_name("name") {
                if name.kind() == "identifier" {
                    return Ok(Some(node_text(name, src)?.to_string()));
                }
            }
        }
        if parent.kind() == "assignment_expression"
            && parent.child_by_field_name("right").is_some_and(|r| r == node)
        {
            if let Some(left) = parent.child_by_field_name("left") {
                if left.kind() == "identifier" {
                    return Ok(Some(node_text(left, src)?.to_string()));
                }
                if left.kind() == "member_expression" {
                    if let Some(prop) = left.child_by_field_name("property") {
                        if matches!(prop.kind(), "property_identifier" | "identifier") {
                            return Ok(Some(node_text(prop, src)?.to_string()));
                        }
                    }
                }
            }
        }
    }
    Ok(None)
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
    container: &str,
    out: &mut Vec<Symbol>,
    budget: &WalkBudget,
) -> Result<()> {
    let _guard = DepthGuard::enter(budget)
        .ok_or_else(|| KeelError::TooDeeplyNested { limit: MAX_WALK_DEPTH })?;
    match node.kind() {
        "function_declaration" | "generator_function_declaration" | "class_declaration" => {
            let kind = if node.kind() == "class_declaration" {
                SymbolKind::Struct
            } else {
                SymbolKind::Function
            };
            emit_named_symbol(node, kind, module_path, container, out, src)?;
        }
        "method_definition" => {
            emit_named_symbol(node, SymbolKind::Function, module_path, container, out, src)?;
        }
        // Class fields are indexed items (`field` kind), so member reads
        // resolve to a definition. The name lives in the `property`
        // field; computed/string/number/private names stay out.
        "field_definition" => {
            if let Some(prop) = node.child_by_field_name("property") {
                if prop.kind() == "property_identifier" {
                    push_symbol(prop, SymbolKind::Other("field".into()), module_path, container, out, src)?;
                }
            }
        }
        "variable_declarator" => {
            emit_bound_function(node, module_path, container, out, src)?;
        }
        "lexical_declaration" | "variable_declaration"
            if is_top_level_js_declaration(node) =>
        {
            emit_top_level_variables(node, module_path, container, out, src)?;
        }
        _ => {}
    }
    // Members of a class body carry the class name; anything else
    // inherits the enclosing container unchanged.
    let owned: Option<String> = match node.kind() {
        "class_declaration" => match node.child_by_field_name("name") {
            Some(name) if matches!(name.kind(), "identifier" | "property_identifier") => {
                Some(node_text(name, src)?.to_string())
            }
            _ => None,
        },
        _ => None,
    };
    let next_container = owned.as_deref().unwrap_or(container);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_symbols(child, src, module_path, next_container, out, budget)?;
    }
    Ok(())
}

fn emit_bound_function(
    declarator: Node,
    module_path: &str,
    container: &str,
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
        push_symbol(name, SymbolKind::Function, module_path, container, out, src)?;
    }
    Ok(())
}

/// Emit top-level `const`/`let`/`var` bindings (`Const` for `const`,
/// `variable` otherwise). Function-valued declarators are skipped here —
/// [`emit_bound_function`] owns those as `Function` — and destructured
/// names (`object_pattern`/`array_pattern`) are not simple bindings.
fn emit_top_level_variables(
    node: Node,
    module_path: &str,
    container: &str,
    out: &mut Vec<Symbol>,
    src: &[u8],
) -> Result<()> {
    // `lexical_declaration` carries a required `kind` field (`const` |
    // `let`); `variable_declaration` has none and is always `var`.
    let kind_text = node
        .child_by_field_name("kind")
        .and_then(|k| node_text(k, src).ok());
    let kind = if kind_text == Some("const") {
        SymbolKind::Const
    } else {
        SymbolKind::Other("variable".into())
    };
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "variable_declarator" {
            continue;
        }
        if let Some(value) = child.child_by_field_name("value") {
            if matches!(
                value.kind(),
                "arrow_function" | "function_expression" | "generator_function"
            ) {
                continue;
            }
        }
        if let Some(name) = child.child_by_field_name("name") {
            if name.kind() == "identifier" {
                push_symbol(name, kind.clone(), module_path, container, out, src)?;
            }
        }
    }
    Ok(())
}

fn emit_named_symbol(
    node: Node,
    kind: SymbolKind,
    module_path: &str,
    container: &str,
    out: &mut Vec<Symbol>,
    src: &[u8],
) -> Result<()> {
    if let Some(name) = node.child_by_field_name("name") {
        if matches!(name.kind(), "identifier" | "property_identifier") {
            push_symbol(name, kind, module_path, container, out, src)?;
        }
    }
    Ok(())
}

fn push_symbol(
    name_node: Node,
    kind: SymbolKind,
    module_path: &str,
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
        module_path: module_path.to_string(),
        container: container.to_string(),
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
    budget: &WalkBudget,
) -> Result<()> {
    let _guard = DepthGuard::enter(budget)
        .ok_or_else(|| KeelError::TooDeeplyNested { limit: MAX_WALK_DEPTH })?;
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
                        walk_references(child, src, file_key, module_path, scope, out, budget)?;
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
                            walk_references(value, src, file_key, module_path, scope, out, budget)?;
                            scope.pop();
                            return Ok(());
                        }
                    }
                }
            }
            // Initializer reads (`const t = value`); the bound name is a
            // definition, never a reference. (Arrow bodies return early
            // above with their scope pushed.)
            emit_field_identifier(node, "value", src, file_key, module_path, scope, out)?;
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
        // Member reads (`items.length`) read the receiver, mirroring the
        // receiver rule for calls, and the property itself. Members owned
        // by another arm (call/new callees, JSX tags, heritage clauses)
        // are skipped so no site emits twice; properties in write
        // positions (plain-assignment LHS, `for-in` targets, `delete`
        // operands) are writes, never reads.
        "member_expression" => {
            emit_member_receiver(node, src, file_key, module_path, scope, out)?;
            emit_member_property(node, src, file_key, module_path, scope, out)?;
        }
        // JSX elements (`<Widget/>`, `<Foo.Bar/>`) invoke components. The
        // closing tag repeats the name, so only opening/self-closing tags
        // emit; lowercase tags are host elements (`<div/>`), not references.
        "jsx_opening_element" | "jsx_self_closing_element" => {
            emit_jsx_reference(node, src, file_key, module_path, scope, out)?;
        }
        // Decorators apply the named decorator (`@Logged`), so the name is
        // a reference attributed to the decorated definition. Children are
        // walked under the decorated name so call-form decorators
        // (`@Route("/x")`) attribute there too.
        "decorator" => {
            if emit_decorator_reference(node, src, file_key, module_path, scope, out, budget)? {
                return Ok(());
            }
        }
        // Bare identifiers in value positions: positional arguments.
        // Object-literal values use the `pair` arm below; property keys
        // are labels, not references.
        "arguments" => {
            emit_argument_values(node, src, file_key, module_path, scope, out)?;
        }
        // Template interpolations (`` `hi ${name}` ``) read directly
        // embedded identifiers; richer expressions keep the normal rules
        // via the recursion below.
        "template_substitution" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.is_named() && child.kind() == "identifier" {
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
        // Shorthand properties (`{ handler }`) read the variable. Binding
        // patterns use the distinct `shorthand_property_identifier_pattern`
        // node and stay untouched.
        "shorthand_property_identifier" => {
            push_reference(
                node_text(node, src)?.to_string(),
                node,
                ReferenceKind::Value,
                file_key,
                module_path,
                scope,
                out,
            );
        }
        // Heritage (`class C extends Base`): the named type is a use
        // attributed to the class (whose scope is already pushed).
        // Call-form heritage (`extends mixin(Base)`) falls through to the
        // call rules via the normal recursion.
        "class_heritage" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.is_named() && !matches!(child.kind(), "call_expression" | "new_expression") {
                    emit_type_subtree(child, src, file_key, module_path, scope, out)?;
                }
            }
        }
        // Value reads in statement/expression positions (`return x`,
        // `throw err`, `...args`, `await p`, `a + b`, `!ok`, `c ? a : b`,
        // `[a]`, `(x)`, `a, b`, `{v}` in JSX): only DIRECT identifier
        // children emit here; nested composites fall through to the
        // recursion below so each site emits exactly once. Attribute
        // names (`timeout=` in `<W timeout={v}>`) sit under
        // `jsx_attribute`, not `jsx_expression`, so they stay out.
        "return_statement" | "throw_statement" | "spread_element"
        | "await_expression" | "yield_expression" | "binary_expression"
        | "unary_expression" | "ternary_expression" | "array"
        | "sequence_expression" | "parenthesized_expression"
        | "jsx_expression" => {
            emit_direct_identifiers(node, src, file_key, module_path, scope, out)?;
        }
        // `export default Name` mentions the local: renaming it must
        // update the export, so it reads. Declarations, specifier
        // lists, and re-exports have richer shapes and stay out
        // (specifiers are owned by the arm below).
        "export_statement" => {
            emit_export_value(node, src, file_key, module_path, scope, out)?;
        }
        // Export specifiers (`export { helper, count }`) mention the
        // local name: renaming it must update the specifier, so it
        // reads. The `alias` (`export { h as helper }`) names the
        // export and stays out, as does the `source` of re-exports
        // (owned by the import edges).
        "export_specifier" => {
            if let Some(name) = node.child_by_field_name("name") {
                if name.kind() == "identifier" {
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
            }
        }
        // Plain assignment reads only the RHS (`x = v`); the LHS is a
        // write, not a read. Compound assignment (`x += v`) and updates
        // (`x++`) read their targets too.
        "assignment_expression" => {
            emit_field_identifier(node, "right", src, file_key, module_path, scope, out)?;
        }
        "augmented_assignment_expression" => {
            emit_field_identifier(node, "left", src, file_key, module_path, scope, out)?;
            emit_field_identifier(node, "right", src, file_key, module_path, scope, out)?;
        }
        "update_expression" => {
            emit_field_identifier(node, "argument", src, file_key, module_path, scope, out)?;
        }
        // Object-literal values (`{ k: v }`); keys are labels, never
        // reads. (Binding patterns use the distinct `pair_pattern`.)
        "pair" => {
            emit_field_identifier(node, "value", src, file_key, module_path, scope, out)?;
        }
        // Computed member access reads both sides (`a[b]`).
        "subscript_expression" => {
            emit_field_identifier(node, "object", src, file_key, module_path, scope, out)?;
            emit_field_identifier(node, "index", src, file_key, module_path, scope, out)?;
        }
        // `for (x of ITER)` / `for (x in OBJ)` read the iterable; the
        // loop target is a binding/write.
        "for_in_statement" => {
            emit_field_identifier(node, "right", src, file_key, module_path, scope, out)?;
        }
        // `case X:` reads the discriminant value.
        "switch_case" => {
            emit_field_identifier(node, "value", src, file_key, module_path, scope, out)?;
        }
        // Default values (`(a = d)`, `[a = d] = v`, `{a = d} = v`):
        // the default reads; the bound pattern is a definition,
        // never a reference. (Composites recurse into their own
        // arms via the walk below.)
        "assignment_pattern" | "object_assignment_pattern" => {
            emit_field_identifier(node, "right", src, file_key, module_path, scope, out)?;
        }
        // Class field initializers (`x = v`); the field name is a
        // definition, never a reference.
        "field_definition" => {
            emit_field_identifier(node, "value", src, file_key, module_path, scope, out)?;
        }
        // Arrow bodies (`=> x`); block bodies recurse normally and
        // parameters bind.
        "arrow_function" => {
            emit_field_identifier(node, "body", src, file_key, module_path, scope, out)?;
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_references(child, src, file_key, module_path, scope, out, budget)?;
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
                    let qualifier = func
                        .child_by_field_name("object")
                        .map(|o| node_text(o, src))
                        .transpose()?
                        .unwrap_or("")
                        .to_string();
                    push_method_reference(
                        text.to_string(),
                        prop,
                        qualifier,
                        file_key,
                        module_path,
                        scope,
                        out,
                    );
                }
            }
            // The receiver (`db` in `db.get()`) is a value use.
            if let Some(recv) = func.child_by_field_name("object") {
                if recv.kind() == "identifier" {
                    push_reference(
                        node_text(recv, src)?.to_string(),
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
        _ => {}
    }
    Ok(())
}

/// Emit the decorator name as a reference scoped to the decorated
/// definition, so impact analysis attributes decorator applications to the
/// definition they wrap. Returns true when fully handled; false falls back
/// to the normal recursion.
fn emit_decorator_reference(
    decorator: Node,
    src: &[u8],
    file_key: &str,
    module_path: &str,
    scope: &mut Vec<String>,
    out: &mut Vec<Reference>,
    budget: &WalkBudget,
) -> Result<bool> {
    let mut cursor = decorator.walk();
    let Some(expr) = decorator.children(&mut cursor).find(|c| c.is_named()) else {
        return Ok(false);
    };
    // The decorated declaration is the parent (`class Service` with the
    // decorator inside) or, for `@D export class C`, the parent
    // `export_statement`'s `declaration` child.
    let decorated = decorator.parent().and_then(|p| {
        named_child(&p, src)
            .or_else(|| p.child_by_field_name("declaration").and_then(|d| named_child(&d, src)))
    });
    let Some(name) = decorated else {
        return Ok(false);
    };
    // The decorator may sit inside the declaration that already pushed its
    // scope (`class C { @D m() {} }`); never double-push.
    let pushed = scope.last().is_none_or(|top| *top != name);
    if pushed {
        scope.push(name);
    }
    if expr.kind() == "call_expression" {
        // `@Route("/x")`: walk the call under the decorated name so the
        // `call_expression`/`arguments` arms attribute it there.
        let mut inner = decorator.walk();
        for child in decorator.children(&mut inner) {
            walk_references(child, src, file_key, module_path, scope, out, budget)?;
        }
    } else {
        emit_call_reference(expr, src, file_key, module_path, scope, out)?;
    }
    if pushed {
        scope.pop();
    }
    Ok(true)
}

/// Emit every identifier under a heritage subtree as a `type` use.
/// Call/new subtrees are skipped here so the normal recursion attributes
/// them as calls (`extends mixin(Base)`).
fn emit_type_subtree(
    root: Node,
    src: &[u8],
    file_key: &str,
    module_path: &str,
    scope: &[String],
    out: &mut Vec<Reference>,
) -> Result<()> {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.id() != root.id() && matches!(node.kind(), "call_expression" | "new_expression") {
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

/// Emit the receiver of a value-position member read (`items` in
/// `items.length`). Call/new callees, JSX tags, and heritage clauses are
/// owned by other arms and skipped.
fn emit_member_receiver(
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
    // Call/new callees are owned by the call arms; the check is direct
    // so chains still read their base (`a.b.c()` reads `a` via the
    // inner member). JSX tags and heritage clauses own whole member
    // subtrees, so those climb to the outermost member — but attribute
    // and child expressions (`title={a.b}`) are not tag names and fall
    // through to emit.
    if let Some(parent) = node.parent() {
        if matches!(parent.kind(), "call_expression" | "new_expression") {
            return Ok(());
        }
    }
    let mut outer = node;
    while let Some(parent) = outer.parent() {
        if parent.kind() == "member_expression"
            && parent.child_by_field_name("object").is_some_and(|o| o.id() == outer.id())
        {
            outer = parent;
        } else {
            break;
        }
    }
    if let Some(parent) = outer.parent() {
        if matches!(
            parent.kind(),
            "jsx_opening_element"
                | "jsx_self_closing_element"
                | "class_heritage"
                | "extends_clause"
        ) {
            return Ok(());
        }
    }
    push_reference(
        node_text(recv, src)?.to_string(),
        recv,
        ReferenceKind::Value,
        file_key,
        module_path,
        scope,
        out,
    );
    Ok(())
}

/// Emit the property of a value-position member read (`length` in
/// `items.length`) as a Value reference, keeping the receiver text as
/// the qualifier exactly like method calls. Call/new callees, JSX tags,
/// and heritage clauses are owned by other arms; properties in write
/// positions (plain-assignment LHS, `for-in` targets, `delete` operands)
/// are writes. Updates and compound targets read, so they emit.
fn emit_member_property(
    node: Node,
    src: &[u8],
    file_key: &str,
    module_path: &str,
    scope: &[String],
    out: &mut Vec<Reference>,
) -> Result<()> {
    let Some(prop) = node.child_by_field_name("property") else {
        return Ok(());
    };
    if !matches!(prop.kind(), "property_identifier" | "identifier") {
        return Ok(());
    }
    let Some(object) = node.child_by_field_name("object") else {
        return Ok(());
    };
    if let Some(parent) = node.parent() {
        // Call/new callees are owned by the call arms; the check is
        // direct so chains still read their middle (`a.b.c()` reads
        // `b` via the inner member).
        if matches!(parent.kind(), "call_expression" | "new_expression") {
            return Ok(());
        }
        // Plain-assignment LHS and `for-in` targets write the property.
        // (Augmented assignment and updates read it, so they emit.)
        // Destructuring targets nest, so the check climbs through
        // transparent pattern wrappers first.
        let target = climb_target_wrappers(node);
        if let Some(target_parent) = target.parent() {
            if matches!(
                target_parent.kind(),
                "assignment_expression" | "for_in_statement"
            ) && target_parent
                .child_by_field_name("left")
                .is_some_and(|left| is_within(target, left))
            {
                return Ok(());
            }
        }
        // `delete a.b` removes the property; it is not a value read.
        if parent.kind() == "unary_expression"
            && parent
                .child_by_field_name("operator")
                .and_then(|op| node_text(op, src).ok())
                == Some("delete")
            && parent
                .child_by_field_name("argument")
                .is_some_and(|arg| arg.id() == node.id())
        {
            return Ok(());
        }
    }
    // JSX tags and heritage clauses own whole member subtrees; climb to
    // the outermost member like the receiver arm.
    let mut outer = node;
    while let Some(parent) = outer.parent() {
        if parent.kind() == "member_expression"
            && parent
                .child_by_field_name("object")
                .is_some_and(|o| o.id() == outer.id())
        {
            outer = parent;
        } else {
            break;
        }
    }
    if let Some(parent) = outer.parent() {
        if matches!(
            parent.kind(),
            "jsx_opening_element"
                | "jsx_self_closing_element"
                | "class_heritage"
                | "extends_clause"
        ) {
            return Ok(());
        }
    }
    let qualifier = node_text(object, src)?.to_string();
    push_value_reference(
        node_text(prop, src)?.to_string(),
        prop,
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

/// Climb from `node` through transparent pattern wrappers (array /
/// object / rest patterns and parens) to the enclosing target itself,
/// so `[a.b, c] = v` detects the write position. `assignment_pattern`
/// defaults (`[x = a.b] = v`) and computed keys (`{[a.b]: x} = v`)
/// read, so only the target slot climbs.
fn climb_target_wrappers(mut node: Node) -> Node {
    while let Some(parent) = node.parent() {
        if parent.kind() == "assignment_pattern" {
            let in_left = parent
                .child_by_field_name("left")
                .is_some_and(|left| is_within(node, left));
            if !in_left {
                break;
            }
            node = parent;
        } else if parent.kind() == "pair_pattern" {
            let in_value = parent
                .child_by_field_name("value")
                .is_some_and(|value| is_within(node, value));
            if !in_value {
                break;
            }
            node = parent;
        } else if matches!(
            parent.kind(),
            "array_pattern" | "object_pattern" | "rest_pattern" | "parenthesized_expression"
        ) {
            node = parent;
        } else {
            break;
        }
    }
    node
}

/// Text of a declaration's `name` field when it is a plain identifier.
fn named_child(node: &Node, src: &[u8]) -> Option<String> {
    node.child_by_field_name("name")
        .filter(|n| matches!(n.kind(), "identifier" | "property_identifier" | "type_identifier"))
        .and_then(|n| node_text(n, src).ok().map(str::to_string))
}

/// Emit a component invocation for a JSX opening/self-closing tag. Simple
/// tags follow the host/component case convention (lowercase `<div/>` is a
/// host element, uppercase `<Widget/>` a component call); member tags
/// (`<Foo.Bar/>`) are always component calls and keep the object as the
/// qualifier, mirroring `member_expression`.
fn emit_jsx_reference(
    tag: Node,
    src: &[u8],
    file_key: &str,
    module_path: &str,
    scope: &[String],
    out: &mut Vec<Reference>,
) -> Result<()> {
    let Some(name) = tag.child_by_field_name("name") else {
        return Ok(());
    };
    match name.kind() {
        "identifier" => {
            let text = node_text(name, src)?;
            if text.starts_with(|c: char| c.is_ascii_uppercase()) {
                push_reference(
                    text.to_string(),
                    name,
                    ReferenceKind::Call,
                    file_key,
                    module_path,
                    scope,
                    out,
                );
            }
        }
        // Member tags (`<Foo.Bar/>`) are always component calls, with the
        // object kept as the qualifier exactly like `member_expression`.
        "member_expression" => {
            emit_call_reference(name, src, file_key, module_path, scope, out)?;
        }
        _ => {}
    }
    Ok(())
}

/// Emit direct identifier arguments and object-literal values as Value
/// references. Property keys and nested structures are left to the normal
/// recursion (nested calls) or skipped (labels); receivers are captured
/// with the callee.
/// Emit direct identifier arguments as Value references. Nested
/// structures fall through to the normal recursion (nested calls, object
/// pairs via the `pair` arm); receivers are captured with the callee.
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
/// Emit the exported name when an `export_statement` holds exactly one
/// bare identifier (`export default helper`).
fn emit_export_value(
    node: Node,
    src: &[u8],
    file_key: &str,
    module_path: &str,
    scope: &[String],
    out: &mut Vec<Reference>,
) -> Result<()> {
    let mut cursor = node.walk();
    let named: Vec<Node> = node.named_children(&mut cursor).collect();
    if let [only] = named.as_slice() {
        if only.kind() == "identifier" {
            push_reference(
                node_text(*only, src)?.to_string(),
                *only,
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
/// for member reads, `items` in `items.length`).
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

fn walk_imports(
    node: Node,
    src: &[u8],
    out: &mut Vec<Import>,
    budget: &WalkBudget,
) -> Result<()> {
    let _guard = DepthGuard::enter(budget)
        .ok_or_else(|| KeelError::TooDeeplyNested { limit: MAX_WALK_DEPTH })?;
    match node.kind() {
        "import_statement" => {
            emit_esm_import(node, src, out)?;
            return Ok(());
        }
        "export_statement" => {
            // Re-export: `export * from './a'` / `export { a } from './a'`.
            // A binding-free module edge (see the TypeScript walker); plain
            // `export …` declarations have no `source` and fall through.
            if let Some(s) = node.child_by_field_name("source") {
                out.push(Import {
                    module_path: strip_quotes(node_text(s, src)?),
                    alias: None,
                    file: PathBuf::new(),
                });
                return Ok(());
            }
        }
        "variable_declarator" => {
            emit_require_import(node, src, out)?;
            emit_dynamic_import(node, src, out)?;
        }
        "call_expression" => {
            emit_bare_dynamic_import(node, src, out)?;
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_imports(child, src, out, budget)?;
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

/// Dynamic `import("…")` bound to a name
/// (`const m = await import("…")`): a whole-module edge aliased to the
/// local name, like `require`. Bare calls are owned by
/// [`emit_bare_dynamic_import`].
fn emit_dynamic_import(declarator: Node, src: &[u8], out: &mut Vec<Import>) -> Result<()> {
    let Some(value) = declarator.child_by_field_name("value") else {
        return Ok(());
    };
    let inner = if value.kind() == "await_expression" {
        value.named_children(&mut value.walk()).next()
    } else {
        Some(value)
    };
    let Some(call) = inner.filter(|n| n.kind() == "call_expression") else {
        return Ok(());
    };
    if !is_dynamic_import_call(call) {
        return Ok(());
    }
    let alias = match declarator.child_by_field_name("name") {
        Some(name) if name.kind() == "identifier" => Some(node_text(name, src)?.to_string()),
        _ => None,
    };
    push_dynamic_import(call, alias, src, out)
}

/// Bare dynamic `import("…")` (statement or expression position): a
/// binding-free module edge. Declarator values are skipped — the
/// declarator arm owns those with their alias.
fn emit_bare_dynamic_import(call: Node, src: &[u8], out: &mut Vec<Import>) -> Result<()> {
    if !is_dynamic_import_call(call) || is_declarator_value(call) {
        return Ok(());
    }
    push_dynamic_import(call, None, src, out)
}

fn is_dynamic_import_call(call: Node) -> bool {
    call.child_by_field_name("function")
        .is_some_and(|f| f.kind() == "import")
}

/// True when `call` is the (await-unwrapped) `value` of a variable
/// declarator.
fn is_declarator_value(call: Node) -> bool {
    let mut node = call;
    while let Some(parent) = node.parent() {
        if parent.kind() == "await_expression" {
            node = parent;
            continue;
        }
        return parent.kind() == "variable_declarator"
            && parent.child_by_field_name("value").is_some_and(|v| v == node);
    }
    false
}

fn push_dynamic_import(
    call: Node,
    alias: Option<String>,
    src: &[u8],
    out: &mut Vec<Import>,
) -> Result<()> {
    let Some(args) = call.child_by_field_name("arguments") else {
        return Ok(());
    };
    let mut cursor = args.walk();
    for child in args.named_children(&mut cursor) {
        if child.kind() == "string" {
            out.push(Import {
                module_path: strip_quotes(node_text(child, src)?),
                alias,
                file: PathBuf::new(),
            });
            return Ok(());
        }
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
    fn member_symbols_carry_enclosing_class_container() {
        let plugin = JavaScriptPlugin;
        let syms = plugin
            .extract_symbols(
                test_path(),
                "function top() {}\nclass Store {\n  limit = 1;\n  save() {}\n}\n",
            )
            .unwrap();
        let find = |n: &str| syms.iter().find(|s| s.name == n).cloned();
        assert_eq!(find("top").unwrap().container, "");
        assert_eq!(find("Store").unwrap().container, "");
        assert_eq!(find("save").unwrap().container, "Store");
        assert_eq!(find("limit").unwrap().container, "Store");
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
            .find(|i| i.module_path == "src/auth/util::helper")
            .expect("named helper import");
        assert_eq!(named.alias, Some("h".to_string()));

        let default = imports
            .iter()
            .find(|i| i.module_path == "src/auth/auth")
            .expect("default Auth import");
        assert_eq!(default.alias, Some("Auth".to_string()));

        let side = imports
            .iter()
            .find(|i| i.module_path == "src/auth/polyfill" && i.alias.is_none())
            .expect("side-effect import");
        assert!(side.alias.is_none());

        let cjs = imports
            .iter()
            .find(|i| i.module_path == "fs")
            .expect("require import");
        assert_eq!(cjs.alias, Some("fs".to_string()));
    }

    #[test]
    fn extracts_dynamic_import_edges() {
        let plugin = JavaScriptPlugin;
        let imports = plugin
            .extract_imports(
                Path::new("src/app.js"),
                "import(\"./bare\").then((m) => m.run());\nconst ns = await import(\"./aliased\");\nconst p = import(\"./promise\");\nimport(target);\n",
            )
            .unwrap();
        let bare: Vec<_> = imports
            .iter()
            .filter(|i| i.module_path.ends_with("bare"))
            .collect();
        assert_eq!(bare.len(), 1, "imports: {imports:?}");
        assert_eq!(bare[0].alias, None);
        let aliased: Vec<_> = imports
            .iter()
            .filter(|i| i.module_path.ends_with("aliased"))
            .collect();
        assert_eq!(aliased.len(), 1, "imports: {imports:?}");
        assert_eq!(aliased[0].alias, Some("ns".to_string()));
        let promise: Vec<_> = imports
            .iter()
            .filter(|i| i.module_path.ends_with("promise"))
            .collect();
        assert_eq!(promise.len(), 1, "imports: {imports:?}");
        assert_eq!(promise[0].alias, Some("p".to_string()));
        // Non-string targets stay out; declarator values emit once.
        assert_eq!(imports.len(), 3, "imports: {imports:?}");
    }

    #[test]
    fn extracts_export_from_reexports() {
        let plugin = JavaScriptPlugin;
        let imports = plugin
            .extract_imports(
                Path::new("src/barrel.js"),
                "export * from \"./a\";\nexport { a as b } from \"./a\";\nexport const c = 1;\nexport { c };\n",
            )
            .unwrap();
        let modules: Vec<&str> = imports.iter().map(|i| i.module_path.as_str()).collect();
        assert_eq!(modules, vec!["src/a", "src/a"]);
        assert!(
            imports.iter().all(|i| i.alias.is_none()),
            "export-from binds nothing locally: {imports:?}"
        );
    }

    #[test]
    fn extracts_top_level_variables_not_locals() {
        let plugin = JavaScriptPlugin;
        let syms = plugin
            .extract_symbols(
                Path::new("src/vars.js"),
                "const C = 1;\nlet l = 2;\nvar v = 3;\nexport const E = 4;\nconst [a] = [];\nfunction f() {\n  let local = 5;\n  return local;\n}\n",
            )
            .unwrap();
        let find = |n: &str| syms.iter().find(|s| s.name == n).cloned();
        assert_eq!(find("C").unwrap().kind, SymbolKind::Const);
        assert_eq!(
            find("l").unwrap().kind,
            SymbolKind::Other("variable".into())
        );
        assert_eq!(
            find("v").unwrap().kind,
            SymbolKind::Other("variable".into())
        );
        assert_eq!(find("E").unwrap().kind, SymbolKind::Const);
        assert!(find("a").is_none(), "destructured names are not bindings");
        assert!(find("local").is_none(), "function locals stay out");
        // The pre-existing bound-function arm still owns arrow declarators,
        // with no Const double emit.
        let syms = plugin
            .extract_symbols(Path::new("src/fn.js"), "const run = () => {};\n")
            .unwrap();
        assert_eq!(syms.len(), 1, "no Const+Function double emit: {syms:?}");
        assert_eq!(syms[0].kind, SymbolKind::Function);
    }

    #[test]
    fn value_positions_read_bare_identifiers_once() {
        let plugin = JavaScriptPlugin;
        let refs = plugin
            .extract_references(
                Path::new("src/vals.js"),
                "const G = 1;\nconst H = 2;\nfunction use() {\n  return G;\n}\nconst t = G;\nconst sum = G + H;\nconst neg = !G;\nconst pick = G ? H : 0;\nconst arr = [G];\nconst cp = [...G];\nasync function w() { await G; }\nfunction* g() { yield G; }\nfunction t2() { throw G; }\nconst p = (G);\nconst s = (G, 0);\nlet m = 0;\nm = G;\nG += H;\nG++;\nconst o = { k: G };\nconst first = G[H];\nfor (const x of G) {}\nswitch (G) { case H: break; }\n",
            )
            .unwrap();
        let named = |n: &str| -> Vec<_> { refs.iter().filter(|r| r.name == n).collect() };
        // return, init, binary, unary, ternary, array, spread, await, yield,
        // throw, parens, sequence, assign-RHS, aug-LHS, update, pair-value,
        // subscript-object, for-in-iterable, switch discriminant — each
        // exactly once.
        let g = named("G");
        assert_eq!(g.len(), 19, "all G reads, no doubles: {g:?}");
        assert!(g.iter().all(|r| r.kind == ReferenceKind::Value));
        // Binary-RHS, ternary-alternative, aug-RHS, subscript-index,
        // switch-case value.
        let h = named("H");
        assert_eq!(h.len(), 5, "all H reads: {h:?}");
        // Writes, bindings, and labels are never reads.
        assert!(named("m").is_empty(), "assign-LHS is a write");
        assert!(named("k").is_empty(), "pair keys are labels");
        assert!(named("x").is_empty(), "for-targets are bindings");
        assert!(named("t").is_empty(), "bound names are definitions");
        // No strays anywhere: 19 + 5 rows total.
        assert_eq!(refs.len(), 24, "unexpected rows: {refs:?}");
        // Container attribution survives: the return sits in `use`.
        assert!(g.iter().any(|r| r.container == "src/vals::use"));
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
    fn method_calls_keep_receiver_qualifier() {
        let plugin = JavaScriptPlugin;
        let refs = plugin
            .extract_references(test_path(), "ns.serve();\ndb.get();\n")
            .unwrap();
        let methods: Vec<(&str, &str)> = refs
            .iter()
            .filter(|r| r.kind == ReferenceKind::Method)
            .map(|r| (r.name.as_str(), r.qualifier.as_str()))
            .collect();
        assert_eq!(methods, vec![("serve", "ns"), ("get", "db")]);
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

    #[test]
    fn extracts_argument_value_references() {
        let plugin = JavaScriptPlugin;
        let src = "run(callbackFn, { key: value });\nnew Widget(helper);\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();
        let has = |name: &str, kind: ReferenceKind| {
            refs.iter().any(|r| r.name == name && r.kind == kind)
        };

        assert!(has("callbackFn", ReferenceKind::Value));
        assert!(has("value", ReferenceKind::Value));
        assert!(!refs.iter().any(|r| r.name == "key"));
        assert!(has("helper", ReferenceKind::Value));
        assert!(has("Widget", ReferenceKind::Call));
    }

    #[test]
    fn method_receiver_is_value_reference() {
        let plugin = JavaScriptPlugin;
        let src = "function f(svc) { svc.refresh(); }\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();
        let has = |name: &str, kind: ReferenceKind| {
            refs.iter().any(|r| r.name == name && r.kind == kind)
        };
        assert!(has("refresh", ReferenceKind::Method));
        assert!(has("svc", ReferenceKind::Value));
    }

    #[test]
    fn extract_impls_records_member_final_segments() {
        let plugin = JavaScriptPlugin;
        let src = "class C extends Base {}\nclass D extends a.b {}\nclass E extends mixin(F) {}\nclass Plain {}\n";
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
        // Member bases count by final segment; call-form heritage
        // stays out (the mixin result, not its argument, is the base).
        assert_eq!(pairs, vec![("C", "Base"), ("D", "b")]);
    }

    #[test]
    fn extract_impls_names_class_expressions_by_declarator() {
        let plugin = JavaScriptPlugin;
        let src = "const X = class extends Base {}\nconst Y = class Named extends Base2 {}\nmodule.exports.Anon = class extends Base3 {}\nexport default class extends Base4 {}\n";
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
        assert_eq!(
            pairs,
            vec![("X", "Base"), ("Named", "Base2"), ("Anon", "Base3")]
        );
    }

    #[test]
    fn jsx_elements_emit_component_calls_not_host_tags() {
        let plugin = JavaScriptPlugin;
        let src = "function App() {\n  return <div><Widget title=\"hi\" /><Foo.Bar /></div>;\n}\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();

        // One call per component use: the closing tag must not double-emit.
        assert_eq!(
            refs.iter().filter(|r| r.name == "Widget").count(),
            1,
            "refs: {refs:?}"
        );
        assert!(refs
            .iter()
            .any(|r| r.name == "Widget" && r.kind == ReferenceKind::Call));
        // Member tags resolve like member calls, keeping the object qualifier.
        let member = refs
            .iter()
            .find(|r| r.name == "Bar" && r.kind == ReferenceKind::Method)
            .expect("Foo.Bar method ref");
        assert_eq!(member.qualifier, "Foo");
        // Lowercase host elements (`div`, `h1`) are not symbol references.
        assert!(!refs.iter().any(|r| r.name == "div"));
    }

    #[test]
    fn decorators_emit_references_scoped_to_decorated_def() {
        let plugin = JavaScriptPlugin;
        let src = "function Logged(t) { return t; }\n@Logged\nclass Service {\n  run() {}\n}\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();

        let logged: Vec<&Reference> = refs.iter().filter(|r| r.name == "Logged").collect();
        assert_eq!(logged.len(), 1, "refs: {refs:?}");
        assert_eq!(logged[0].kind, ReferenceKind::Call);
        assert_eq!(logged[0].container, "src/auth/service::Service");
    }

    #[test]
    fn heritage_emits_type_use_scoped_to_subclass() {
        let plugin = JavaScriptPlugin;
        let src = "class Base {}\nclass Child extends Base {}\nclass Mixed extends mixin(Base) {}\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();

        let base: Vec<&Reference> = refs.iter().filter(|r| r.name == "Base").collect();
        // `extends Base` is one Type use; `mixin(Base)` keeps call rules
        // (Call `mixin`, Value `Base`), never a Type row.
        assert_eq!(base.len(), 2, "refs: {refs:?}");
        let heritage = base.iter().find(|r| r.kind == ReferenceKind::Type).expect("heritage Type");
        assert_eq!(heritage.container, "src/auth/service::Child");
        assert!(base.iter().any(|r| r.kind == ReferenceKind::Value));
        assert!(refs.iter().any(|r| r.name == "mixin" && r.kind == ReferenceKind::Call));
    }

    #[test]
    fn shorthand_properties_emit_value_reads_not_bindings() {
        let plugin = JavaScriptPlugin;
        let src = "function build(handler) {\n  const { bound } = load();\n  return { handler, key: bound };\n}\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();

        let handler: Vec<&Reference> = refs.iter().filter(|r| r.name == "handler").collect();
        assert_eq!(handler.len(), 1, "refs: {refs:?}");
        assert_eq!(handler[0].kind, ReferenceKind::Value);
        // Destructuring targets are bindings, not reads.
        assert!(!refs.iter().any(|r| r.name == "bound" && r.start_line == 2));
    }

    #[test]
    fn member_reads_emit_receivers_once() {
        let plugin = JavaScriptPlugin;
        let src = "function total(items) {\n  return items.length;\n}\nfunction chain(a) {\n  return a.b.c();\n}\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();

        let items: Vec<&Reference> = refs.iter().filter(|r| r.name == "items").collect();
        assert_eq!(items.len(), 1, "refs: {refs:?}");
        assert_eq!(items[0].kind, ReferenceKind::Value);
        // Chains read their base once; the callee keeps call rules.
        let a: Vec<&Reference> = refs.iter().filter(|r| r.name == "a").collect();
        assert_eq!(a.len(), 1, "refs: {refs:?}");
        assert!(refs.iter().any(|r| r.name == "c" && r.kind == ReferenceKind::Method));
    }

    #[test]
    fn template_substitutions_emit_direct_identifier_reads() {
        let plugin = JavaScriptPlugin;
        let src = "function greet(name) {\n  return `hello ${name}, ${format(name)}!`;\n}\n";
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
        let plugin = JavaScriptPlugin;
        let syms = plugin
            .extract_symbols(
                Path::new("src/members.js"),
                "class Config {\n  port = 8080;\n  #secret = 1;\n  serve() {}\n}\n",
            )
            .unwrap();
        let find = |n: &str| syms.iter().find(|s| s.name == n).cloned();
        assert_eq!(find("port").unwrap().kind, SymbolKind::Other("field".into()));
        assert_eq!(find("port").unwrap().start_line, 2);
        assert_eq!(find("serve").unwrap().kind, SymbolKind::Function);
        assert!(find("#secret").is_none());
    }

    #[test]
    fn member_reads_emit_properties_once_with_qualifier() {
        let plugin = JavaScriptPlugin;
        let src = "function f(cfg) {\n  const p = cfg.port;\n  cfg.port = 1;\n  cfg.count += 1;\n  delete cfg.gone;\n  const q = cfg.a.b;\n  cfg.serve();\n}\n";
        let refs = plugin.extract_references(Path::new("src/uses.js"), src).unwrap();
        let named = |n: &str| -> Vec<_> { refs.iter().filter(|r| r.name == n).collect() };

        let port = named("port");
        assert_eq!(port.len(), 1, "refs: {refs:?}");
        assert_eq!(port[0].kind, ReferenceKind::Value);
        assert_eq!(port[0].qualifier, "cfg");
        assert_eq!(named("count").len(), 1);
        assert_eq!(named("a").len(), 1);
        assert_eq!(named("a")[0].qualifier, "cfg");
        assert_eq!(named("b").len(), 1);
        assert_eq!(named("b")[0].qualifier, "cfg.a");
        assert!(named("gone").is_empty(), "delete is not a read");
        assert_eq!(named("serve").len(), 1);
        assert_eq!(named("serve")[0].kind, ReferenceKind::Method);
        assert_eq!(named("cfg").len(), 6, "refs: {refs:?}");
        // No strays: 6 receivers + port/count/a/b + serve.
        assert_eq!(refs.len(), 11, "unexpected rows: {refs:?}");
    }

    #[test]
    fn member_writes_in_destructuring_targets_stay_out() {
        let plugin = JavaScriptPlugin;
        let src = "function f(a, v) {\n  [a.b, c] = v;\n  [x2 = a.d] = v;\n}\n";
        let refs = plugin.extract_references(Path::new("src/destructure.js"), src).unwrap();
        let named = |n: &str| -> Vec<_> { refs.iter().filter(|r| r.name == n).collect() };

        assert!(named("b").is_empty(), "array target is a write: {refs:?}");
        assert_eq!(named("d").len(), 1);
        assert_eq!(named("a").len(), 2, "refs: {refs:?}");
        assert_eq!(named("v").len(), 2);
        assert_eq!(refs.len(), 5, "unexpected rows: {refs:?}");
    }

    #[test]
    fn export_specifiers_read_local_names_not_aliases() {
        let plugin = JavaScriptPlugin;
        let src = "function helper() {}\nconst count = 0;\nexport { helper, count as total };\nexport { helper as helper2 } from \"./other.js\";\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();
        let named = |n: &str| -> Vec<_> { refs.iter().filter(|r| r.name == n).collect() };

        assert_eq!(named("helper").len(), 2, "refs: {refs:?}");
        assert!(named("helper").iter().all(|r| r.kind == ReferenceKind::Value));
        assert_eq!(named("count").len(), 1);
        // Aliases name the export, never the local.
        assert!(named("total").is_empty());
        assert!(named("helper2").is_empty());
        assert_eq!(refs.len(), 3, "unexpected rows: {refs:?}");
    }

    #[test]
    fn export_default_reads_local_names() {
        let plugin = JavaScriptPlugin;
        let refs = plugin
            .extract_references(
                test_path(),
                "function helper() { return 1; }\nexport default helper;\nexport const x = 1;\nexport default function direct() {}\n",
            )
            .unwrap();
        let named = |n: &str| -> Vec<_> { refs.iter().filter(|r| r.name == n).collect() };

        assert_eq!(named("helper").len(), 1, "refs: {refs:?}");
        assert_eq!(named("helper")[0].kind, ReferenceKind::Value);
        // Declarations bind, never read.
        assert!(named("x").is_empty(), "refs: {refs:?}");
        assert!(named("direct").is_empty(), "refs: {refs:?}");
        assert_eq!(refs.len(), 1, "unexpected rows: {refs:?}");
    }

    #[test]
    fn default_values_and_field_initializers_read() {
        let plugin = JavaScriptPlugin;
        let src = "const f = (a = d) => a;\nclass C {\n  x = v;\n  m(p = w) { return p; }\n}\nconst [r = e] = arr;\n[q = g] = arr2;\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();
        let named = |n: &str| -> Vec<_> { refs.iter().filter(|r| r.name == n).collect() };

        for d in ["d", "v", "w", "e", "arr", "g", "arr2"] {
            assert_eq!(named(d).len(), 1, "{d}: {refs:?}");
            assert_eq!(named(d)[0].kind, ReferenceKind::Value);
        }
        // Bound names and assignment targets bind, never read (`a`
        // and `p` read once at their returns).
        for bound in ["f", "x", "r", "q"] {
            assert!(named(bound).is_empty(), "{bound}: {refs:?}");
        }
        assert_eq!(named("a").len(), 1, "refs: {refs:?}");
        assert_eq!(named("p").len(), 1, "refs: {refs:?}");
    }

    #[test]
    fn jsx_expressions_read_bare_identifiers_once() {
        let plugin = JavaScriptPlugin;
        let src = "import { W } from \"./w\";\nfunction render(limit, other) {\n  return <W timeout={limit} size={limit * 2} label={other.name}>{limit}</W>;\n}\n";
        let refs = plugin.extract_references(Path::new("src/render.jsx"), src).unwrap();
        let named = |n: &str| -> Vec<_> { refs.iter().filter(|r| r.name == n).collect() };

        assert_eq!(named("limit").len(), 3, "refs: {refs:?}");
        assert!(named("limit").iter().all(|r| r.kind == ReferenceKind::Value));
        // Attribute names are markup, never reads.
        assert!(named("timeout").is_empty());
        assert!(named("size").is_empty());
        assert!(named("label").is_empty());
        // Member reads keep their rule inside expressions.
        assert_eq!(named("name").len(), 1);
        assert_eq!(named("other").len(), 1);
        assert_eq!(named("W").len(), 1);
        assert_eq!(named("W")[0].kind, ReferenceKind::Call);
        // No strays: 3 limits + other + name + the component call.
        assert_eq!(refs.len(), 6, "unexpected rows: {refs:?}");
    }
}
