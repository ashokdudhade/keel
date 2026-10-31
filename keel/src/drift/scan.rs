//! Fence-aware extraction of checkable claims from Markdown docs.
//!
//! A drift scan reads human prose, not code: fenced code blocks are samples,
//! not claims about the repo, so spans inside them are skipped. What remains
//! are single-backtick spans — the way docs name symbols (`serve`) and files
//! (`src/auth/service.ts`). Each span is classified as a symbol claim, a
//! path claim, or noise (flags, prose quotes, versions).

/// A single backtick span found outside fenced code blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimSpan {
    /// 1-based line number within the doc.
    pub line: u32,
    /// Span text, trimmed. Backticks are stripped by the extractor.
    pub text: String,
    /// Lowercased alphanumeric word immediately before the span on the
    /// same line, when present (e.g. `trait` in "the trait `Store`").
    /// The checker maps this against a kind vocabulary.
    pub kind_word: Option<String>,
}

/// Classification of a span's claim about the repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimKind {
    /// A symbol reference: `serve`, `Widget.show`, `crate::mcp::serve`.
    /// Carries the bare lookup name (last segment).
    Symbol {
        /// Bare name to look up in the index (`show` for `Widget.show`).
        name: String,
        /// Member qualifier for single-dot form (`Widget` for
        /// `Widget.show`); checked against `symbols.container`.
        container: Option<String>,
    },
    /// A file reference: `src/lib.rs`, `docs/../x.py`. Carries raw text;
    /// the checker resolves it against the doc dir, then the project root.
    Path {
        /// Raw span text, untouched.
        raw: String,
    },
    /// Not a checkable claim: flags (`--json`), prose quotes, versions,
    /// empty spans.
    Ignore,
}

/// Extract backtick spans from Markdown, skipping fenced code blocks.
///
/// Fences are lines whose trimmed form starts with three or more
/// backticks or tildes; backtick and tilde fences track independently so
/// a `~~~` sample inside a ```` ``` ```` block does not close it early.
/// Span detection is a naive backtick split (odd segments are spans),
/// which also degrades gracefully on ` `` ` double-backtick spans.
pub fn extract_spans(text: &str) -> Vec<ClaimSpan> {
    let mut spans = Vec::new();
    let mut in_back_fence = false;
    let mut in_tilde_fence = false;
    for (idx, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_back_fence = !in_back_fence;
            continue;
        }
        if trimmed.starts_with("~~~") {
            in_tilde_fence = !in_tilde_fence;
            continue;
        }
        if in_back_fence || in_tilde_fence {
            continue;
        }
        // Walk backtick pairs, keeping the byte offset so the kind word
        // (text before the span on this line) is recoverable.
        let mut cursor = 0usize;
        for (part_idx, part) in line.split('`').enumerate() {
            let part_start = cursor;
            cursor += part.len() + 1; // +1 for the consumed backtick
            if part_idx % 2 == 0 {
                continue; // outside a span
            }
            let span_text = part.trim();
            if span_text.is_empty() {
                continue;
            }
            // `part_start` sits one past the opening backtick (the split
            // consumed it), so the text before the span excludes it.
            let before = &line[..part_start.saturating_sub(1).min(line.len())];
            spans.push(ClaimSpan {
                line: (idx + 1) as u32,
                text: span_text.to_string(),
                kind_word: last_word(before),
            });
        }
    }
    spans
}

/// Last alphanumeric word of `text`, lowercased; `None` when the text
/// ends in nothing word-like, or when anything but whitespace sits
/// between the word and the span (`trait + \`R\`` attributes the kind
/// to whatever precedes the `+`, not to `R`).
fn last_word(text: &str) -> Option<String> {
    let end = text
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_alphanumeric())
        .map(|(i, c)| i + c.len_utf8())?;
    if !text[end..].chars().all(|c| c.is_whitespace()) {
        return None;
    }
    let start = text[..end]
        .char_indices()
        .rev()
        .find(|(_, c)| !c.is_alphanumeric() && *c != '_')
        .map_or(0, |(i, c)| i + c.len_utf8());
    let word = &text[start..end];
    if word.is_empty() {
        None
    } else {
        Some(word.to_lowercase())
    }
}

/// Source and doc extensions that mark a slashless span as a path claim
/// (`foo.rs`, `adr-003.md`). Compared case-insensitively.
const PATH_EXTENSIONS: &[&str] = &[
    "rs", "py", "pyi", "js", "jsx", "ts", "tsx", "mjs", "cjs", "mts", "cts", "go", "mod",
    "sum", "md", "markdown", "rst", "txt", "json", "yaml", "yml", "toml", "cfg", "ini",
    "lock", "sh", "rb", "java", "c", "h", "cpp", "hpp", "cs", "php", "swift", "kt", "png",
    "svg", "db", "sqlite", "sqlite3", "log", "port", "jsonl",
];

/// True for version tags: a leading `v`/`V` plus a digit or `X`,
/// with a dot separating release segments (`v1.4.1`, `vX.Y.Z`).
fn is_version_tag(text: &str) -> bool {
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some(v), Some(second))
            if (v == 'v' || v == 'V') && (second.is_ascii_digit() || second == 'X' || second == 'x') => {}
        _ => return false,
    }
    text.contains('.')
}

/// Language keywords (Rust, Python, JS/TS, Go) that can never be drift:
/// on an index miss they read as language mentions, not removed symbols.
/// Soft/contextual keywords that double as plausible names (`of`, `get`,
/// `set`, `next`) stay out — only words no definition could carry.
/// Compared lowercased.
const KEYWORDS: &[&str] = &[
    // Rust (strict + reserved + weak).
    "abstract", "as", "async", "await", "become", "box", "break", "const", "continue", "crate",
    "do", "dyn", "else", "enum", "extern", "false", "final", "fn", "for", "gen", "if", "impl",
    "in", "let", "loop", "macro", "match", "mod", "move", "mut", "override", "priv", "pub",
    "raw", "ref", "return", "safe", "self", "static", "struct", "super", "trait", "true", "try",
    "type", "typeof", "union", "unsafe", "unsized", "use", "virtual", "where", "while", "yield",
    // Python.
    "and", "assert", "class", "def", "del", "elif", "except", "finally", "from", "global",
    "import", "is", "lambda", "none", "nonlocal", "not", "or", "pass", "raise", "with",
    // JS/TS.
    "any", "asserts", "boolean", "catch", "constructor", "debugger", "declare", "default",
    "delete", "export", "extends", "function", "implements", "infer", "instanceof", "interface",
    "keyof", "new", "null", "number", "object", "package", "private", "protected", "public",
    "readonly", "satisfies", "string", "switch", "symbol", "this", "throw", "undefined",
    "unknown", "var", "void",
    // Go.
    "break", "case", "chan", "defer", "fallthrough", "func", "go", "goto", "map", "range",
    "select",
];

/// Bare extension shorthands (`` `ts` ``, `` `go` ``) that read as language
/// mentions on an index miss, not removed symbols. Compared lowercased.
/// Short names that double as plausible symbols (`sum`, `lock`, `txt`)
/// stay out — those check normally.
const EXT_SHORTHANDS: &[&str] = &[
    "cjs", "cpp", "cs", "cts", "go", "hpp", "java", "js", "json", "jsx", "kt", "markdown", "md",
    "mjs", "mod", "mts", "php", "py", "pyi", "rb", "rs", "rst", "svg", "swift", "toml", "ts",
    "tsx", "yaml", "yml",
];

/// Toolchain binaries cited in setup prose (`` `cargo` ``, `` `rg` ``).
/// Compared lowercased.
const TOOLCHAIN: &[&str] = &[
    "ansible", "awk", "bash", "brew", "bun", "cargo", "cc", "clang", "cmake", "curl", "deno",
    "docker", "eslint", "fish", "gcc", "gh", "git", "glab", "grep", "jest", "jq", "kubectl",
    "make", "mypy", "node", "npm", "npx", "pip", "pip3", "podman", "pwsh", "pytest", "python",
    "python3", "rg", "ruff", "rustc", "rustup", "scp", "sed", "sh", "sqlite", "sqlite3", "ssh",
    "tar", "terraform", "tsc", "unzip", "vitest", "wget", "zip", "zsh",
];

/// IANA media-type top levels: `text/html` in prose is a MIME type, not
/// a repo path. Only un-dotted two-segment spans match, so real paths
/// like `model/user.py` still check.
const MIME_TOP_LEVELS: &[&str] = &[
    "application",
    "audio",
    "font",
    "image",
    "message",
    "model",
    "multipart",
    "text",
    "video",
];

/// True for MIME-shaped spans: two segments, a known top level, and no
/// dot in the subtype (`text/html`, not `model/user.py`).
fn is_mime_shape(text: &str) -> bool {
    match text.split_once('/') {
        Some((top, sub))
            if !sub.contains('/') && !sub.contains('.') && !sub.is_empty() =>
        {
            MIME_TOP_LEVELS.contains(&top.to_lowercase().as_str())
        }
        _ => false,
    }
}

/// True when a backslash starts an escape sequence (`\t`, `\n`): format
/// strings, not Windows paths (whose backslashes precede names).
fn has_escape(text: &str) -> bool {
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                if matches!(next, 't' | 'n' | 'r' | '0' | '\\' | '\'' | '"') {
                    return true;
                }
            }
        }
    }
    false
}

/// Bare MCP protocol methods (the slashy ones live in path shape).
const MCP_METHODS: &[&str] = &["initialize", "ping"];

/// MCP protocol paths cited as slashy spans (`tools/call`).
pub(crate) const MCP_PATHS: &[&str] = &[
    "initialize",
    "notifications/cancelled",
    "notifications/initialized",
    "prompts/get",
    "prompts/list",
    "resources/list",
    "resources/read",
    "tools/call",
    "tools/list",
];

/// Keel-generated runtime files whose absence is not drift: they exist
/// only while the daemon, watcher, or wiki has run (`index.db`,
/// `projects.json`, `watch.log`, `usage.jsonl`, `insights.port`,
/// `.keel-wiki.json`). Compared by basename, exact case.
pub(crate) const RUNTIME_FILES: &[&str] = &[
    ".keel-wiki.json",
    "index.db",
    "insights.port",
    "projects.json",
    "usage.jsonl",
    "usage.jsonl.1",
    "watch.log",
];

/// True when a missing bare name reads as language/tooling mention
/// rather than drift (keywords, extension shorthands, toolchain
/// binaries, bare MCP methods). Only consulted on an index miss —
/// a real definition always wins.
pub fn is_reserved_word(name: &str) -> bool {
    let lower = name.to_lowercase();
    let word = lower.as_str();
    KEYWORDS.contains(&word)
        || EXT_SHORTHANDS.contains(&word)
        || TOOLCHAIN.contains(&word)
        || MCP_METHODS.contains(&word)
}

/// Classify a span as a symbol claim, a path claim, or noise.
pub fn classify(span: &str) -> ClaimKind {
    let mut text = span.trim();
    while let Some(stripped) = text.strip_suffix("()") {
        text = stripped.trim_end();
    }
    if text.is_empty() {
        return ClaimKind::Ignore;
    }
    // Shell commands, HTTP routes, commit messages, and sample output
    // all contain whitespace; real repo paths in backticks don't.
    if text.chars().any(|c| c.is_whitespace()) {
        return ClaimKind::Ignore;
    }
    // URLs, absolute/machine paths, home-relative paths, templates
    // (`<project>/…`), globs (`src/*`, `{a,b}`), assignments (`A=/b`),
    // quoted fragments, and MIME types are never repo claims.
    if text.contains("://")
        || text.starts_with('/')
        || text.starts_with('~')
        || text.contains(['<', '>', '*', '?', '{', '}', '[', ']', '=', '\'', '"'])
        || is_mime_shape(text)
        || has_escape(text)
    {
        return ClaimKind::Ignore;
    }
    // Extension lists (`.ts/.tsx`) and leading-dot shorthands (`.rs`)
    // read as extension mentions, not file claims; dotted paths
    // (`./x`, `.tar.gz`) still check as paths.
    if text.contains('/') {
        if text.split('/').all(|seg| seg.starts_with('.')) {
            return ClaimKind::Ignore;
        }
    } else if let Some(rest) = text.strip_prefix('.') {
        if !rest.contains(['/', '.', '\\']) {
            return ClaimKind::Ignore;
        }
    }
    // Version tags (`v1.4.1`, `vX.Y.Z`) are releases, not symbols.
    if is_version_tag(text) {
        return ClaimKind::Ignore;
    }
    if text.contains(['/', '\\']) {
        return ClaimKind::Path {
            raw: text.to_string(),
        };
    }
    if has_path_extension(text) {
        return ClaimKind::Path {
            raw: text.to_string(),
        };
    }
    match bare_symbol_name(text) {
        Some(name) => {
            let container = member_container(text);
            ClaimKind::Symbol { name, container }
        }
        None => ClaimKind::Ignore,
    }
}

/// True when `text` ends in `.ext` for a known path extension.
fn has_path_extension(text: &str) -> bool {
    match text.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() => PATH_EXTENSIONS
            .iter()
            .any(|known| ext.eq_ignore_ascii_case(known)),
        _ => false,
    }
}

/// True for identifier shapes: `[A-Za-z_][A-Za-z0-9_]*`.
fn is_ident(piece: &str) -> bool {
    let mut chars = piece.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Bare lookup name for an identifier path (`a`, `A.b`, `m::C::f`),
/// or `None` when the text is not identifier-shaped.
fn bare_symbol_name(text: &str) -> Option<String> {
    if text.is_empty() || text.starts_with(['.', ':']) || text.ends_with(['.', ':']) {
        return None;
    }
    if text.contains("...") || text.contains(":::") {
        return None;
    }
    let mut pieces: Vec<&str> = Vec::new();
    for dot_piece in text.split('.') {
        for colon_piece in dot_piece.split("::") {
            if colon_piece.is_empty() || !is_ident(colon_piece) {
                return None;
            }
            pieces.push(colon_piece);
        }
    }
    // Reject single `:` separators (`a:b`): only `.` and `::` join.
    // After removing `::` pairs, any remaining `:` is a lone separator.
    if text.contains(':') && text.replace("::", "").contains(':') {
        return None;
    }
    pieces.last().map(|last| (*last).to_string())
}

/// Container qualifier for single-dot member form (`Widget` in
/// `Widget.show`), mirroring `graph::target::split_member_name`.
fn member_container(text: &str) -> Option<String> {
    crate::graph::target::split_member_name(text).map(|(head, _)| head.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_backtick_fences() {
        let text = "See `serve`.\n```rust\nlet `not_a_claim` = 1;\n```\nAnd `run`.\n";
        let spans = extract_spans(text);
        let texts: Vec<&str> = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec!["serve", "run"]);
        assert_eq!(spans[0].line, 1);
        assert_eq!(spans[1].line, 5);
    }

    #[test]
    fn skips_tilde_fences_independently() {
        let text = "```\n~~~\n`hidden`\n~~~\n```\n`shown`\n";
        let spans = extract_spans(text);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "shown");
    }

    #[test]
    fn captures_kind_word() {
        let spans = extract_spans("The trait `Store` persists.\nCall `save` now.\n");
        assert_eq!(spans[0].kind_word.as_deref(), Some("trait"));
        assert_eq!(spans[1].kind_word.as_deref(), Some("call"));
    }

    #[test]
    fn kind_word_needs_adjacency() {
        // The kind word binds across whitespace only: `trait` here
        // belongs to whatever precedes the `+`, not to `Registry`.
        let spans = extract_spans("`Plugin` trait + `Registry` ships.\n");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1].kind_word, None);
    }

    #[test]
    fn classification_table() {
        assert!(matches!(
            classify("serve"),
            ClaimKind::Symbol { .. }
        ));
        assert!(matches!(
            classify("Widget.show"),
            ClaimKind::Symbol {
                container: Some(_),
                ..
            }
        ));
        assert!(matches!(
            classify("crate::mcp::serve"),
            ClaimKind::Symbol {
                container: None,
                ..
            }
        ));
        assert!(matches!(classify("save()"), ClaimKind::Symbol { .. }));
        assert!(matches!(classify("src/lib.rs"), ClaimKind::Path { .. }));
        assert!(matches!(classify("foo.py"), ClaimKind::Path { .. }));
        assert!(matches!(classify("--json"), ClaimKind::Ignore));
        assert!(matches!(classify("never"), ClaimKind::Symbol { .. }));
        assert!(matches!(classify("v1.2"), ClaimKind::Ignore));
        assert!(matches!(classify("a:b"), ClaimKind::Ignore));
        assert!(matches!(classify(""), ClaimKind::Ignore));
        assert!(matches!(classify("has space"), ClaimKind::Ignore));
        assert!(matches!(classify(".rs"), ClaimKind::Ignore));
        assert!(matches!(classify("./x.py"), ClaimKind::Path { .. }));
        // Commands, routes, and prose carry whitespace; paths don't.
        assert!(matches!(classify("GET /health"), ClaimKind::Ignore));
        assert!(matches!(classify("cargo build --release"), ClaimKind::Ignore));
        // URLs, absolute paths, templates, and globs aren't repo claims.
        assert!(matches!(
            classify("https://example.com/x"),
            ClaimKind::Ignore
        ));
        assert!(matches!(classify("/tmp/x"), ClaimKind::Ignore));
        assert!(matches!(classify("~/.keel/x"), ClaimKind::Ignore));
        assert!(matches!(classify("<root>/x.py"), ClaimKind::Ignore));
        assert!(matches!(classify("src/*"), ClaimKind::Ignore));
        assert!(matches!(classify("src/{a,b}.rs"), ClaimKind::Ignore));
        assert!(matches!(classify("tools/call"), ClaimKind::Path { .. }));
        assert!(matches!(classify("text/html"), ClaimKind::Ignore));
        assert!(matches!(classify("application/json"), ClaimKind::Ignore));
        assert!(matches!(classify("model/user.py"), ClaimKind::Path { .. }));
        assert!(matches!(classify("A=/b/c"), ClaimKind::Ignore));
        assert!(matches!(classify("fetch('x')"), ClaimKind::Ignore));
        assert!(matches!(classify("a\\tb"), ClaimKind::Ignore));
        assert!(matches!(classify("src\\foo\\bar"), ClaimKind::Path { .. }));
        // Extension lists and versions aren't claims either.
        assert!(matches!(classify(".ts/.tsx"), ClaimKind::Ignore));
        assert!(matches!(classify("v1.4.1"), ClaimKind::Ignore));
        assert!(matches!(classify("vX.Y.Z"), ClaimKind::Ignore));
        assert!(matches!(classify("index.db"), ClaimKind::Path { .. }));
    }

    #[test]
    fn reserved_words_cover_keywords_tools_and_shorthands() {
        for word in ["use", "pub", "fn", "Self", "extends", "func", "match", "None"] {
            assert!(is_reserved_word(word), "{word}");
        }
        for word in ["ts", "tsx", "go", "rs", "py", "md"] {
            assert!(is_reserved_word(word), "{word}");
        }
        for word in ["cargo", "rg", "python3", "docker", "initialize", "ping"] {
            assert!(is_reserved_word(word), "{word}");
        }
        // Plausible symbol names stay checkable.
        for word in ["serve", "of", "sum", "lock", "next", "handler"] {
            assert!(!is_reserved_word(word), "{word}");
        }
    }

    #[test]
    fn bare_names_strip_qualifiers() {
        assert_eq!(bare_symbol_name("serve").as_deref(), Some("serve"));
        assert_eq!(bare_symbol_name("Widget.show").as_deref(), Some("show"));
        assert_eq!(
            bare_symbol_name("crate::mcp::serve").as_deref(),
            Some("serve")
        );
        assert_eq!(bare_symbol_name("a.b.c").as_deref(), Some("c"));
        assert_eq!(bare_symbol_name("2bad"), None);
        assert_eq!(bare_symbol_name(".lead"), None);
        assert_eq!(bare_symbol_name("trail."), None);
    }
}
