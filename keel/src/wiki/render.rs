//! Wiki markdown rendering: fenced regions, pages, and mermaid diagrams.
//!
//! Every builder is a pure function of its inputs (no I/O, no time), so
//! identical inputs render byte-identical pages. Human safety comes from
//! [`crate::wiki::gen_start`] / [`crate::wiki::gen_end`] markers: only the
//! enclosed region is ever rewritten.

use crate::wiki::{gen_end, gen_start};

/// Cap on dependency edges in the architecture mermaid diagram.
pub const MAX_ARCH_EDGES: usize = 40;
/// Cap on external modules listed on the architecture page.
pub const MAX_ARCH_EXTERNALS: usize = 20;
/// Cap on symbols listed per module page (ranked by reference count).
pub const MAX_MODULE_SYMBOLS: usize = 50;
/// Cap on call edges per flow page / module section.
pub const MAX_FLOW_EDGES: usize = 15;
/// Cap on dependency entries per module page.
pub const MAX_MODULE_DEPS: usize = 30;
/// Cap on linked knowledge rows per module page.
pub const MAX_MODULE_KNOWLEDGE: usize = 10;

/// Escape markdown-significant characters in prose and table cells.
pub fn escape_md(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(
            c,
            '\\' | '`' | '*' | '_' | '{' | '}' | '[' | ']' | '(' | ')' | '#' | '+' | '-' | '.'
                | '!'
                | '|'
                | '<' | '>'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Escape for code spans: only backslash and backtick are significant there.
/// Using [`escape_md`] inside backticks would litter ids and paths with
/// pointless backslashes (`adr\.jwt`).
pub fn escape_code(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\\' | '`') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A markdown link, angle-wrapping targets with spaces or parens.
pub fn md_link(text: &str, target: &str) -> String {
    if target.contains([' ', '(', ')']) {
        format!("[{text}](<{target}>)")
    } else {
        format!("[{text}]({target})")
    }
}

/// Wrap generated markdown in the page's managed region.
pub fn fenced(page_id: &str, inner: &str) -> String {
    format!("{}\n{inner}\n{}\n", gen_start(page_id), gen_end())
}

/// Replace the managed region in `existing`, preserving surrounding text.
/// Returns `None` when the markers are missing or corrupt (the caller then
/// reports the page `manual` instead of touching it).
pub fn replace_region(existing: &str, page_id: &str, inner: &str) -> Option<String> {
    let start = gen_start(page_id);
    let start_at = existing.find(start.as_str())?;
    let after_start = start_at + start.len();
    let end_at = existing[after_start..].find(gen_end())? + after_start;
    let end_len = gen_end().len();
    let mut out = String::with_capacity(existing.len() + inner.len());
    out.push_str(&existing[..after_start]);
    out.push('\n');
    out.push_str(inner);
    out.push('\n');
    out.push_str(gen_end());
    out.push_str(&existing[end_at + end_len..]);
    Some(out)
}

/// Whether `text` carries an intact managed region for `page_id`.
pub fn has_region(text: &str, page_id: &str) -> bool {
    let start = gen_start(page_id);
    match text.find(start.as_str()) {
        Some(at) => text[at + start.len()..].contains(gen_end()),
        None => false,
    }
}

/// Text outside the managed region (human content), trimmed. `None` when the
/// region is missing or corrupt.
pub fn human_content(text: &str, page_id: &str) -> Option<String> {
    let start = gen_start(page_id);
    let start_at = text.find(start.as_str())?;
    let after_start = start_at + start.len();
    let end_at = text[after_start..].find(gen_end())? + after_start;
    let mut out = String::from(&text[..start_at]);
    out.push_str(&text[end_at + gen_end().len()..]);
    let trimmed = out.trim().to_string();
    Some(trimmed)
}

/// Sanitize a mermaid node label (no quotes/newlines/backticks).
fn mermaid_label(text: &str) -> String {
    text.replace('"', "'")
        .replace(['\n', '\r', '`', '[', ']'], " ")
}

/// A deterministic `flowchart LR` block: node ids follow sorted node order,
/// edges are deduplicated and sorted. Empty edge sets render a placeholder
/// node so the block still parses.
pub fn mermaid_flowchart(nodes: &[String], edges: &[(String, String)]) -> String {
    let mut sorted_nodes: Vec<String> = nodes.to_vec();
    sorted_nodes.sort();
    sorted_nodes.dedup();
    let mut sorted_edges: Vec<(String, String)> = edges.to_vec();
    sorted_edges.sort();
    sorted_edges.dedup();
    let mut out = String::from("```mermaid\nflowchart LR\n");
    if sorted_nodes.is_empty() {
        out.push_str("    none[\"no relationships recorded\"]\n");
    }
    for (i, node) in sorted_nodes.iter().enumerate() {
        out.push_str(&format!("    n{i}[\"{}\"]\n", mermaid_label(node)));
    }
    for (from, to) in &sorted_edges {
        let a = sorted_nodes.iter().position(|n| n == from);
        let b = sorted_nodes.iter().position(|n| n == to);
        if let (Some(a), Some(b)) = (a, b) {
            out.push_str(&format!("    n{a} --> n{b}\n"));
        }
    }
    out.push_str("```\n");
    out
}

/// Page footer: inputs pin, generator version, and regeneration hint. No
/// timestamps — pages stay byte-stable across runs with identical inputs.
pub fn footer(inputs_short: &str) -> String {
    format!(
        "---\n*Generated structure (inputs `{inputs_short}`, Keel {}) — deterministic, no LLM prose. Refresh with `keel wiki update`; notes outside the managed markers are preserved.*\n",
        env!("CARGO_PKG_VERSION")
    )
}

// ---------------------------------------------------------------------------
// Page data + builders.
// ---------------------------------------------------------------------------

/// One module row on the index page.
pub struct ModuleEntry {
    /// Source directory, relative (`src/auth`, or `(root)`).
    pub dir: String,
    /// Page slug (`src-auth`).
    pub slug: String,
    /// Indexed file count.
    pub files: usize,
    /// Defined symbol count.
    pub symbols: usize,
}

/// Inputs for the index page.
pub struct IndexData {
    /// Indexed file count.
    pub files: usize,
    /// Defined symbol count.
    pub symbols: usize,
    /// Language label → file count, ordered.
    pub languages: Vec<(String, usize)>,
    /// Module rows, ordered by dir.
    pub modules: Vec<ModuleEntry>,
    /// Knowledge record count.
    pub knowledge_total: usize,
    /// Status label → count, ordered.
    pub knowledge_by_status: Vec<(String, usize)>,
    /// Short inputs hash for the footer.
    pub inputs_short: String,
}

/// Render `index.md` inner content.
pub fn render_index(data: &IndexData) -> String {
    let mut out = String::from("# Project wiki\n\n");
    out.push_str("Source-linked architecture documentation generated from the Keel index. ");
    out.push_str("Every section cites indexed paths; counts update with `keel wiki update`.\n\n");
    out.push_str("## Overview\n\n");
    out.push_str(&format!(
        "- Files indexed: {}\n- Symbols defined: {}\n- Knowledge records: {}\n",
        data.files, data.symbols, data.knowledge_total
    ));
    if !data.languages.is_empty() {
        out.push_str("\n## Languages\n\n| Language | Files |\n|---|---|\n");
        for (language, count) in &data.languages {
            out.push_str(&format!("| {} | {} |\n", escape_md(language), count));
        }
    }
    out.push_str("\n## Modules\n\n| Directory | Files | Symbols | Page |\n|---|---|---|---|\n");
    for module in &data.modules {
        out.push_str(&format!(
            "| `{}` | {} | {} | {} |\n",
            escape_code(&module.dir),
            module.files,
            module.symbols,
            md_link("page", &format!("modules/{}.md", module.slug))
        ));
    }
    if !data.knowledge_by_status.is_empty() {
        out.push_str("\n## Knowledge freshness\n\n| Status | Records |\n|---|---|\n");
        for (status, count) in &data.knowledge_by_status {
            out.push_str(&format!("| {} | {} |\n", escape_md(status), count));
        }
    }
    out.push_str("\n## Pages\n\n");
    out.push_str(&format!("- {} (module dependency graph)\n", md_link("Architecture", "architecture.md")));
    out.push_str(&format!("- {} (static call relationships)\n", md_link("Flows", "flows/index.md")));
    out.push_str(&format!("- {} (decisions and constraints)\n", md_link("Decisions", "decisions/index.md")));
    out.push('\n');
    out.push_str(&footer(&data.inputs_short));
    out
}

/// Inputs for the architecture page.
pub struct ArchData {
    /// Directory-level dependency edges (from, to), pre-capped.
    pub edges: Vec<(String, String)>,
    /// Edges in total (before the cap).
    pub edge_total: usize,
    /// External (unindexed) modules, pre-capped.
    pub externals: Vec<String>,
    /// Externals in total (before the cap).
    pub externals_total: usize,
    /// Short inputs hash for the footer.
    pub inputs_short: String,
}

/// Render `architecture.md` inner content.
pub fn render_architecture(data: &ArchData) -> String {
    let mut out = String::from("# Architecture\n\n");
    out.push_str("Static module relationships derived from imports and resolved references. ");
    out.push_str("Directions read *depends on*; this is build-time structure, not observed runtime behavior.\n\n");
    out.push_str("## Dependency graph\n\n");
    let nodes: Vec<String> = {
        let mut all: Vec<String> = data
            .edges
            .iter()
            .flat_map(|(a, b)| [a.clone(), b.clone()])
            .collect();
        all.sort();
        all.dedup();
        all
    };
    out.push_str(&mermaid_flowchart(&nodes, &data.edges));
    out.push('\n');
    if data.edge_total > data.edges.len() {
        out.push_str(&format!(
            "*Showing {} of {} edges (highest-connectivity first).*\n\n",
            data.edges.len(),
            data.edge_total
        ));
    }
    if !data.externals.is_empty() {
        out.push_str("## External dependencies\n\nUnindexed modules (stdlib / third-party):\n\n");
        for external in &data.externals {
            out.push_str(&format!("- `{}`\n", escape_code(external)));
        }
        if data.externals_total > data.externals.len() {
            out.push_str(&format!(
                "\n*Showing {} of {} externals.*\n",
                data.externals.len(),
                data.externals_total
            ));
        }
        out.push('\n');
    }
    out.push_str(&footer(&data.inputs_short));
    out
}

/// One symbol row on a module page.
pub struct SymbolRow {
    /// Symbol name.
    pub name: String,
    /// Kind label.
    pub kind: String,
    /// Defining file.
    pub path: String,
    /// 1-based definition line.
    pub line: u32,
    /// Recorded reference count.
    pub refs: usize,
}

/// One knowledge row on module/decision pages.
pub struct KnowledgeRow {
    /// Record id.
    pub id: String,
    /// Record title.
    pub title: String,
    /// Stored status.
    pub status: String,
    /// Origin label.
    pub origin: String,
    /// Live freshness verdict.
    pub fresh: bool,
    /// Evidence citations (`path` or `path (symbol)`).
    pub evidence: Vec<String>,
}

/// Inputs for one module page.
pub struct ModuleData {
    /// Source directory, relative.
    pub dir: String,
    /// Files in the directory, ordered.
    pub files: Vec<String>,
    /// Top top-level symbols by reference count, pre-capped.
    pub symbols: Vec<SymbolRow>,
    /// Top-level symbols in total (the ranked pool, before the cap).
    pub ranked_total: usize,
    /// All defined symbols including members (header count).
    pub symbols_total: usize,
    /// Imported module paths, pre-capped.
    pub dependencies: Vec<String>,
    /// Dependencies in total (before the cap).
    pub dependencies_total: usize,
    /// Dependent module paths, pre-capped.
    pub dependents: Vec<String>,
    /// Dependents in total (before the cap).
    pub dependents_total: usize,
    /// Linked knowledge rows, pre-capped.
    pub knowledge: Vec<KnowledgeRow>,
    /// Linked knowledge rows in total (before the cap).
    pub knowledge_total: usize,
    /// Short inputs hash for the footer.
    pub inputs_short: String,
}

/// Render one `modules/<slug>.md` inner content.
pub fn render_module(data: &ModuleData) -> String {
    let mut out = format!("# Module `{}`\n\n", escape_code(&data.dir));
    out.push_str("Source directory grouped by location (language-agnostic): ");
    out.push_str(&format!(
        "{} file(s), {} symbol(s).\n\n",
        data.files.len(),
        data.symbols_total
    ));
    out.push_str("## Files\n\n");
    for file in &data.files {
        out.push_str(&format!("- `{}`\n", escape_code(file)));
    }
    out.push_str("\n## Key symbols\n\nTop top-level symbols by recorded reference count (members excluded):\n\n");
    out.push_str("| Symbol | Kind | Defined | References |\n|---|---|---|---|\n");
    for symbol in &data.symbols {
        out.push_str(&format!(
            "| `{}` | {} | `{}`:{} | {} |\n",
            escape_code(&symbol.name),
            escape_md(&symbol.kind),
            escape_code(&symbol.path),
            symbol.line,
            symbol.refs
        ));
    }
    if data.ranked_total > data.symbols.len() {
        out.push_str(&format!(
            "\n*Showing {} of {} top-level symbols.*\n",
            data.symbols.len(),
            data.ranked_total
        ));
    }
    out.push_str("\n## Dependencies\n\n");
    if data.dependencies.is_empty() {
        out.push_str("No imports recorded.\n");
    } else {
        for dep in &data.dependencies {
            out.push_str(&format!("- `{}`\n", escape_code(dep)));
        }
        if data.dependencies_total > data.dependencies.len() {
            out.push_str(&format!(
                "\n*Showing {} of {}.*\n",
                data.dependencies.len(),
                data.dependencies_total
            ));
        }
    }
    out.push_str("\n## Dependents\n\n");
    if data.dependents.is_empty() {
        out.push_str("No indexed dependents recorded.\n");
    } else {
        for dep in &data.dependents {
            out.push_str(&format!("- `{}`\n", escape_code(dep)));
        }
        if data.dependents_total > data.dependents.len() {
            out.push_str(&format!(
                "\n*Showing {} of {}.*\n",
                data.dependents.len(),
                data.dependents_total
            ));
        }
    }
    if !data.knowledge.is_empty() {
        out.push_str("\n## Linked knowledge\n\n");
        for row in &data.knowledge {
            let verdict = if row.fresh { "fresh" } else { "drifted" };
            out.push_str(&format!(
                "- `{}` [{}|{}] {} — {}\n",
                escape_code(&row.id),
                escape_md(&row.status),
                verdict,
                escape_md(&row.origin),
                escape_md(&row.title)
            ));
            for citation in &row.evidence {
                out.push_str(&format!("  - evidence: `{}`\n", escape_code(citation)));
            }
        }
        if data.knowledge_total > data.knowledge.len() {
            out.push_str(&format!(
                "\n*Showing {} of {} linked records.*\n",
                data.knowledge.len(),
                data.knowledge_total
            ));
        }
    }
    out.push('\n');
    out.push_str(&footer(&data.inputs_short));
    out
}

/// Inputs for the flows index + one flow page.
pub struct FlowData {
    /// Source directory, relative.
    pub dir: String,
    /// Static call edges (caller, callee, sites), pre-capped.
    pub edges: Vec<(String, String, usize)>,
    /// Edges in total (before the cap).
    pub edge_total: usize,
    /// Short inputs hash for the footer.
    pub inputs_short: String,
}

/// Render one `flows/<slug>.md` inner content.
pub fn render_flow(data: &FlowData) -> String {
    let mut out = format!("# Flow `{}`\n\n", escape_code(&data.dir));
    out.push_str("> Static call relationships from the index — **not observed runtime behavior**. ");
    out.push_str("Edges aggregate recorded call sites; counts are sites, not executions.\n\n");
    if data.edges.is_empty() {
        out.push_str("No call relationships recorded for this directory.\n\n");
    } else {
        out.push_str("## Call graph\n\n");
        let nodes: Vec<String> = {
            let mut all: Vec<String> = data
                .edges
                .iter()
                .flat_map(|(a, b, _)| [a.clone(), b.clone()])
                .collect();
            all.sort();
            all.dedup();
            all
        };
        let pairs: Vec<(String, String)> = data
            .edges
            .iter()
            .map(|(a, b, _)| (a.clone(), b.clone()))
            .collect();
        out.push_str(&mermaid_flowchart(&nodes, &pairs));
        out.push('\n');
        out.push_str("## Significant edges\n\n| Caller | Callee | Sites |\n|---|---|---|\n");
        for (caller, callee, sites) in &data.edges {
            out.push_str(&format!(
                "| `{}` | `{}` | {} |\n",
                escape_code(caller),
                escape_code(callee),
                sites
            ));
        }
        if data.edge_total > data.edges.len() {
            out.push_str(&format!(
                "\n*Showing {} of {} edges (most sites first).*\n",
                data.edges.len(),
                data.edge_total
            ));
        }
        out.push('\n');
    }
    out.push_str(&footer(&data.inputs_short));
    out
}

/// Render `flows/index.md` inner content.
pub fn render_flows_index(dirs: &[(String, String)], inputs_short: &str) -> String {
    let mut out = String::from("# Flows\n\n");
    out.push_str("Static call relationships per directory. These pages describe what the code ");
    out.push_str("*can* call (recorded call sites) — never what it *did* call at runtime.\n\n");
    for (dir, slug) in dirs {
        out.push_str(&format!(
            "- {} (`{}`)\n",
            md_link(&escape_md(dir), &format!("{slug}.md")),
            escape_code(dir)
        ));
    }
    out.push('\n');
    out.push_str(&footer(inputs_short));
    out
}

/// Inputs for the decisions index.
pub struct DecisionData {
    /// Decision/constraint rows, ordered by id.
    pub records: Vec<KnowledgeRow>,
    /// Record kinds covered.
    pub kinds: Vec<String>,
    /// Short inputs hash for the footer.
    pub inputs_short: String,
}

/// Render `decisions/index.md` inner content.
pub fn render_decisions(data: &DecisionData) -> String {
    let mut out = String::from("# Decisions\n\n");
    out.push_str("Architecture decisions and constraints from the knowledge store ");
    out.push_str(&format!("({}). ", data.kinds.join(", ")));
    out.push_str("Statuses are live: `supported` + fresh is the strongest signal; ");
    out.push_str("`stale` means pinned evidence verifiably drifted.\n\n");
    if data.records.is_empty() {
        out.push_str("No decision or constraint records stored yet.\n\n");
    }
    for row in &data.records {
        let verdict = if row.fresh { "fresh" } else { "drifted" };
        out.push_str(&format!(
            "## `{}`\n\n- Status: `{}` ({})\n- Origin: `{}`\n- Title: {}\n",
            escape_code(&row.id),
            escape_md(&row.status),
            verdict,
            escape_md(&row.origin),
            escape_md(&row.title)
        ));
        if !row.evidence.is_empty() {
            out.push_str("- Evidence:\n");
            for citation in &row.evidence {
                out.push_str(&format!("  - `{}`\n", escape_code(citation)));
            }
        }
        out.push('\n');
    }
    out.push_str(&footer(&data.inputs_short));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_replace_and_detect_human_content() {
        let fenced_text = fenced("modules/x", "BODY");
        assert!(has_region(&fenced_text, "modules/x"));
        assert!(!has_region(&fenced_text, "modules/y"));
        assert_eq!(human_content(&fenced_text, "modules/x").unwrap(), "");

        let with_human = format!("# Notes\n\nMy note.\n\n{fenced_text}\nMore below.\n");
        let replaced = replace_region(&with_human, "modules/x", "NEW").unwrap();
        assert!(replaced.contains("My note."));
        assert!(replaced.contains("More below."));
        assert!(replaced.contains("NEW"));
        assert!(!replaced.contains("BODY"));
        // Markers survive the replacement (idempotent: replacing again with
        // the same body is a byte-identical no-op).
        assert!(replaced.contains(&gen_start("modules/x")));
        assert!(replaced.contains(gen_end()));
        assert_eq!(
            replace_region(&replaced, "modules/x", "NEW").unwrap(),
            replaced
        );
        // Human content is byte-faithful (ends trimmed only).
        assert_eq!(
            human_content(&with_human, "modules/x").unwrap(),
            "# Notes\n\nMy note.\n\n\n\nMore below."
        );

        // Missing or corrupt markers: no replacement, manual handling.
        assert!(replace_region("no markers", "modules/x", "NEW").is_none());
        assert!(replace_region(&gen_start("modules/x"), "modules/x", "NEW").is_none());
        assert!(!has_region("no markers", "modules/x"));
    }

    #[test]
    fn mermaid_is_deterministic_and_safe() {
        let nodes = vec!["b".to_string(), "a".to_string()];
        let edges = vec![
            ("b".to_string(), "a".to_string()),
            ("a".to_string(), "b".to_string()),
            ("b".to_string(), "a".to_string()),
        ];
        let first = mermaid_flowchart(&nodes, &edges);
        let second = mermaid_flowchart(&nodes, &edges);
        assert_eq!(first, second);
        // Sorted nodes: a=n0, b=n1; deduped edges.
        assert!(first.contains("n0[\"a\"]"));
        assert!(first.contains("n0 --> n1"));
        assert_eq!(first.matches("-->").count(), 2);
        // Quotes cannot break out of labels.
        let evil = mermaid_flowchart(&["x\" --> y".to_string()], &[]);
        assert!(!evil.contains("\" --> "));
    }

    #[test]
    fn links_and_escapes_hold() {
        assert_eq!(md_link("p", "a/b.md"), "[p](a/b.md)");
        assert_eq!(md_link("p", "a b.md"), "[p](<a b.md>)");
        assert_eq!(escape_md("a_b[mid]"), "a\\_b\\[mid\\]");
        // Code spans stay readable: dots and dashes pass through.
        assert_eq!(escape_code("adr.jwt"), "adr.jwt");
        assert_eq!(escape_code("a`b\\c"), "a\\`b\\\\c");
    }
}
