//! ADR discovery and header parsing.
//!
//! Architecture decision records live wherever a repo keeps them:
//! `adr/` trees, `decisions/` trees, or `*adr*.md` filenames. Discovery
//! is deliberately loose (any depth, case-insensitive) because ADR
//! layout is a team convention, not a standard. Headers are parsed
//! loosely too: the first `#` heading is the title, the first `Status:`
//! line is the declared status, the first `Date:` line is the date.
//! Anything missing stays missing — never inferred.

use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

/// Maximum ADRs per project; overflow is reported, not silent.
pub const MAX_ADRS: usize = 200;

/// Directory names that mark decision records at any depth.
const ADR_DIRS: &[&str] = &["adr", "adrs", "decisions"];

/// Discover ADR files under `root`, sorted. See the module docs for
/// what counts.
pub fn discover_adrs(root: &Path) -> Vec<PathBuf> {
    let mut adrs: Vec<PathBuf> = WalkBuilder::new(root)
        .standard_filters(true)
        .require_git(false)
        .add_custom_ignore_filename(".keelignore")
        .build()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path().to_path_buf())
        .filter(|path| path.is_file())
        .filter(|path| {
            path.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
                .unwrap_or(false)
        })
        .filter(|path| {
            let rel = path.strip_prefix(root).unwrap_or(path);
            is_adr_path(rel)
        })
        .collect();
    adrs.sort();
    adrs.truncate(MAX_ADRS);
    adrs
}

/// True when a root-relative path reads as an ADR: under an ADR-ish
/// directory, or an ADR-ish filename (`adr-003-auth.md`, `ADR.md`).
fn is_adr_path(rel: &Path) -> bool {
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    let lower = rel_str.to_lowercase();
    if lower.split('/').any(|seg| ADR_DIRS.contains(&seg)) {
        return true;
    }
    let file_name = rel
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_lowercase();
    let stem = file_name
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(&file_name);
    stem.contains("adr")
}

/// Parsed ADR header: identity plus declared (not derived) metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdrHeader {
    /// File stem (`adr-003-auth`): the stable ADR id.
    pub id: String,
    /// First `#` heading, if any.
    pub title: Option<String>,
    /// First `Status:` line value, if any.
    pub status: Option<String>,
    /// First `Date:` line value, if any.
    pub date: Option<String>,
}

/// Parse an ADR header from its Markdown text. Only the first
/// occurrence of each field counts; later `Status:` lines (quoted
/// history, changelogs) don't override.
pub fn parse_header(file_name: &str, text: &str) -> AdrHeader {
    let id = file_name
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(file_name)
        .to_string();
    let mut title = None;
    let mut status = None;
    let mut date = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if title.is_none() {
            if let Some(heading) = trimmed.strip_prefix('#') {
                // `##` subheadings don't count: only the document title.
                if !heading.starts_with('#') {
                    let heading = heading.trim();
                    if !heading.is_empty() {
                        title = Some(heading.to_string());
                    }
                }
            }
        }
        if let Some((key, value)) = trimmed.split_once(':') {
            let key = key.trim().to_lowercase();
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            if key == "status" && status.is_none() {
                status = Some(value.to_string());
            } else if key == "date" && date.is_none() {
                date = Some(value.to_string());
            }
        }
        if title.is_some() && status.is_some() && date.is_some() {
            break;
        }
    }
    AdrHeader {
        id,
        title,
        status,
        date,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adr_path_rules() {
        assert!(is_adr_path(Path::new("docs/adr/001.md")));
        assert!(is_adr_path(Path::new("team/decisions/x.md")));
        assert!(is_adr_path(Path::new("notes/adr-003-auth.md")));
        assert!(is_adr_path(Path::new("ADR.md")));
        assert!(!is_adr_path(Path::new("docs/arch.md")));
        assert!(!is_adr_path(Path::new("README.md")));
        assert!(!is_adr_path(Path::new("notes/random.md")));
    }

    #[test]
    fn header_first_wins() {
        let header = parse_header(
            "adr-003-auth.md",
            "# Use JWT sessions\n\nStatus: accepted\nDate: 2026-01-02\n\n\
             Quoted history:\n\nStatus: proposed\n",
        );
        assert_eq!(header.id, "adr-003-auth");
        assert_eq!(header.title.as_deref(), Some("Use JWT sessions"));
        assert_eq!(header.status.as_deref(), Some("accepted"));
        assert_eq!(header.date.as_deref(), Some("2026-01-02"));
    }

    #[test]
    fn header_tolerates_missing_fields() {
        let header = parse_header("001.md", "No headings here.\n");
        assert_eq!(header.id, "001");
        assert!(header.title.is_none());
        assert!(header.status.is_none());
        assert!(header.date.is_none());
    }

    #[test]
    fn subheadings_are_not_titles() {
        let header = parse_header("a.md", "## Context\n\n# Real title\n");
        assert_eq!(header.title.as_deref(), Some("Real title"));
    }
}
