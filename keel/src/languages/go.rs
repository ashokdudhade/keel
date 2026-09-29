//! Go language plugin: Tree-sitter based symbol and reference extraction.
//!
//! `module_path` is the package name from the file's `package` clause (e.g.
//! `auth`). For `package main`, a path-based identity is used so mains in
//! different files do not collide. Go has no `impl Trait for Type` form,
//! so [`LanguagePlugin::extract_impls`] records only explicit blank
//! assertions (`var _ io.Reader = MyReader{}`); structural method-set
//! matching is future work.

use super::{file_path_key, path_module_identity, DepthGuard, LanguagePlugin, WalkBudget, MAX_WALK_DEPTH};
use crate::error::{Result, KeelError};
use crate::graph::types::{ImplRecord, Import, Reference, ReferenceKind, Symbol, SymbolKind};
use std::path::{Path, PathBuf};
use tree_sitter::{Node, Parser, Tree};

/// Extractor for Go source using Tree-sitter.
pub struct GoPlugin;

impl GoPlugin {
    fn parse(source: &str) -> Result<Tree> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .map_err(|e| KeelError::TreeSitter(e.to_string()))?;
        parser.parse(source, None).ok_or(KeelError::Parse)
    }
}

impl LanguagePlugin for GoPlugin {
    fn has_syntax_errors(&self, source_code: &str) -> bool {
        Self::parse(source_code)
            .map(|tree| tree.root_node().has_error())
            .unwrap_or(false)
    }
    fn extensions(&self) -> &[&str] {
        &["go"]
    }

    fn extract_symbols(&self, path: &Path, source_code: &str) -> Result<Vec<Symbol>> {
        let tree = Self::parse(source_code)?;
        let src = source_code.as_bytes();
        let package = resolve_module_path(path, tree.root_node(), src)?;
        let mut out = Vec::new();
        let budget = WalkBudget::new();
        walk_symbols(tree.root_node(), src, &package, "", &mut out, &budget)?;
        Ok(out)
    }

    fn extract_references(&self, path: &Path, source_code: &str) -> Result<Vec<Reference>> {
        let tree = Self::parse(source_code)?;
        let src = source_code.as_bytes();
        let package = resolve_module_path(path, tree.root_node(), src)?;
        let file_key = file_path_key(path);
        let mut scope: Vec<String> = Vec::new();
        let mut out = Vec::new();
        let budget = WalkBudget::new();
        walk_references(
            tree.root_node(),
            src,
            &file_key,
            &package,
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
        let mut out = Vec::new();
        let budget = WalkBudget::new();
        walk_impls(tree.root_node(), src, &mut out, &budget)?;
        Ok(out)
    }
}

/// Collect (type, interface) pairs from explicit blank assertions
/// (`var _ io.Reader = MyReader{}`, `var _ I = (*T)(nil)`).
///
/// Only the blank name counts: a named `var w Widget = Widget{}` is an
/// initialization, not an implementation claim. Values that are plain
/// identifiers or constructor calls carry no provable type and stay out.
fn walk_impls(
    node: Node,
    src: &[u8],
    out: &mut Vec<ImplRecord>,
    budget: &WalkBudget,
) -> Result<()> {
    let _guard = DepthGuard::enter(budget)
        .ok_or_else(|| KeelError::TooDeeplyNested { limit: MAX_WALK_DEPTH })?;
    if node.kind() == "var_spec" {
        if let (Some(name), Some(ty), Some(value)) = (
            node.child_by_field_name("name"),
            node.child_by_field_name("type"),
            node.child_by_field_name("value"),
        ) {
            if name.kind() == "identifier" && node_text(name, src)? == "_" {
                if let (Some(trait_name), Some(type_name)) = (
                    concrete_type_name(ty, src, budget)?,
                    assertion_value_type(value, src, budget)?,
                ) {
                    let pos = node.start_position();
                    out.push(ImplRecord {
                        type_name,
                        trait_name: Some(trait_name),
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

/// Final type name of an assertion side: a bare name, the `name` of a
/// qualified type (`io.Reader` → `Reader`), the `type` of a generic
/// instantiation (`Store[T]` → `Store`), through parens and pointers.
fn concrete_type_name(node: Node, src: &[u8], budget: &WalkBudget) -> Result<Option<String>> {
    let _guard = DepthGuard::enter(budget)
        .ok_or_else(|| KeelError::TooDeeplyNested { limit: MAX_WALK_DEPTH })?;
    match node.kind() {
        "identifier" | "type_identifier" => Ok(Some(node_text(node, src)?.to_string())),
        "qualified_type" => match node.child_by_field_name("name") {
            Some(n) if matches!(n.kind(), "type_identifier" | "identifier") => {
                Ok(Some(node_text(n, src)?.to_string()))
            }
            _ => Ok(None),
        },
        "selector_expression" => match node.child_by_field_name("field") {
            Some(f) if matches!(f.kind(), "field_identifier" | "identifier") => {
                Ok(Some(node_text(f, src)?.to_string()))
            }
            _ => Ok(None),
        },
        "generic_type" => match node.child_by_field_name("type") {
            Some(t) => concrete_type_name(t, src, budget),
            None => Ok(None),
        },
        "index_expression" => match node.child_by_field_name("operand") {
            Some(o) => concrete_type_name(o, src, budget),
            None => Ok(None),
        },
        "parenthesized_expression" | "parenthesized_type" | "pointer_type" => {
            let mut cursor = node.walk();
            let mut named = node.children(&mut cursor).filter(|c| c.is_named());
            match (named.next(), named.next()) {
                (Some(only), None) => concrete_type_name(only, src, budget),
                _ => Ok(None),
            }
        }
        "unary_expression" => match node.child_by_field_name("operand") {
            Some(o) => concrete_type_name(o, src, budget),
            None => Ok(None),
        },
        _ => Ok(None),
    }
}

/// Provable concrete type of an assertion value: a composite literal
/// (`T{}`, `&T{}`, through parens), or a parenthesized conversion
/// (`(*T)(nil)` — the function of a conversion is a type by syntax).
/// Bare values, derefs, and constructor calls prove nothing and stay out.
fn assertion_value_type(node: Node, src: &[u8], budget: &WalkBudget) -> Result<Option<String>> {
    let _guard = DepthGuard::enter(budget)
        .ok_or_else(|| KeelError::TooDeeplyNested { limit: MAX_WALK_DEPTH })?;
    match node.kind() {
        "composite_literal" => match node.child_by_field_name("type") {
            Some(t) => concrete_type_name(t, src, budget),
            None => Ok(None),
        },
        "unary_expression" => match node.child_by_field_name("operand") {
            Some(o) if o.kind() == "composite_literal" => assertion_value_type(o, src, budget),
            _ => Ok(None),
        },
        "parenthesized_expression" => {
            let mut cursor = node.walk();
            let mut named = node.children(&mut cursor).filter(|c| c.is_named());
            match (named.next(), named.next()) {
                (Some(only), None) => assertion_value_type(only, src, budget),
                _ => Ok(None),
            }
        }
        "call_expression" => match node.child_by_field_name("function") {
            Some(f) if f.kind() == "parenthesized_expression" => concrete_type_name(f, src, budget),
            _ => Ok(None),
        },
        // Assertion values sit under one `expression_list`; multi-value
        // lists prove nothing about any single type.
        "expression_list" => {
            let mut cursor = node.walk();
            let mut named = node.children(&mut cursor).filter(|c| c.is_named());
            match (named.next(), named.next()) {
                (Some(only), None) => assertion_value_type(only, src, budget),
                _ => Ok(None),
            }
        }
        _ => Ok(None),
    }
}

fn resolve_module_path(path: &Path, root: Node, src: &[u8]) -> Result<String> {
    let package = package_name(root, src)?.unwrap_or_default();
    if package == "main" || package.is_empty() {
        Ok(path_module_identity(path))
    } else {
        Ok(package)
    }
}

fn node_text<'a>(node: Node, src: &'a [u8]) -> Result<&'a str> {
    node.utf8_text(src)
        .map_err(|e| KeelError::TreeSitter(e.to_string()))
}

fn package_name(root: Node, src: &[u8]) -> Result<Option<String>> {
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() == "package_clause" {
            let mut pc = child.walk();
            for id in child.named_children(&mut pc) {
                if id.kind() == "package_identifier" {
                    return Ok(Some(node_text(id, src)?.to_string()));
                }
            }
        }
    }
    Ok(None)
}

fn qualify_scope(file_key: &str, package: &str, scope: &[String]) -> String {
    if scope.is_empty() {
        file_key.to_string()
    } else if package.is_empty() {
        scope.join("::")
    } else {
        format!("{package}::{}", scope.join("::"))
    }
}

fn walk_symbols(
    node: Node,
    src: &[u8],
    package: &str,
    container: &str,
    out: &mut Vec<Symbol>,
    budget: &WalkBudget,
) -> Result<()> {
    let _guard = DepthGuard::enter(budget)
        .ok_or_else(|| KeelError::TooDeeplyNested { limit: MAX_WALK_DEPTH })?;
    match node.kind() {
        "function_declaration" => {
            emit_named_symbol(node, SymbolKind::Function, package, "", out, src)?;
        }
        "method_declaration" => {
            // Methods are top-level; the container is the receiver type.
            let receiver = method_receiver(node, src, budget)?.unwrap_or_default();
            emit_named_symbol(node, SymbolKind::Function, package, &receiver, out, src)?;
        }
        // Interface method specs (`Do(x int) int` in `type Doer
        // interface`): same Function shape as methods, so outline and
        // definition find interface members. Embedded interface names
        // are `type_elem` children, never `method_elem`, so they stay
        // out.
        "method_elem" => {
            emit_named_symbol(node, SymbolKind::Function, package, container, out, src)?;
        }
        "type_spec" => {
            emit_type_spec(node, package, container, out, src)?;
        }
        "type_alias" => {
            // `type A = B`
            if let Some(name) = node.child_by_field_name("name") {
                push_symbol(
                    name,
                    SymbolKind::Other("type".into()),
                    package,
                    container,
                    out,
                    src,
                )?;
            }
        }
        "const_spec" | "var_spec" => {
            emit_const_var_names(node, package, container, out, src)?;
        }
        // Struct fields are indexed items (`field` kind), so selector
        // reads resolve to a definition. Every direct
        // `field_identifier` child is a name (`X, Y int`); embedded
        // fields carry a type instead and stay out.
        "field_declaration" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "field_identifier" {
                    push_symbol(
                        child,
                        SymbolKind::Other("field".into()),
                        package,
                        container,
                        out,
                        src,
                    )?;
                }
            }
        }
        _ => {}
    }
    // Members of a struct/interface body carry the type name; anything
    // else inherits the enclosing container unchanged.
    let owned: Option<String> = match node.kind() {
        "type_spec" => match (
            node.child_by_field_name("name"),
            node.child_by_field_name("type"),
        ) {
            (Some(name), Some(ty))
                if name.kind() == "type_identifier"
                    && matches!(ty.kind(), "struct_type" | "interface_type") =>
            {
                Some(node_text(name, src)?.to_string())
            }
            _ => None,
        },
        _ => None,
    };
    let next_container = owned.as_deref().unwrap_or(container);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_symbols(child, src, package, next_container, out, budget)?;
    }
    Ok(())
}

/// Receiver base type of a `method_declaration` (`*User` → `User`,
/// through generics and qualification), or `None` when unresolvable.
fn method_receiver(node: Node, src: &[u8], budget: &WalkBudget) -> Result<Option<String>> {
    let Some(receiver) = node.child_by_field_name("receiver") else {
        return Ok(None);
    };
    let mut cursor = receiver.walk();
    let Some(first) = receiver
        .children(&mut cursor)
        .find(|c| c.kind() == "parameter_declaration")
    else {
        return Ok(None);
    };
    match first.child_by_field_name("type") {
        Some(ty) => concrete_type_name(ty, src, budget),
        None => Ok(None),
    }
}

fn emit_type_spec(
    node: Node,
    package: &str,
    container: &str,
    out: &mut Vec<Symbol>,
    src: &[u8],
) -> Result<()> {
    let Some(name) = node.child_by_field_name("name") else {
        return Ok(());
    };
    let kind = match node.child_by_field_name("type") {
        Some(ty) if ty.kind() == "struct_type" => SymbolKind::Struct,
        Some(ty) if ty.kind() == "interface_type" => SymbolKind::Trait,
        _ => SymbolKind::Other("type".into()),
    };
    push_symbol(name, kind, package, container, out, src)
}

/// Emit names from a `const_spec` or `var_spec` (same shape: `name`
/// field plus pre-value/pre-type identifiers). Module-level variables
/// share the `Const` kind, matching Python's module constants.
fn emit_const_var_names(
    node: Node,
    package: &str,
    container: &str,
    out: &mut Vec<Symbol>,
    src: &[u8],
) -> Result<()> {
    // `name` field may list multiple identifiers (`const a, b = …`).
    if let Some(name_field) = node.child_by_field_name("name") {
        if name_field.kind() == "identifier" {
            push_symbol(name_field, SymbolKind::Const, package, container, out, src)?;
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "identifier" && child != name_field {
                let before_value = match node.child_by_field_name("value") {
                    Some(v) => child.start_byte() < v.start_byte(),
                    None => true,
                };
                let before_type = match node.child_by_field_name("type") {
                    Some(t) => child.start_byte() < t.start_byte(),
                    None => true,
                };
                if before_value && before_type {
                    push_symbol(child, SymbolKind::Const, package, container, out, src)?;
                }
            }
        }
    }
    Ok(())
}

fn emit_named_symbol(
    node: Node,
    kind: SymbolKind,
    package: &str,
    container: &str,
    out: &mut Vec<Symbol>,
    src: &[u8],
) -> Result<()> {
    if let Some(name) = node.child_by_field_name("name") {
        if matches!(name.kind(), "identifier" | "field_identifier") {
            push_symbol(name, kind, package, container, out, src)?;
        }
    }
    Ok(())
}

fn push_symbol(
    name_node: Node,
    kind: SymbolKind,
    package: &str,
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
        module_path: package.to_string(),
        container: container.to_string(),
    });
    Ok(())
}

fn walk_references(
    node: Node,
    src: &[u8],
    file_key: &str,
    package: &str,
    scope: &mut Vec<String>,
    out: &mut Vec<Reference>,
    budget: &WalkBudget,
) -> Result<()> {
    let _guard = DepthGuard::enter(budget)
        .ok_or_else(|| KeelError::TooDeeplyNested { limit: MAX_WALK_DEPTH })?;
    match node.kind() {
        "function_declaration" | "method_declaration" | "method_elem" => {
            if let Some(name) = node.child_by_field_name("name") {
                if matches!(name.kind(), "identifier" | "field_identifier") {
                    let text = node_text(name, src)?;
                    scope.push(text.to_string());
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        walk_references(child, src, file_key, package, scope, out, budget)?;
                    }
                    scope.pop();
                    return Ok(());
                }
            }
        }
        "call_expression" => {
            if let Some(func) = node.child_by_field_name("function") {
                emit_call_reference(func, src, file_key, package, scope, out)?;
            }
            if let Some(args) = node.child_by_field_name("arguments") {
                emit_argument_values(args, src, file_key, package, scope, out)?;
            }
        }
        // Selector reads (`user.Name`) read the receiver, mirroring the
        // receiver rule for calls, and the field itself. Call callees
        // are owned by the call arm and skipped so no site emits
        // twice; fields in plain-assignment/range/receive LHS position
        // (through multi-target lists) are writes, never reads.
        "selector_expression" => {
            emit_selector_receiver(node, src, file_key, package, scope, out)?;
            emit_selector_field(node, src, file_key, package, scope, out)?;
        }
        // Type uses (`var w Widget`, `*Widget` results, `v.(Widget)`,
        // `Widget{...}` literals): the only `type_identifier` definition
        // site is `type_spec.name`; conversions (`Widget(x)`) parse as
        // calls and stay owned by the `call_expression` arm.
        "type_identifier" => {
            emit_type_reference(node, src, file_key, package, scope, out)?;
        }
        // Value reads in statement/expression positions (`return x`,
        // `a + b`, `-x`, `(x)`, `n++`, composite elements, spread
        // operands): only DIRECT identifier children emit here; nested
        // composites fall through to the recursion below so each site
        // emits exactly once.
        "binary_expression" | "unary_expression" | "parenthesized_expression"
        | "inc_statement" | "dec_statement" | "variadic_argument" => {
            emit_direct_identifiers(node, src, file_key, package, scope, out)?;
        }
        // `return` operands always sit under one `expression_list`.
        "return_statement" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                emit_wrapped_identifier(child, "expression_list", src, file_key, package, scope, out)?;
            }
        }
        // Composite elements wrap one `literal_element` level
        // (`[]int{G}`); nested literals recurse normally.
        "literal_value" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                emit_wrapped_identifier(child, "literal_element", src, file_key, package, scope, out)?;
            }
        }
        // Conditions and switch subjects read (`if x`, `for` conditions,
        // `switch x`); initializers and bindings recurse into their own
        // arms. (`for cond {}` subjects recurse into the binary arm.)
        "if_statement" | "for_clause" => {
            emit_field_identifier(node, "condition", src, file_key, package, scope, out)?;
        }
        "expression_switch_statement" | "type_switch_statement" => {
            emit_field_identifier(node, "value", src, file_key, package, scope, out)?;
        }
        // Index/slice sides and channel operands read (`a[i]`,
        // `s[lo:hi]`, `ch <- v`, `v.(T)` operands); the asserted type
        // owns itself via the type arm.
        "type_assertion_expression" => {
            emit_field_identifier(node, "operand", src, file_key, package, scope, out)?;
        }
        "index_expression" => {
            emit_field_identifier(node, "operand", src, file_key, package, scope, out)?;
            emit_field_identifier(node, "index", src, file_key, package, scope, out)?;
        }
        "slice_expression" => {
            emit_field_identifier(node, "operand", src, file_key, package, scope, out)?;
            emit_field_identifier(node, "start", src, file_key, package, scope, out)?;
            emit_field_identifier(node, "end", src, file_key, package, scope, out)?;
            emit_field_identifier(node, "capacity", src, file_key, package, scope, out)?;
        }
        "send_statement" => {
            emit_field_identifier(node, "channel", src, file_key, package, scope, out)?;
            emit_field_identifier(node, "value", src, file_key, package, scope, out)?;
        }
        // `for k, v := range ITEMS` reads the iterable; targets bind.
        "range_clause" => {
            emit_field_identifier(node, "right", src, file_key, package, scope, out)?;
        }
        // Composite keys and values (`Point{X: v}`), through the same
        // `literal_element` wrapper. Bare-identifier keys read: field
        // names in struct literals, variables in map literals. String
        // keys and composite keys hold no direct identifier and stay
        // out (nested calls still emit via the recursion below).
        "keyed_element" => {
            if let Some(key) = node.child_by_field_name("key") {
                emit_wrapped_identifier(key, "literal_element", src, file_key, package, scope, out)?;
            }
            if let Some(value) = node.child_by_field_name("value") {
                emit_wrapped_identifier(value, "literal_element", src, file_key, package, scope, out)?;
            }
        }
        // `case` values, `var`/`const` initializers, and `:=` right
        // sides read; each list may hold several values.
        "expression_case" | "var_spec" | "const_spec" => {
            emit_field_list_identifiers(node, "value", src, file_key, package, scope, out)?;
        }
        "short_var_declaration" => {
            emit_field_list_identifiers(node, "right", src, file_key, package, scope, out)?;
        }
        // Plain assignment reads only the RHS (`x = v`); the LHS is a
        // write, never a read. Compound assignment (`x += v`) reads its
        // target too. (Indexed LHS addresses recurse into the index arm.)
        "assignment_statement" => {
            emit_field_list_identifiers(node, "right", src, file_key, package, scope, out)?;
            if is_compound_assignment(node, src)? {
                emit_field_list_identifiers(node, "left", src, file_key, package, scope, out)?;
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_references(child, src, file_key, package, scope, out, budget)?;
    }
    Ok(())
}

fn emit_call_reference(
    func: Node,
    src: &[u8],
    file_key: &str,
    package: &str,
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
                package,
                scope,
                out,
            );
        }
        "selector_expression" => {
            if let Some(field) = func.child_by_field_name("field") {
                if matches!(field.kind(), "field_identifier" | "identifier") {
                    let text = node_text(field, src)?;
                    let qualifier = func
                        .child_by_field_name("operand")
                        .map(|o| node_text(o, src))
                        .transpose()?
                        .unwrap_or("")
                        .to_string();
                    push_method_reference(
                        text.to_string(),
                        field,
                        qualifier,
                        file_key,
                        package,
                        scope,
                        out,
                    );
                }
            }
            // The receiver (`db` in `db.Get()`) is a value use.
            if let Some(recv) = func.child_by_field_name("operand") {
                if recv.kind() == "identifier" {
                    push_reference(
                        node_text(recv, src)?.to_string(),
                        recv,
                        ReferenceKind::Value,
                        file_key,
                        package,
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

/// Emit a type use, keeping the package qualifier for `pkg.Type` so the
/// resolver can break ties exactly like qualified calls.
fn emit_type_reference(
    node: Node,
    src: &[u8],
    file_key: &str,
    package: &str,
    scope: &[String],
    out: &mut Vec<Reference>,
) -> Result<()> {
    if let Some(parent) = node.parent() {
        if parent.kind() == "type_spec"
            && parent.child_by_field_name("name").is_some_and(|n| n.id() == node.id())
        {
            return Ok(());
        }
        if parent.kind() == "call_expression"
            && parent.child_by_field_name("function").is_some_and(|n| n.id() == node.id())
        {
            return Ok(());
        }
        let qualifier = if parent.kind() == "qualified_type" {
            parent
                .child_by_field_name("package")
                .map(|p| node_text(p, src))
                .transpose()?
                .unwrap_or("")
                .to_string()
        } else {
            String::new()
        };
        push_type_reference(
            node_text(node, src)?.to_string(),
            node,
            qualifier,
            file_key,
            package,
            scope,
            out,
        );
    }
    Ok(())
}

/// Push a type-use reference with an explicit qualifier (`pkg` in `pkg.Type`).
fn push_type_reference(
    name: String,
    node: Node,
    qualifier: String,
    file_key: &str,
    package: &str,
    scope: &[String],
    out: &mut Vec<Reference>,
) {
    let pos = node.start_position();
    out.push(Reference {
        name,
        file: PathBuf::new(),
        start_line: pos.row as u32 + 1,
        start_col: pos.column as u32 + 1,
        kind: ReferenceKind::Type,
        container: qualify_scope(file_key, package, scope),
        qualifier,
    });
}

fn push_reference(
    name: String,
    node: Node,
    kind: ReferenceKind,
    file_key: &str,
    package: &str,
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
        container: qualify_scope(file_key, package, scope),
        qualifier: String::new(),
    });
}

/// Push a value reference with an explicit qualifier (the operand text
/// for selector reads, `user` in `user.Name`).
fn push_value_reference(
    name: String,
    node: Node,
    qualifier: String,
    file_key: &str,
    package: &str,
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
        container: qualify_scope(file_key, package, scope),
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
    package: &str,
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
        container: qualify_scope(file_key, package, scope),
        qualifier,
    });
}

/// Emit the receiver of a value-position selector read (`user` in
/// `user.Name`). Call callees are owned by the call arm and skipped; the
/// check is direct so chains still read their base.
fn emit_selector_receiver(
    node: Node,
    src: &[u8],
    file_key: &str,
    package: &str,
    scope: &[String],
    out: &mut Vec<Reference>,
) -> Result<()> {
    let Some(recv) = node.child_by_field_name("operand") else {
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
        package,
        scope,
        out,
    );
    Ok(())
}

/// Emit the field of a value-position selector read (`Name` in
/// `user.Name`) as a Value reference, keeping the operand text as the
/// qualifier exactly like method calls. Call callees are owned by the
/// call arm; fields in plain-assignment/range/receive LHS position
/// (through multi-target lists) are writes. Compound targets and
/// inc/dec targets read, so they emit.
fn emit_selector_field(
    node: Node,
    src: &[u8],
    file_key: &str,
    package: &str,
    scope: &[String],
    out: &mut Vec<Reference>,
) -> Result<()> {
    let Some(field) = node.child_by_field_name("field") else {
        return Ok(());
    };
    if field.kind() != "field_identifier" {
        return Ok(());
    }
    let Some(operand) = node.child_by_field_name("operand") else {
        return Ok(());
    };
    // Call callees are owned by the call arm; the check is direct so
    // chains still read their middle (`a.B.C()` reads `B` via the
    // inner selector).
    if node.parent().is_some_and(|p| p.kind() == "call_expression") {
        return Ok(());
    }
    // Plain-assignment, range, and receive LHS positions write the
    // field; multi-targets nest one expression list level. Compound
    // assignment (`+=`) shares the node kind but reads its target.
    let target = climb_target_wrappers(node);
    if let Some(parent) = target.parent() {
        if parent.kind() == "assignment_statement"
            && !is_compound_assignment(parent, src)?
            && parent
                .child_by_field_name("left")
                .is_some_and(|left| is_within(target, left))
        {
            return Ok(());
        }
        if matches!(parent.kind(), "range_clause" | "receive_statement")
            && parent
                .child_by_field_name("left")
                .is_some_and(|left| is_within(target, left))
        {
            return Ok(());
        }
    }
    let qualifier = node_text(operand, src)?.to_string();
    push_value_reference(
        node_text(field, src)?.to_string(),
        field,
        qualifier,
        file_key,
        package,
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

/// Climb from `node` through multi-target lists to the enclosing target
/// itself, so `a.B, c = v, w` detects the write position. Value lists
/// climb harmlessly: the landing slot check decides.
fn climb_target_wrappers(mut node: Node) -> Node {
    while let Some(parent) = node.parent() {
        if parent.kind() == "expression_list" {
            node = parent;
        } else {
            break;
        }
    }
    node
}

/// Bare identifiers passed directly as call arguments are value references
/// (`handler` in `Spawn(handler)`), matching the other four languages.
/// Only direct children count: nested calls keep their own callee references
/// via the generic recursion, receivers are captured with the callee, and
/// binding sites stay untouched.
fn emit_argument_values(
    args: Node,
    src: &[u8],
    file_key: &str,
    package: &str,
    scope: &[String],
    out: &mut Vec<Reference>,
) -> Result<()> {
    emit_direct_identifiers(args, src, file_key, package, scope, out)
}

/// Emit DIRECT identifier children as Value references. Nested
/// composites are left to the normal recursion so each site emits exactly
/// once (no double emits: every identifier has exactly one parent arm).
fn emit_direct_identifiers(
    node: Node,
    src: &[u8],
    file_key: &str,
    package: &str,
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
                package,
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
    package: &str,
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
                package,
                scope,
                out,
            );
        }
    }
    Ok(())
}

/// Emit bare identifiers in the `field` expression list (`case A, B:`,
/// `var x, y = v, w`, `p, q := r, s`). Composite elements fall through
/// to the normal recursion.
fn emit_field_list_identifiers(
    node: Node,
    field: &str,
    src: &[u8],
    file_key: &str,
    package: &str,
    scope: &[String],
    out: &mut Vec<Reference>,
) -> Result<()> {
    if let Some(list) = node.child_by_field_name(field) {
        emit_direct_identifiers(list, src, file_key, package, scope, out)?;
    }
    Ok(())
}

/// Emit `node` as a Value reference when it is a bare identifier, or
/// unwrap one `wrapper` level (`expression_list`, `literal_element`)
/// first. Deeper composites fall through to the normal recursion.
fn emit_wrapped_identifier(
    node: Node,
    wrapper: &str,
    src: &[u8],
    file_key: &str,
    package: &str,
    scope: &[String],
    out: &mut Vec<Reference>,
) -> Result<()> {
    if node.kind() == "identifier" {
        push_reference(
            node_text(node, src)?.to_string(),
            node,
            ReferenceKind::Value,
            file_key,
            package,
            scope,
            out,
        );
    } else if node.kind() == wrapper {
        emit_direct_identifiers(node, src, file_key, package, scope, out)?;
    }
    Ok(())
}

/// True unless the assignment operator is plain `=`. A missing operator
/// (error recovery) reads as plain: a missed read beats a write
/// misreported as one.
fn is_compound_assignment(node: Node, src: &[u8]) -> Result<bool> {
    match node.child_by_field_name("operator") {
        Some(op) => Ok(node_text(op, src)? != "="),
        None => Ok(false),
    }
}

fn walk_imports(
    node: Node,
    src: &[u8],
    out: &mut Vec<Import>,
    budget: &WalkBudget,
) -> Result<()> {
    let _guard = DepthGuard::enter(budget)
        .ok_or_else(|| KeelError::TooDeeplyNested { limit: MAX_WALK_DEPTH })?;
    if node.kind() == "import_spec" {
        let path = match node.child_by_field_name("path") {
            Some(p) => strip_quotes(node_text(p, src)?),
            None => return Ok(()),
        };
        let alias = match node.child_by_field_name("name") {
            Some(n) if n.kind() == "package_identifier" => Some(node_text(n, src)?.to_string()),
            Some(n) if n.kind() == "blank_identifier" || n.kind() == "dot" => {
                Some(node_text(n, src)?.to_string())
            }
            _ => None,
        };
        out.push(Import {
            module_path: path,
            alias,
            file: PathBuf::new(),
        });
        return Ok(());
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_imports(child, src, out, budget)?;
    }
    Ok(())
}

fn strip_quotes(s: &str) -> String {
    s.trim_matches(|c| c == '"' || c == '`').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = r#"
package auth

import (
        "fmt"
        h "helper"
)

type Storage interface {
        Get(key string) string
}

type User struct {
        Name string
}

const MaxRetries = 3

func CreateOrder() {}

func (u User) Login() {
        CreateOrder()
        fmt.Println("hi")
        h.Run()
}
"#;

    fn test_path() -> &'static Path {
        Path::new("auth/service.go")
    }

    #[test]
    fn member_symbols_carry_enclosing_type_container() {
        let plugin = GoPlugin;
        let syms = plugin
            .extract_symbols(
                test_path(),
                "package auth\n\nfunc Top() {}\n\ntype Store struct {\n\tLimit int\n}\n\nfunc (s *Store) Save() {}\n\ntype Saver interface {\n\tSave()\n}\n",
            )
            .unwrap();
        let find = |n: &str| syms.iter().find(|s| s.name == n).cloned();
        assert_eq!(find("Top").unwrap().container, "");
        assert_eq!(find("Store").unwrap().container, "");
        assert_eq!(find("Limit").unwrap().container, "Store");
        let saves: Vec<&str> = syms
            .iter()
            .filter(|s| s.name == "Save")
            .map(|s| s.container.as_str())
            .collect();
        assert_eq!(saves, vec!["Store", "Saver"]);
    }

    #[test]
    fn extracts_package_aware_funcs_types_and_const() {
        let plugin = GoPlugin;
        let syms = plugin.extract_symbols(test_path(), SOURCE).unwrap();
        let find = |n: &str| syms.iter().find(|s| s.name == n).cloned();

        let create = find("CreateOrder").expect("CreateOrder");
        assert_eq!(create.kind, SymbolKind::Function);
        assert_eq!(create.module_path, "auth");

        assert_eq!(find("Login").unwrap().kind, SymbolKind::Function);
        assert_eq!(find("User").unwrap().kind, SymbolKind::Struct);
        assert_eq!(find("Storage").unwrap().kind, SymbolKind::Trait);
        assert_eq!(find("MaxRetries").unwrap().kind, SymbolKind::Const);
    }

    #[test]
    fn extracts_top_level_vars_as_const() {
        let plugin = GoPlugin;
        let syms = plugin
            .extract_symbols(
                test_path(),
                "package auth\n\nvar DefaultTimeout = 30\n\nvar (\n\tA = 1\n\tB, C = 2, 3\n)\n",
            )
            .unwrap();
        let find = |n: &str| syms.iter().find(|s| s.name == n).cloned();
        for name in ["DefaultTimeout", "A", "B", "C"] {
            let sym = find(name).unwrap_or_else(|| panic!("{name} extracted"));
            assert_eq!(sym.kind, SymbolKind::Const);
            assert_eq!(sym.module_path, "auth");
        }
    }

    #[test]
    fn package_main_uses_path_based_module() {
        let plugin = GoPlugin;
        let source = "package main\n\nfunc main() {}\n";
        let a = plugin
            .extract_symbols(Path::new("cmd/a/main.go"), source)
            .unwrap();
        let b = plugin
            .extract_symbols(Path::new("cmd/b/main.go"), source)
            .unwrap();
        assert_eq!(a[0].module_path, "cmd/a/main");
        assert_eq!(b[0].module_path, "cmd/b/main");
        assert_ne!(a[0].module_path, b[0].module_path);
    }

    #[test]
    fn extracts_imports_with_and_without_alias() {
        let plugin = GoPlugin;
        let imports = plugin.extract_imports(test_path(), SOURCE).unwrap();

        let fmt = imports
            .iter()
            .find(|i| i.module_path == "fmt")
            .expect("fmt import");
        assert_eq!(fmt.alias, None);

        let helper = imports
            .iter()
            .find(|i| i.module_path == "helper")
            .expect("helper import");
        assert_eq!(helper.alias, Some("h".to_string()));
    }

    #[test]
    fn extracts_call_and_selector_references() {
        let plugin = GoPlugin;
        let refs = plugin.extract_references(test_path(), SOURCE).unwrap();

        let call = refs
            .iter()
            .find(|r| r.name == "CreateOrder" && r.kind == ReferenceKind::Call)
            .expect("CreateOrder call");
        assert_eq!(call.container, "auth::Login");

        let println = refs
            .iter()
            .find(|r| r.name == "Println" && r.kind == ReferenceKind::Method)
            .expect("Println selector call");
        assert_eq!(println.kind, ReferenceKind::Method);

        let run = refs
            .iter()
            .find(|r| r.name == "Run" && r.kind == ReferenceKind::Method)
            .expect("Run selector call");
        assert_eq!(run.kind, ReferenceKind::Method);
    }

    #[test]
    fn extension_is_go() {
        assert_eq!(GoPlugin.extensions(), &["go"]);
    }

    #[test]
    fn selector_calls_keep_operand_qualifier() {
        let plugin = GoPlugin;
        let src = "package main\n\nimport \"fmt\"\n\nfunc run(svc Store) {\n\tfmt.Println(\"x\")\n\tsvc.Run()\n}\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();
        let methods: Vec<(&str, &str)> = refs
            .iter()
            .filter(|r| r.kind == ReferenceKind::Method)
            .map(|r| (r.name.as_str(), r.qualifier.as_str()))
            .collect();
        assert_eq!(methods, vec![("Println", "fmt"), ("Run", "svc")]);
    }

    #[test]
    fn call_arguments_capture_bare_value_references() {
        let plugin = GoPlugin;
        let src = r#"package auth

func Login(handler Handler) {
        Spawn(handler, 3)
        svc.Run(handler)
        Outer(Inner(deep))
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
        assert!(has("Spawn", ReferenceKind::Call));
        assert!(has("Run", ReferenceKind::Method));
        assert!(!has("Spawn", ReferenceKind::Value));
        // The method receiver is a value use.
        assert!(has("svc", ReferenceKind::Value));
    }

    #[test]
    fn type_uses_emit_type_references_not_definitions() {
        let plugin = GoPlugin;
        let src = "package auth\n\nimport \"fmt\"\n\ntype Widget struct{}\n\nfunc unwrap(v any) *Widget {\n\tw, _ := v.(*Widget)\n\treturn w\n}\n\nfunc build() Widget {\n\treturn Widget{}\n}\n\nfunc show(w fmt.Stringer) {}\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();
        let types: Vec<(&str, &str)> = refs
            .iter()
            .filter(|r| r.kind == ReferenceKind::Type)
            .map(|r| (r.name.as_str(), r.qualifier.as_str()))
            .collect();
        // Two result types, assertion, literal: definitions never emit.
        assert_eq!(types.iter().filter(|(n, _)| *n == "Widget").count(), 4);
        // Qualified types keep the package qualifier.
        assert!(types.contains(&("Stringer", "fmt")));
        // Conversions stay calls: `Widget(x)` must not double-emit a Type.
        let conv = plugin
            .extract_references(test_path(), "package auth\n\nfunc f(v any) any {\n\treturn Widget(v)\n}\n")
            .unwrap();
        assert!(conv.iter().any(|r| r.name == "Widget" && r.kind == ReferenceKind::Call));
        assert!(!conv.iter().any(|r| r.name == "Widget" && r.kind == ReferenceKind::Type));
    }

    #[test]
    fn selector_reads_emit_receivers_once() {
        let plugin = GoPlugin;
        let src = "package auth\n\nfunc total(items Items) int {\n\treturn items.Count\n}\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();

        let items: Vec<&Reference> = refs.iter().filter(|r| r.name == "items").collect();
        assert_eq!(items.len(), 1, "refs: {refs:?}");
        assert_eq!(items[0].kind, ReferenceKind::Value);
        let count: Vec<&Reference> = refs.iter().filter(|r| r.name == "Count").collect();
        assert_eq!(count.len(), 1, "refs: {refs:?}");
        assert_eq!(count[0].kind, ReferenceKind::Value);
        assert_eq!(count[0].qualifier, "items");
    }

    #[test]
    fn composite_literal_keys_read() {
        let plugin = GoPlugin;
        let src = "package auth\n\nfunc build(v int, k string) {\n\t_ = Point{X: v}\n\t_ = map[string]int{\"a\": v}\n\t_ = map[string]int{k: v}\n}\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();

        // Struct keys name declared fields and read unqualified.
        let x: Vec<&Reference> = refs.iter().filter(|r| r.name == "X").collect();
        assert_eq!(x.len(), 1, "refs: {refs:?}");
        assert_eq!(x[0].kind, ReferenceKind::Value);
        assert_eq!(x[0].qualifier, "");
        assert_eq!((x[0].start_line, x[0].start_col), (4, 12));
        // Map variable keys read; string keys hold no identifier.
        let k: Vec<&Reference> = refs.iter().filter(|r| r.name == "k").collect();
        assert_eq!(k.len(), 1, "refs: {refs:?}");
        assert_eq!(k[0].kind, ReferenceKind::Value);
        assert!(refs.iter().all(|r| r.name != "a"), "refs: {refs:?}");
        let v: Vec<&Reference> = refs.iter().filter(|r| r.name == "v").collect();
        assert_eq!(v.len(), 3, "refs: {refs:?}");
    }

    #[test]
    fn value_positions_read_bare_identifiers_once() {
        let plugin = GoPlugin;
        let src = r#"package pos

const G = 1
const H = 2

type Point struct {
	X int
}

func ret() int {
	return G
}

func sum() int {
	return G + H
}

func neg() int {
	return -G
}

func cond() int {
	if G > 0 {
		return G
	}
	return H
}

func loop(n int) {
	for n < G {
		n++
	}
	for i := 0; i < G; i++ {
		_ = i
	}
	for _, v := range []int{G, H} {
		_ = v
	}
}

func swtch(opt int) int {
	switch opt {
	case G:
		return G
	default:
		return H
	}
}

func assigns() {
	t := G
	var u int = H
	t = G
	t += H
	t = t + G
	_ = t
	_ = u
}

func coll() {
	a := []int{G, H}
	m := map[string]int{suffix: G}
	p := Point{X: G}
	s := a[G:H]
	x := a[G]
	y := (G)
	_ = a
	_ = m
	_ = p
	_ = s
	_ = x
	_ = y
}

func chfn(ch chan int) {
	ch <- G
	v := <-ch
	_ = v
}

func spread(xs ...int) int {
	return sum(xs...)
}

func paren() int {
	return (G)
}

func assert(v any) {
	w := v.(Point)
	_ = w
}

func multi() (int, int) {
	return G, H
}

func sel(p Point) int {
	return p.X
}
"#;
        let refs = plugin.extract_references(test_path(), src).unwrap();
        let named = |name: &str| -> Vec<&Reference> {
            refs.iter().filter(|r| r.name == name).collect()
        };

        // return, binary, unary, if-cond, then-return, for-cond,
        // for-clause-cond, range element, case value, case return,
        // short-var init, assign-RHS, assign-RHS binary operand, array
        // element, map value, struct field value, slice start, index,
        // paren, send value, paren-return, multi-return — each exactly
        // once.
        let g = named("G");
        assert_eq!(g.len(), 22, "all G reads, no doubles: {g:?}");
        assert!(g.iter().all(|r| r.kind == ReferenceKind::Value));
        // Binary-RHS, else-return, range element, default return,
        // var init, compound-RHS, array element, slice end,
        // multi-return.
        let h = named("H");
        assert_eq!(h.len(), 9, "all H reads: {h:?}");
        assert!(h.iter().all(|r| r.kind == ReferenceKind::Value));
        // Loop/index/channel variables.
        assert_eq!(named("n").len(), 2);
        assert_eq!(named("i").len(), 3);
        assert_eq!(named("v").len(), 3);
        assert_eq!(named("opt").len(), 1);
        assert_eq!(named("t").len(), 3);
        assert_eq!(named("u").len(), 1);
        assert_eq!(named("a").len(), 3);
        assert_eq!(named("m").len(), 1);
        assert_eq!(named("p").len(), 2);
        assert_eq!(named("s").len(), 1);
        assert_eq!(named("x").len(), 1);
        assert_eq!(named("y").len(), 1);
        assert_eq!(named("ch").len(), 2);
        assert_eq!(named("xs").len(), 1);
        assert_eq!(named("w").len(), 1);
        let sum = named("sum");
        assert_eq!(sum.len(), 1);
        assert_eq!(sum[0].kind, ReferenceKind::Call);
        // Type uses: literal, assertion, parameter.
        let point = named("Point");
        assert_eq!(point.len(), 3, "Point uses: {point:?}");
        assert!(point.iter().all(|r| r.kind == ReferenceKind::Type));
        // Builtin-type annotations stay Type uses.
        assert_eq!(named("int").len(), 19);
        assert!(named("int").iter().all(|r| r.kind == ReferenceKind::Type));
        assert_eq!(named("string").len(), 1);
        assert_eq!(named("any").len(), 1);
        // The selector field reads with its operand as qualifier;
        // the struct-literal key reads unqualified.
        let field_x = named("X");
        assert_eq!(field_x.len(), 2, "X uses: {field_x:?}");
        assert!(field_x.iter().all(|r| r.kind == ReferenceKind::Value));
        let quals: Vec<&str> = field_x.iter().map(|r| r.qualifier.as_str()).collect();
        assert!(quals.contains(&"p"), "quals: {quals:?}");
        assert!(quals.contains(&""), "quals: {quals:?}");
        // Map variable keys read.
        let suffix = named("suffix");
        assert_eq!(suffix.len(), 1, "suffix uses: {suffix:?}");
        assert_eq!(suffix[0].kind, ReferenceKind::Value);
        // Writes, bindings, and blanks never read.
        assert!(named("_").is_empty(), "_ must not read");
        // No strays: 22 + 9 + 2 + 3 + 3 + 1 + 3 + 1 + 3 + 1 + 2 + 1
        // + 1 + 1 + 2 + 1 + 1 values + the selector field, the two
        // literal keys, the call, `Point` x3, and the builtin-type
        // annotations (`int` x19, `string`, `any`).
        assert_eq!(refs.len(), 85, "unexpected rows: {refs:?}");
        // Container attribution survives: the return sits in `pos::ret`.
        assert!(g.iter().any(|r| r.container == "pos::ret"));
    }

    #[test]
    fn member_definitions_are_field_symbols() {
        let plugin = GoPlugin;
        let syms = plugin
            .extract_symbols(
                test_path(),
                "package pos\n\ntype Config struct {\n\tPort int\n\tHost string `json:\"host\"`\n\tX, Y int\n\tEmbedded\n}\n\nfunc (c Config) Serve() {}\n",
            )
            .unwrap();
        let find = |n: &str| syms.iter().find(|s| s.name == n).cloned();
        let field = SymbolKind::Other("field".into());
        assert_eq!(find("Port").unwrap().kind, field);
        assert_eq!(find("Port").unwrap().start_line, 4);
        assert_eq!(find("Host").unwrap().kind, field);
        assert_eq!(find("X").unwrap().kind, field);
        assert_eq!(find("Y").unwrap().kind, field);
        assert_eq!(find("Config").unwrap().kind, SymbolKind::Struct);
        assert_eq!(find("Serve").unwrap().kind, SymbolKind::Function);
        // Embedded fields carry a type, not a name.
        assert_eq!(
            syms.iter().filter(|s| s.kind == field).count(),
            4,
            "syms: {syms:?}"
        );
    }

    #[test]
    fn member_reads_emit_fields_once_with_qualifier() {
        let plugin = GoPlugin;
        let src = "package pos\n\nfunc f(cfg Config) int {\n\tp := cfg.Port\n\tcfg.Port = 1\n\tcfg.Count++\n\tcfg.Total += 1\n\t_, cfg.Multi = pair()\n\tq := cfg.A.B\n\tcfg.Serve()\n\treturn p\n}\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();
        let named = |n: &str| -> Vec<_> { refs.iter().filter(|r| r.name == n).collect() };

        // Reads carry the operand text as qualifier.
        let port = named("Port");
        assert_eq!(port.len(), 1, "refs: {refs:?}");
        assert_eq!(port[0].kind, ReferenceKind::Value);
        assert_eq!(port[0].qualifier, "cfg");
        assert_eq!(named("Count").len(), 1);
        assert_eq!(named("Total").len(), 1);
        let a = named("A");
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].qualifier, "cfg");
        let b = named("B");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].qualifier, "cfg.A");
        // Writes never read; the call keeps Method rules.
        assert!(named("Multi").is_empty(), "multi-target is a write");
        assert_eq!(named("Serve").len(), 1);
        assert_eq!(named("Serve")[0].kind, ReferenceKind::Method);
        assert_eq!(named("cfg").len(), 7, "refs: {refs:?}");
        assert!(named("cfg").iter().all(|r| r.kind == ReferenceKind::Value));
        // No strays: 7 operands + Port/Count/Total/A/B/Serve + pair +
        // p + Config + the int result.
        assert_eq!(refs.len(), 17, "unexpected rows: {refs:?}");
    }

    #[test]
    fn extract_impls_records_blank_assertions_only() {
        let plugin = GoPlugin;
        let src = "package auth\n\nimport \"io\"\n\nvar _ io.Reader = MyReader{}\nvar _ Store = &MemStore{}\nvar _ Closer = (*FileCloser)(nil)\nvar _ Store2[User] = GenericStore{}\nvar named Widget = Widget{}\nvar _ io.Writer = makeWriter()\nvar _ io.Seeker = current\n";
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
        // Qualified, pointer-literal, conversion, and generic-interface
        // forms all record; named vars, constructor calls, and bare
        // values prove nothing.
        assert_eq!(
            pairs,
            vec![
                ("MyReader", "Reader"),
                ("MemStore", "Store"),
                ("FileCloser", "Closer"),
                ("GenericStore", "Store2"),
            ]
        );
    }

    #[test]
    fn interface_method_specs_are_function_symbols() {
        let plugin = GoPlugin;
        let src = "package auth\n\ntype Doer interface {\n\tDo(x int) int\n\tClose()\n\tio.Reader\n}\n";
        let syms = plugin.extract_symbols(test_path(), src).unwrap();
        let find = |n: &str| syms.iter().find(|s| s.name == n).cloned();

        assert_eq!(find("Doer").unwrap().kind, SymbolKind::Trait);
        assert_eq!(find("Do").unwrap().kind, SymbolKind::Function);
        assert_eq!(find("Close").unwrap().kind, SymbolKind::Function);
        assert!(
            find("Reader").is_none(),
            "embedded interface is a use, not a def"
        );
        assert_eq!(syms.len(), 3, "unexpected symbols: {syms:?}");
    }
}
