//! TypeScript/TSX language plugin: Tree-sitter based symbol and reference extraction.
//!
//! `module_path` is derived from the file path with the extension stripped
//! (e.g. `src/auth/service`), using forward slashes.
//!
//! `.ts`/`.mts`/`.cts` parse with the TypeScript grammar; `.tsx` uses the TSX
//! grammar. Extraction walks are shared.

use super::{file_path_key, is_top_level_js_declaration, path_module_identity, resolve_relative_path_module, LanguagePlugin};
use crate::error::{Result, KeelError};
use crate::graph::types::{ImplRecord, Import, Reference, ReferenceKind, Symbol, SymbolKind};
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
    fn has_syntax_errors(&self, source_code: &str) -> bool {
        Self::parse_ts(source_code)
            .map(|tree| tree.root_node().has_error())
            .unwrap_or(false)
    }
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

    fn extract_imports(&self, path: &Path, source_code: &str) -> Result<Vec<Import>> {
        let tree = Self::parse_ts(source_code)?;
        let src = source_code.as_bytes();
        let mut out = Vec::new();
        walk_imports(tree.root_node(), src, &mut out)?;
        for imp in &mut out {
            imp.module_path = resolve_relative_path_module(path, &imp.module_path);
        }
        Ok(out)
    }

    fn extract_impls(&self, _path: &Path, source_code: &str) -> Result<Vec<ImplRecord>> {
        let tree = Self::parse_ts(source_code)?;
        let src = source_code.as_bytes();
        let mut out = Vec::new();
        walk_impls(tree.root_node(), src, &mut out)?;
        Ok(out)
    }
}

impl LanguagePlugin for TsxPlugin {
    fn has_syntax_errors(&self, source_code: &str) -> bool {
        TypeScriptPlugin::parse_tsx(source_code)
            .map(|tree| tree.root_node().has_error())
            .unwrap_or(false)
    }
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

    fn extract_imports(&self, path: &Path, source_code: &str) -> Result<Vec<Import>> {
        let tree = TypeScriptPlugin::parse_tsx(source_code)?;
        let src = source_code.as_bytes();
        let mut out = Vec::new();
        walk_imports(tree.root_node(), src, &mut out)?;
        for imp in &mut out {
            imp.module_path = resolve_relative_path_module(path, &imp.module_path);
        }
        Ok(out)
    }

    fn extract_impls(&self, _path: &Path, source_code: &str) -> Result<Vec<ImplRecord>> {
        let tree = TypeScriptPlugin::parse_tsx(source_code)?;
        let src = source_code.as_bytes();
        let mut out = Vec::new();
        walk_impls(tree.root_node(), src, &mut out)?;
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
        // Bodiless declarations are still definitions: abstract methods,
        // interface members, and function overloads.
        "abstract_method_signature" | "method_signature" | "function_signature" => {
            emit_named_symbol(node, SymbolKind::Function, module_path, out, src)?;
        }
        // Members are indexed items too: class fields, property
        // signatures, and enum members (`field` kind), so member reads
        // resolve to a definition. Computed/string/number/private names
        // are not plain identifiers and stay out.
        "public_field_definition" | "property_signature" | "enum_assignment" => {
            emit_named_symbol(node, SymbolKind::Other("field".into()), module_path, out, src)?;
        }
        // Bare enum members (`User,` without `= …`) are direct
        // identifiers under the body, not assignments.
        "enum_body" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "property_identifier" {
                    push_symbol(child, SymbolKind::Other("field".into()), module_path, out, src)?;
                }
            }
        }
        "lexical_declaration" | "variable_declaration"
            if is_top_level_js_declaration(node) =>
        {
            emit_top_level_variables(node, module_path, out, src)?;
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

/// Emit top-level `const`/`let`/`var` bindings (`Const` for `const`,
/// `variable` otherwise). Function-valued declarators are skipped here —
/// [`emit_bound_function`] owns those as `Function` — and destructured
/// names (`object_pattern`/`array_pattern`) are not simple bindings.
fn emit_top_level_variables(
    node: Node,
    module_path: &str,
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
                push_symbol(name, kind.clone(), module_path, out, src)?;
            }
        }
    }
    Ok(())
}

/// Emit `const name = () => …` / `function …` bindings as `Function`
/// (mirrors the JavaScript walker).
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
        | "interface_declaration"
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
            if emit_decorator_reference(node, src, file_key, module_path, scope, out)? {
                return Ok(());
            }
        }
        // Bare identifiers in value positions: positional arguments.
        // Object-literal values use the `pair` arm below; property keys
        // are labels, not references.
        "arguments" => {
            emit_argument_values(node, src, file_key, module_path, scope, out)?;
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
        // Type annotations (`x: T`). Nested annotations handle their own
        // level; call/new subtrees fall through to the normal rules below.
        "type_annotation" => {
            emit_type_identifiers(node, src, file_key, module_path, scope, out)?;
        }
        // Heritage clauses (`extends Base`, `implements Shape`, interface
        // `extends`): the named types are uses attributed to the
        // class/interface (whose scope is already pushed). Call-form
        // heritage (`extends mixin(Base)`) falls through to the call rules.
        "extends_clause" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.is_named() && child.kind() != "call_expression" {
                    emit_type_subtree(child, src, file_key, module_path, scope, out)?;
                }
            }
        }
        "implements_clause" | "extends_type_clause" | "constraint" | "default_type" => {
            emit_type_subtree(node, src, file_key, module_path, scope, out)?;
        }
        // Type alias bodies (`type X = T extends A<infer U> ? U : B`):
        // every named type inside is a use. `constraint`/`default_type`
        // in the alias's own parameters are owned by the arm above, and
        // nested annotations/calls keep their own rules, so each site
        // emits exactly once. (`typeof foo` operands read as Type, and
        // `infer` bindings stay out — see the walker.) The alias pushes
        // scope like any declaration, so impact lists the alias (not a
        // file-scope note) as the enclosing symbol of body uses.
        "type_alias_declaration" => {
            let pushed = match node.child_by_field_name("name") {
                Some(name)
                    if matches!(name.kind(), "type_identifier" | "identifier") =>
                {
                    scope.push(node_text(name, src)?.to_string());
                    true
                }
                _ => false,
            };
            if let Some(value) = node.child_by_field_name("value") {
                emit_alias_value_types(value, src, file_key, module_path, scope, out)?;
            }
            if pushed {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    walk_references(child, src, file_key, module_path, scope, out)?;
                }
                scope.pop();
                return Ok(());
            }
        }
        // Casts (`x as Config`, `x satisfies Config`): the type operand
        // (always the last named child) is a use, and so is a bare
        // value-side identifier; composite values fall through to the
        // normal recursion. (The type operand is owned by the Type arm —
        // never double-emitted here.)
        "as_expression" | "satisfies_expression" => {
            let mut cursor = node.walk();
            let named: Vec<Node> =
                node.children(&mut cursor).filter(|c| c.is_named()).collect();
            if let Some(ty) = named.last() {
                emit_type_subtree(*ty, src, file_key, module_path, scope, out)?;
            }
            if let Some(first) = named.first() {
                let is_type = named.last().is_some_and(|t| t.id() == first.id());
                if !is_type && first.kind() == "identifier" {
                    push_reference(
                        node_text(*first, src)?.to_string(),
                        *first,
                        ReferenceKind::Value,
                        file_key,
                        module_path,
                        scope,
                        out,
                    );
                }
            }
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
        // Value reads in statement/expression positions (`return x`,
        // `throw err`, `...args`, `await p`, `a + b`, `!ok`, `c ? a : b`,
        // `[a]`, `(x)`, `a, b`, `x!`, `{v}` in TSX): only DIRECT
        // identifier children emit here; nested composites fall through
        // to the recursion below so each site emits exactly once.
        // Attribute names (`timeout=` in `<W timeout={v}>`) sit under
        // `jsx_attribute`, not `jsx_expression`, so they stay out.
        "return_statement" | "throw_statement" | "spread_element"
        | "await_expression" | "yield_expression" | "binary_expression"
        | "unary_expression" | "ternary_expression" | "array"
        | "sequence_expression" | "parenthesized_expression"
        | "non_null_expression" | "jsx_expression" => {
            emit_direct_identifiers(node, src, file_key, module_path, scope, out)?;
        }
        // `export = Name` and `export default Name` mention the local:
        // renaming it must update the export, so it reads. Declarations,
        // specifier lists, and re-exports have richer shapes and stay
        // out (specifiers are owned by the arm below).
        "export_statement" => {
            emit_export_value(node, src, file_key, module_path, scope, out)?;
        }
        // Export specifiers (`export { helper }`, `export type { Local }`)
        // mention the local name: renaming it must update the specifier,
        // so it reads (as Value even for type exports — references and
        // impact match by name). The `alias` (`export { h as helper }`)
        // names the export and stays out, as does the `source` of
        // re-exports (owned by the import edges).
        "export_specifier" => {
            if let Some(name) = node.child_by_field_name("name") {
                if matches!(name.kind(), "identifier" | "type_identifier") {
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
        // Initializer reads (`const t = value`); the bound name is a
        // definition, never a reference.
        "variable_declarator" => {
            emit_field_identifier(node, "value", src, file_key, module_path, scope, out)?;
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
        // Enum member initializers (`A = LIMIT`) read.
        "enum_assignment" => {
            emit_field_identifier(node, "value", src, file_key, module_path, scope, out)?;
        }
        // Default values (`(a = d)`, `[a = d] = v`, `{a = d} = v`):
        // the default reads; the bound pattern is a definition,
        // never a reference. (Composites recurse into their own
        // arms via the walk below.)
        "assignment_pattern" | "object_assignment_pattern" => {
            emit_field_identifier(node, "right", src, file_key, module_path, scope, out)?;
        }
        // Typed parameter defaults (`(a: T = d)`); the pattern binds
        // and the annotation owns itself via the type arms. (Defaults
        // are illegal in type positions, so `value` there is absent.)
        "required_parameter" | "optional_parameter" => {
            emit_field_identifier(node, "value", src, file_key, module_path, scope, out)?;
        }
        // Class field initializers (`x = v`); the field name is a
        // definition, never a reference.
        "public_field_definition" => {
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
        walk_references(child, src, file_key, module_path, scope, out)?;
    }
    Ok(())
}

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
/// bare identifier (`export = Store`, `export default helper`).
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

/// Emit identifiers in a `type_annotation` subtree as Type references,
/// skipping nested annotations (their own arm level handles them) and
/// call/new subtrees (handled through normal recursion).
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
        if node.id() != root.id()
            && matches!(
                node.kind(),
                "type_annotation" | "call_expression" | "new_expression"
            )
        {
            continue;
        }
        if matches!(node.kind(), "identifier" | "type_identifier") {
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

/// Emit identifiers in a type alias body as Type references. Nested
/// annotations, bounds, defaults, value-level template holes (under
/// `typeof`), and call/new subtrees keep their own rules via the normal
/// recursion, so each site emits exactly once; `infer U` and
/// generic-function `<T>` bindings declare and stay out (only their
/// bounds/defaults emit). Template-*type* holes are a different node
/// (`template_type`) and read as types here.
fn emit_alias_value_types(
    root: Node,
    src: &[u8],
    file_key: &str,
    module_path: &str,
    scope: &[String],
    out: &mut Vec<Reference>,
) -> Result<()> {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.id() != root.id()
            && matches!(
                node.kind(),
                "type_annotation"
                    | "call_expression"
                    | "new_expression"
                    | "constraint"
                    | "default_type"
                    | "template_substitution"
            )
        {
            continue;
        }
        // `infer U [extends X]`: the name binds; anything else (a bound)
        // still uses.
        if node.kind() == "infer_type" {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if !child.is_named() || child.kind() != "type_identifier" {
                    stack.push(child);
                }
            }
            continue;
        }
        // Generic function types (`<T>(x: T) => T`): the parameter name
        // binds and stays out; its bound/default match the constraint
        // arms through the normal recursion.
        if node.kind() == "type_parameter" {
            continue;
        }
        // Function-type parameters (`(x: T)`): the pattern binds; only
        // the annotation uses. (Defaults/decorators are not legal in
        // type positions, so the `type` field is all that matters.)
        if node.kind() == "required_parameter" || node.kind() == "optional_parameter" {
            if let Some(ty) = node.child_by_field_name("type") {
                stack.push(ty);
            }
            continue;
        }
        if matches!(node.kind(), "identifier" | "type_identifier") {
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

/// Emit every identifier under a heritage/bound subtree as a `type` use.
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
        if matches!(node.kind(), "identifier" | "type_identifier") {
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
            walk_references(child, src, file_key, module_path, scope, out)?;
        }
    } else {
        emit_call_reference(expr, src, file_key, module_path, scope, out)?;
    }
    if pushed {
        scope.pop();
    }
    Ok(true)
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
    if node.kind() == "import_require_clause" {
        // Legacy `import util = require("./util")`: a whole-module edge
        // aliased to the local name, exactly like `import * as util`.
        // (`import A = B.C` entity aliases bind no module and stay out.)
        if let (Some(s), Some(alias)) = (
            node.child_by_field_name("source"),
            node.named_children(&mut node.walk()).find(|c| c.kind() == "identifier"),
        ) {
            out.push(Import {
                module_path: strip_quotes(node_text(s, src)?),
                alias: Some(node_text(alias, src)?.to_string()),
                file: PathBuf::new(),
            });
        }
        return Ok(());
    }
    if node.kind() == "export_statement" {
        // Re-export: `export * from './a'`, `export { a as b } from './a'`,
        // `export * as ns from './a'`. A local-binding-free module edge
        // (unlike `import`, `export { a } from` binds nothing locally), so
        // one row with no alias. Plain `export { a }` / `export const`
        // declarations have no `source` and fall through.
        if let Some(s) = node.child_by_field_name("source") {
            out.push(Import {
                module_path: strip_quotes(node_text(s, src)?),
                alias: None,
                file: PathBuf::new(),
            });
            return Ok(());
        }
    }
    if node.kind() == "variable_declarator" {
        emit_require_import(node, src, out)?;
        emit_dynamic_import(node, src, out)?;
    }
    if node.kind() == "call_expression" {
        emit_bare_dynamic_import(node, src, out)?;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_imports(child, src, out)?;
    }
    Ok(())
}

/// CommonJS `const util = require("./util")`: a whole-module edge
/// aliased to the local name, exactly like `import * as util`.
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

/// Record one (type, base) pair at the declaration's position.
/// Emit one [`ImplRecord`] per `implements`/`extends` entry of a class or
/// class expression's `class_heritage`.
fn push_class_heritage(
    node: Node,
    type_name: String,
    src: &[u8],
    out: &mut Vec<ImplRecord>,
) -> Result<()> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "class_heritage" {
            continue;
        }
        let mut hcursor = child.walk();
        for entry in child.children(&mut hcursor) {
            match entry.kind() {
                "implements_clause" => {
                    let mut icursor = entry.walk();
                    for base in entry.children(&mut icursor) {
                        if let Some(base_name) = heritage_base_name(base, src)? {
                            push_impl(type_name.clone(), base_name, node, out)?;
                        }
                    }
                }
                "extends_clause" => {
                    if let Some(value) = entry.child_by_field_name("value") {
                        if let Some(base_name) = heritage_base_name(value, src)? {
                            push_impl(type_name.clone(), base_name, node, out)?;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Name a `class` expression: its own `class Y` name first, else the
/// enclosing `const X = class ...` declarator or `X = class ...`
/// assignment target (bare or `obj.X`). Anonymous exports
/// (`export default class ...`) have none of these and stay out.
fn class_expression_name(node: Node, src: &[u8]) -> Result<Option<String>> {
    if let Some(name) = node.child_by_field_name("name") {
        if matches!(name.kind(), "type_identifier" | "identifier") {
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

fn push_impl(
    type_name: String,
    base_name: String,
    decl: Node,
    out: &mut Vec<ImplRecord>,
) -> Result<()> {
    let pos = decl.start_position();
    out.push(ImplRecord {
        type_name,
        trait_name: Some(base_name),
        file: PathBuf::new(),
        start_line: pos.row as u32 + 1,
        start_col: pos.column as u32 + 1,
    });
    Ok(())
}

/// Base name of a heritage entry: a bare identifier, the final segment
/// of a member expression or nested type identifier (`ns.Base` counts as
/// `Base` — the query side attributes via the file's imports), or the
/// outer name of a generic instantiation (`I<T>` counts as `I`).
/// Anything else is skipped.
fn heritage_base_name(node: Node, src: &[u8]) -> Result<Option<String>> {
    match node.kind() {
        "identifier" | "type_identifier" => Ok(Some(node_text(node, src)?.to_string())),
        "member_expression" => match node.child_by_field_name("property") {
            Some(p) if matches!(p.kind(), "identifier" | "property_identifier") => {
                Ok(Some(node_text(p, src)?.to_string()))
            }
            _ => Ok(None),
        },
        "nested_type_identifier" => match node.child_by_field_name("name") {
            Some(n) if matches!(n.kind(), "identifier" | "type_identifier") => {
                Ok(Some(node_text(n, src)?.to_string()))
            }
            _ => Ok(None),
        },
        "generic_type" => match node.child_by_field_name("name") {
            Some(n) => heritage_base_name(n, src),
            _ => Ok(None),
        },
        _ => Ok(None),
    }
}

/// Collect (class/interface, base) pairs: `implements` and `extends` on
/// classes, `extends` on interfaces. Shared by the TS and TSX plugins.
fn walk_impls(node: Node, src: &[u8], out: &mut Vec<ImplRecord>) -> Result<()> {
    match node.kind() {
        "class_declaration" | "abstract_class_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                if matches!(name.kind(), "type_identifier" | "identifier") {
                    push_class_heritage(node, node_text(name, src)?.to_string(), src, out)?;
                }
            }
        }
        // Class expressions (`const X = class implements I`) count too,
        // named by their declarator (or their own `class Y` name first).
        "class" => {
            if let Some(type_name) = class_expression_name(node, src)? {
                push_class_heritage(node, type_name, src, out)?;
            }
        }
        "interface_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                if matches!(name.kind(), "type_identifier" | "identifier") {
                    let type_name = node_text(name, src)?.to_string();
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        if child.kind() != "extends_type_clause" {
                            continue;
                        }
                        let mut tcursor = child.walk();
                        for base in child.children(&mut tcursor) {
                            if let Some(base_name) = heritage_base_name(base, src)? {
                                push_impl(type_name.clone(), base_name, node, out)?;
                            }
                        }
                    }
                }
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_impls(child, src, out)?;
    }
    Ok(())
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
            .find(|i| i.module_path == "src/auth/util::helper")
            .expect("named helper import");
        assert_eq!(named.alias, Some("h".to_string()));

        let default = imports
            .iter()
            .find(|i| i.module_path == "src/auth/auth")
            .expect("default Auth import");
        assert_eq!(default.alias, Some("Auth".to_string()));
    }

    #[test]
    fn extracts_export_from_reexports() {
        let plugin = TypeScriptPlugin;
        let imports = plugin
            .extract_imports(
                Path::new("src/barrel.ts"),
                "export * from \"./a\";\nexport { a as b } from \"./a\";\nexport * as ns from \"./b\";\nexport const c = 1;\nexport { c };\n",
            )
            .unwrap();
        let modules: Vec<&str> = imports.iter().map(|i| i.module_path.as_str()).collect();
        assert_eq!(modules, vec!["src/a", "src/a", "src/b"]);
        assert!(
            imports.iter().all(|i| i.alias.is_none()),
            "export-from binds nothing locally: {imports:?}"
        );
    }

    #[test]
    fn extracts_import_require_clause_as_namespace_edge() {
        let plugin = TypeScriptPlugin;
        let imports = plugin
            .extract_imports(
                Path::new("src/app.ts"),
                "import util = require(\"./util\");\n",
            )
            .unwrap();
        assert_eq!(imports.len(), 1, "imports: {imports:?}");
        assert_eq!(imports[0].alias, Some("util".to_string()));
        assert!(
            imports[0].module_path.ends_with("util"),
            "imports: {imports:?}"
        );
    }

    #[test]
    fn extracts_commonjs_require_calls() {
        let plugin = TypeScriptPlugin;
        let imports = plugin
            .extract_imports(
                Path::new("src/app.ts"),
                "const util = require(\"./util\");\n",
            )
            .unwrap();
        assert_eq!(imports.len(), 1, "imports: {imports:?}");
        assert_eq!(imports[0].alias, Some("util".to_string()));
        assert!(
            imports[0].module_path.ends_with("util"),
            "imports: {imports:?}"
        );
    }

    #[test]
    fn extracts_dynamic_import_edges() {
        let plugin = TypeScriptPlugin;
        let imports = plugin
            .extract_imports(
                Path::new("src/app.ts"),
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
    fn extracts_top_level_variables_not_locals() {
        let plugin = TypeScriptPlugin;
        let syms = plugin
            .extract_symbols(
                Path::new("src/vars.ts"),
                "const C = 1;\nlet l = 2;\nvar v = 3;\nexport const E = 4;\ndeclare const D: number;\nconst { d } = {};\nfunction f() {\n  const local = 5;\n  return local;\n}\n",
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
        assert_eq!(find("D").unwrap().kind, SymbolKind::Const);
        assert!(find("d").is_none(), "destructured names are not bindings");
        assert!(find("local").is_none(), "function locals stay out");
        assert!(find("f").is_some());
    }

    #[test]
    fn bound_arrow_functions_are_functions_once() {
        let plugin = TypeScriptPlugin;
        let syms = plugin
            .extract_symbols(Path::new("src/fn.ts"), "const run = () => {};\n")
            .unwrap();
        assert_eq!(syms.len(), 1, "no Const+Function double emit: {syms:?}");
        assert_eq!(syms[0].kind, SymbolKind::Function);
    }

    #[test]
    fn value_positions_read_bare_identifiers_once() {
        let plugin = TypeScriptPlugin;
        let refs = plugin
            .extract_references(
                Path::new("src/vals.ts"),
                "const G = 1;\nconst H = 2;\ninterface Config {}\nfunction use() {\n  return G;\n}\nconst t = G;\nconst sum = G + H;\nconst neg = !G;\nconst pick = G ? H : 0;\nconst arr = [G];\nconst cp = [...G];\nasync function w() { await G; }\nfunction* g() { yield G; }\nfunction t2() { throw G; }\nconst p = (G);\nconst s = (G, 0);\nlet m = 0;\nm = G;\nG += H;\nG++;\nconst o = { k: G };\nconst first = G[H];\nfor (const x of G) {}\nswitch (G) { case H: break; }\nenum E { A = G }\nconst cast = G as Config;\nconst nn = G!;\n",
            )
            .unwrap();
        let named = |n: &str| -> Vec<_> { refs.iter().filter(|r| r.name == n).collect() };
        // return, init, binary, unary, ternary, array, spread, await, yield,
        // throw, parens, sequence, assign-RHS, aug-LHS, update, pair-value,
        // subscript-object, for-in-iterable, switch discriminant, enum
        // value, as-value, non-null — each exactly once.
        let g = named("G");
        assert_eq!(g.len(), 22, "all G reads, no doubles: {g:?}");
        assert!(g.iter().all(|r| r.kind == ReferenceKind::Value));
        // Binary-RHS, ternary-alternative, aug-RHS, subscript-index,
        // switch-case value.
        let h = named("H");
        assert_eq!(h.len(), 5, "all H reads: {h:?}");
        // The cast target is a Type use, not a Value read.
        let config = named("Config");
        assert_eq!(config.len(), 1);
        assert_eq!(config[0].kind, ReferenceKind::Type);
        // Writes, bindings, and labels are never reads.
        assert!(named("m").is_empty(), "assign-LHS is a write");
        assert!(named("k").is_empty(), "pair keys are labels");
        assert!(named("x").is_empty(), "for-targets are bindings");
        assert!(named("t").is_empty(), "bound names are definitions");
        // No strays anywhere: 22 + 5 + 1 rows total.
        assert_eq!(refs.len(), 28, "unexpected rows: {refs:?}");
        // Container attribution survives: the return sits in `use`.
        assert!(g.iter().any(|r| r.container == "src/vals::use"));
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
    fn extracts_annotation_and_argument_value_references() {
        let plugin = TypeScriptPlugin;
        let src = "function f(x: MyType): Ret { return x; }\n\
             const y: MyType = make();\n\
             run(callbackFn, { key: value });\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();
        let has = |name: &str, kind: ReferenceKind| {
            refs.iter().any(|r| r.name == name && r.kind == kind)
        };

        assert!(has("MyType", ReferenceKind::Type));
        assert!(has("Ret", ReferenceKind::Type));
        assert!(has("callbackFn", ReferenceKind::Value));
        assert!(has("value", ReferenceKind::Value));
        assert!(!refs.iter().any(|r| r.name == "key"));
        // Return-statement uses read too.
        assert!(has("x", ReferenceKind::Value));
    }

    #[test]
    fn method_receiver_is_value_reference() {
        let plugin = TypeScriptPlugin;
        let src = "function f(svc: Store) { svc.refresh(); this.reset(); }\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();
        let has = |name: &str, kind: ReferenceKind| {
            refs.iter().any(|r| r.name == name && r.kind == kind)
        };
        assert!(has("refresh", ReferenceKind::Method));
        assert!(has("svc", ReferenceKind::Value));
        // `this` is a keyword, not an identifier: no receiver ref.
        assert!(!refs.iter().any(|r| r.name == "this"));
    }

    #[test]
    fn method_calls_keep_receiver_qualifier() {
        let plugin = TypeScriptPlugin;
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
    fn extract_impls_records_implements_and_extends() {
        let plugin = TypeScriptPlugin;
        let src = "class C implements I, J {}\nclass D extends E {}\ninterface K extends L {}\nclass Plain {}\n";
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
            vec![("C", "I"), ("C", "J"), ("D", "E"), ("K", "L")]
        );
        assert_eq!(impls[0].start_line, 1);
        assert_eq!(impls[3].start_line, 3);
    }

    #[test]
    fn extract_impls_records_member_final_segments() {
        let plugin = TypeScriptPlugin;
        let src = "class C extends ns.Base {}\nclass D implements ns.Sized {}\nclass E extends ns.Box<number> {}\nclass F extends mixin(G) {}\n";
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
        // Member bases count by final segment (generics included);
        // call-form heritage stays out (the mixin result, not its
        // argument, is the base).
        assert_eq!(pairs, vec![("C", "Base"), ("D", "Sized"), ("E", "Box")]);
    }

    #[test]
    fn extract_impls_names_class_expressions_by_declarator() {
        let plugin = TypeScriptPlugin;
        let src = "const X = class implements I {}\nconst Y = class Named extends E {}\nconst Z = class Y2 implements J {}\nexports.Svc = class implements K2 {}\nexport default class implements K {}\n";
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
        // Declarator names win for anonymous classes; an own `class Y`
        // name wins over its declarator; assignment targets (bare or
        // member) name the rest; anonymous default exports stay out.
        assert_eq!(
            pairs,
            vec![("X", "I"), ("Named", "E"), ("Y2", "J"), ("Svc", "K2")]
        );
    }

    #[test]
    fn extensions_cover_typescript_variants() {
        let plugin = TypeScriptPlugin;
        assert_eq!(plugin.extensions(), &["ts", "mts", "cts"]);
        let tsx = TsxPlugin;
        assert_eq!(tsx.extensions(), &["tsx"]);
    }

    #[test]
    fn jsx_elements_emit_component_calls_not_host_tags() {
        let plugin = TsxPlugin;
        let src = "function App(): JSX.Element {\n  return <div><Widget title=\"hi\" /><Foo.Bar /></div>;\n}\n";
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
        let plugin = TypeScriptPlugin;
        let src = "function Logged(t: any) { return t; }\n@Logged\nexport class Service {\n  run() {}\n}\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();

        let logged: Vec<&Reference> = refs.iter().filter(|r| r.name == "Logged").collect();
        assert_eq!(logged.len(), 1, "refs: {refs:?}");
        assert_eq!(logged[0].kind, ReferenceKind::Call);
        assert_eq!(logged[0].container, "src/auth/service::Service");
    }

    #[test]
    fn heritage_and_bounds_emit_type_uses() {
        let plugin = TypeScriptPlugin;
        let src = "class Base {}\ninterface Shape {}\nclass Child extends Base implements Shape {}\ninterface Sub extends Shape {}\nfunction pick<T extends Shape>(v: T): T { return v; }\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();
        let types: Vec<(&str, &str)> = refs
            .iter()
            .filter(|r| r.kind == ReferenceKind::Type)
            .map(|r| (r.name.as_str(), r.container.as_str()))
            .collect();

        assert!(types.contains(&("Base", "src/auth/service::Child")), "types: {types:?}");
        assert!(types.contains(&("Shape", "src/auth/service::Child")), "types: {types:?}");
        // Interface heritage attributes to the sub-interface.
        assert!(types.contains(&("Shape", "src/auth/service::Sub")), "types: {types:?}");
        // Generic bounds attribute to the constrained function.
        assert!(types.contains(&("Shape", "src/auth/service::pick")), "types: {types:?}");
    }

    #[test]
    fn shorthand_properties_emit_value_reads_not_bindings() {
        let plugin = TypeScriptPlugin;
        let src = "function build(handler: H): Response {\n  const { bound } = load();\n  return { handler };\n}\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();

        let handler: Vec<&Reference> = refs.iter().filter(|r| r.name == "handler").collect();
        assert_eq!(handler.len(), 1, "refs: {refs:?}");
        assert_eq!(handler[0].kind, ReferenceKind::Value);
        // Destructuring targets are bindings, not reads.
        assert!(!refs.iter().any(|r| r.name == "bound" && r.start_line == 2));
    }

    #[test]
    fn bodiless_signatures_are_definitions() {
        let plugin = TypeScriptPlugin;
        let src = "abstract class Store {\n  abstract load(key: string): string;\n}\ninterface Shape {\n  area(): number;\n}\nfunction pick(v: string): void;\nfunction pick(v: number): void;\nfunction pick(v: unknown) {}\n";
        let syms = plugin.extract_symbols(test_path(), src).unwrap();
        let find = |n: &str| syms.iter().find(|s| s.name == n).cloned();

        assert_eq!(find("load").expect("abstract").kind, SymbolKind::Function);
        assert_eq!(find("area").expect("interface").kind, SymbolKind::Function);
        // Both overload signatures plus the implementation index.
        assert_eq!(syms.iter().filter(|s| s.name == "pick").count(), 3);
    }

    #[test]
    fn member_reads_emit_receivers_and_new_calls_work() {
        let plugin = TypeScriptPlugin;
        let src = "function total(items: Item[]): number {\n  return items.length;\n}\nfunction build() {\n  return new Helper();\n}\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();

        let items: Vec<&Reference> = refs.iter().filter(|r| r.name == "items").collect();
        assert_eq!(items.len(), 1, "refs: {refs:?}");
        assert_eq!(items[0].kind, ReferenceKind::Value);
        // Constructor calls resolve like calls.
        assert!(refs
            .iter()
            .any(|r| r.name == "Helper" && r.kind == ReferenceKind::Call));
    }

    #[test]
    fn casts_emit_target_types_and_templates_emit_reads() {
        let plugin = TypeScriptPlugin;
        let src = "type Config = { port: number };\nfunction load(raw: unknown): Config {\n  const cfg = raw as Config;\n  return `${cfg.port}`;\n}\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();

        let config: Vec<&Reference> = refs.iter().filter(|r| r.name == "Config").collect();
        // Return annotation plus the `as` cast target.
        assert_eq!(config.len(), 2, "refs: {refs:?}");
        assert!(config.iter().all(|r| r.kind == ReferenceKind::Type));
        // Member interpolation reads its receiver via the member arm.
        assert!(refs.iter().any(|r| r.name == "cfg" && r.kind == ReferenceKind::Value));
    }

    #[test]
    fn member_definitions_are_field_symbols() {
        let plugin = TypeScriptPlugin;
        let syms = plugin
            .extract_symbols(
                Path::new("src/members.ts"),
                "class Config {\n  port = 8080;\n  host: string;\n  #secret = 1;\n  serve() {}\n}\ninterface Point {\n  x: number;\n  move(): void;\n}\nenum Role {\n  Admin = 1,\n  User,\n}\n",
            )
            .unwrap();
        let find = |n: &str| syms.iter().find(|s| s.name == n).cloned();
        let field = SymbolKind::Other("field".into());
        assert_eq!(find("port").unwrap().kind, field);
        assert_eq!(find("port").unwrap().start_line, 2);
        assert_eq!(find("host").unwrap().kind, field);
        assert_eq!(find("x").unwrap().kind, field);
        assert_eq!(find("Admin").unwrap().kind, field);
        assert_eq!(find("User").unwrap().kind, field);
        // Methods keep their kinds; private names stay out.
        assert_eq!(find("serve").unwrap().kind, SymbolKind::Function);
        assert_eq!(find("move").unwrap().kind, SymbolKind::Function);
        assert!(find("#secret").is_none());
        // No doubles: assigned and bare members emit once each.
        assert_eq!(syms.iter().filter(|s| s.name == "Admin").count(), 1);
        assert_eq!(syms.iter().filter(|s| s.name == "User").count(), 1);
    }

    #[test]
    fn member_reads_emit_properties_once_with_qualifier() {
        let plugin = TypeScriptPlugin;
        let src = "function f(cfg: Config) {\n  const p = cfg.port;\n  cfg.port = 1;\n  cfg.count += 1;\n  cfg.total++;\n  for (cfg.key in obj) {}\n  delete cfg.gone;\n  const q = cfg.a.b;\n  cfg.serve();\n  const r = Role.Admin;\n}\n";
        let refs = plugin.extract_references(Path::new("src/uses.ts"), src).unwrap();
        let named = |n: &str| -> Vec<_> { refs.iter().filter(|r| r.name == n).collect() };

        // Reads carry the receiver text as qualifier.
        let port = named("port");
        assert_eq!(port.len(), 1, "refs: {refs:?}");
        assert_eq!(port[0].kind, ReferenceKind::Value);
        assert_eq!(port[0].qualifier, "cfg");
        assert_eq!(named("count").len(), 1);
        assert_eq!(named("total").len(), 1);
        let a = named("a");
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].qualifier, "cfg");
        let b = named("b");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].qualifier, "cfg.a");
        let admin = named("Admin");
        assert_eq!(admin.len(), 1);
        assert_eq!(admin[0].qualifier, "Role");
        // Writes never read; the call keeps Method rules with no Value echo.
        assert!(named("key").is_empty(), "for-in target is a write");
        assert!(named("gone").is_empty(), "delete is not a read");
        assert!(named("serve").iter().all(|r| r.kind == ReferenceKind::Method));
        assert_eq!(named("serve").len(), 1);
        // Receivers still read at every site, including writes.
        assert_eq!(named("cfg").len(), 8, "refs: {refs:?}");
        assert!(named("cfg").iter().all(|r| r.kind == ReferenceKind::Value));
        // No strays: 8 receivers + port/count/total/a/b/Admin + serve +
        // Role + obj + the Config annotation.
        assert_eq!(refs.len(), 18, "unexpected rows: {refs:?}");
    }

    #[test]
    fn member_writes_in_destructuring_targets_stay_out() {
        let plugin = TypeScriptPlugin;
        let src = "function f(a, v) {\n  [a.b, c] = v;\n  ({x: a.c} = v);\n  [x2 = a.d] = v;\n  ({[a.e]: y} = v);\n  for ([a.f] of g) {}\n}\n";
        let refs = plugin.extract_references(Path::new("src/destructure.ts"), src).unwrap();
        let named = |n: &str| -> Vec<_> { refs.iter().filter(|r| r.name == n).collect() };

        // Destructuring targets write; defaults and computed keys read.
        assert!(named("b").is_empty(), "array target is a write: {refs:?}");
        assert!(named("c").is_empty());
        assert!(named("f").is_empty(), "for-in target is a write");
        assert_eq!(named("d").len(), 1);
        assert_eq!(named("e").len(), 1);
        assert_eq!(named("a").len(), 5, "refs: {refs:?}");
        assert_eq!(named("v").len(), 4);
        assert_eq!(named("g").len(), 1);
        assert_eq!(refs.len(), 12, "unexpected rows: {refs:?}");
    }

    #[test]
    fn export_specifiers_read_local_names_not_aliases() {
        let plugin = TypeScriptPlugin;
        let src = "interface Local { v: number; }\nfunction helper() { return 1; }\nexport { helper };\nexport type { Local };\nexport { helper as h };\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();
        let named = |n: &str| -> Vec<_> { refs.iter().filter(|r| r.name == n).collect() };

        assert_eq!(named("helper").len(), 2, "refs: {refs:?}");
        assert!(named("helper").iter().all(|r| r.kind == ReferenceKind::Value));
        assert_eq!(named("Local").len(), 1);
        assert_eq!(named("Local")[0].kind, ReferenceKind::Value);
        assert!(named("h").is_empty(), "refs: {refs:?}");
        // No strays: 2 helpers + Local.
        assert_eq!(refs.len(), 3, "unexpected rows: {refs:?}");
    }

    #[test]
    fn export_assignment_and_default_read_local_names() {
        let plugin = TypeScriptPlugin;
        let refs = plugin
            .extract_references(
                test_path(),
                "class Store {}\nfunction helper() { return 1; }\nexport = Store;\nexport default helper;\nexport const x = 1;\nexport default function direct() {}\n",
            )
            .unwrap();
        let named = |n: &str| -> Vec<_> { refs.iter().filter(|r| r.name == n).collect() };

        assert_eq!(named("Store").len(), 1, "refs: {refs:?}");
        assert_eq!(named("Store")[0].kind, ReferenceKind::Value);
        assert_eq!(named("helper").len(), 1, "refs: {refs:?}");
        assert_eq!(named("helper")[0].kind, ReferenceKind::Value);
        // Declarations bind, never read.
        assert!(named("x").is_empty(), "refs: {refs:?}");
        assert!(named("direct").is_empty(), "refs: {refs:?}");
        assert_eq!(refs.len(), 2, "unexpected rows: {refs:?}");
    }

    #[test]
    fn default_values_and_field_initializers_read() {
        let plugin = TypeScriptPlugin;
        let src = "function run(a: number = dflt, [b = e1]: any = e2) {\n  const { c = e3 } = cfg;\n  return a;\n}\nclass Store {\n  limit = seed;\n  sized: number = size;\n}\nconst id = (z: number = dz) => z;\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();
        let named = |n: &str| -> Vec<_> { refs.iter().filter(|r| r.name == n).collect() };

        for d in ["dflt", "e1", "e2", "e3", "cfg", "seed", "size", "dz", "z"] {
            assert_eq!(named(d).len(), 1, "{d}: {refs:?}");
            assert_eq!(named(d)[0].kind, ReferenceKind::Value);
        }
        assert_eq!(
            (named("dflt")[0].start_line, named("dflt")[0].start_col),
            (1, 26)
        );
        // Bound patterns and field names bind, never read (`a` reads
        // once at its return).
        for bound in ["b", "c", "limit", "sized"] {
            assert!(named(bound).is_empty(), "{bound}: {refs:?}");
        }
        assert_eq!(named("a").len(), 1, "refs: {refs:?}");
    }

    #[test]
    fn type_alias_bodies_read_named_types_once() {
        let plugin = TypeScriptPlugin;
        let src = "type Elem<T> = T extends Array<infer U> ? U : Fallback;\ntype Pair2<A, B = Second> = [A, B];\ntype Fn = <T>(x: T) => T;\ntype Key = `data-${Id}`;\n";
        let refs = plugin.extract_references(test_path(), src).unwrap();
        let named = |n: &str| -> Vec<_> { refs.iter().filter(|r| r.name == n).collect() };

        assert_eq!(named("T").len(), 3, "refs: {refs:?}");
        assert!(named("T").iter().all(|r| r.kind == ReferenceKind::Type));
        // The `infer` use reads; its binding never does.
        assert_eq!(named("U").len(), 1);
        assert_eq!(named("Array").len(), 1);
        assert_eq!(named("Fallback").len(), 1);
        assert_eq!(named("A").len(), 1);
        assert_eq!(named("B").len(), 1);
        assert_eq!(named("Second").len(), 1);
        // Template-type holes (`template_type`, not the value-level
        // `template_substitution`) read as types.
        assert_eq!(named("Id").len(), 1);
        assert_eq!(named("Id")[0].kind, ReferenceKind::Type);
        // Parameter patterns bind, never read.
        assert!(named("x").is_empty(), "refs: {refs:?}");
        // Body uses sit inside the alias scope, so impact attributes
        // them to the alias symbol rather than the file.
        assert_eq!(named("U")[0].container, "src/auth/service::Elem");
        assert_eq!(named("Second")[0].container, "src/auth/service::Pair2");
        // No strays: 3 T + U + Array + Fallback + A + B + Second + Id.
        assert_eq!(refs.len(), 10, "unexpected rows: {refs:?}");
    }

    #[test]
    fn tsx_expressions_read_bare_identifiers_once() {
        let plugin = TsxPlugin;
        let src = "import { W } from \"./w\";\nfunction render(limit: number, other: any) {\n  return <W timeout={limit} size={limit * 2} label={other.name}>{limit}</W>;\n}\n";
        let refs = plugin.extract_references(Path::new("src/render.tsx"), src).unwrap();
        let named = |n: &str| -> Vec<_> { refs.iter().filter(|r| r.name == n).collect() };

        assert_eq!(named("limit").len(), 3, "refs: {refs:?}");
        assert!(named("limit").iter().all(|r| r.kind == ReferenceKind::Value));
        assert!(named("timeout").is_empty());
        assert!(named("size").is_empty());
        assert!(named("label").is_empty());
        assert_eq!(named("name").len(), 1);
        assert_eq!(named("other").len(), 1);
        assert_eq!(named("W").len(), 1);
        assert_eq!(refs.len(), 6, "unexpected rows: {refs:?}");
    }
}
