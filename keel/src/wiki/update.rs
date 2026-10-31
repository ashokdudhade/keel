//! Wiki updates and status: stale-only rewrites and freshness reporting.

use crate::error::Result;
use crate::wiki::generate::{collect_pages, load_manifest, write_pages};
use crate::wiki::{PageState, PageStatus, WikiReport, WikiStatusReport};
use rusqlite::Connection;
use std::collections::BTreeSet;
use std::path::Path;

/// Rewrite only stale or missing pages (pins still matching stay untouched),
/// prune orphaned generated-only pages, and refresh the manifest.
pub fn update_stale(conn: &Connection, out_dir: &Path) -> Result<WikiReport> {
    let pages = collect_pages(conn)?;
    let mut report = write_pages(conn, out_dir, &pages, true)?;
    if report.written.is_empty() && report.deleted.is_empty() && report.kept.is_empty() {
        report.notes.push("Wiki is current: no pages needed updates.".to_string());
    }
    Ok(report)
}

/// Report per-page freshness without writing anything.
pub fn status_report(conn: &Connection, out_dir: &Path) -> Result<WikiStatusReport> {
    let pages = collect_pages(conn)?;
    let manifest_path = out_dir.join(crate::wiki::MANIFEST_FILE);
    let manifest = load_manifest(out_dir);
    let live: BTreeSet<&str> = pages.iter().map(|p| p.rel_path.as_str()).collect();
    let mut states = Vec::new();

    for page in &pages {
        let target = out_dir.join(&page.rel_path);
        let state = if !target.exists() {
            PageStatus {
                path: page.rel_path.clone(),
                state: PageState::Missing,
                detail: "Page file is missing.".to_string(),
            }
        } else {
            let text = std::fs::read_to_string(&target).unwrap_or_default();
            if !crate::wiki::render::has_region(&text, &page.id) {
                PageStatus {
                    path: page.rel_path.clone(),
                    state: PageState::Manual,
                    detail: "No intact managed region; updates never touch this file.".to_string(),
                }
            } else {
                match manifest.pages.get(&page.rel_path) {
                    Some(pinned) if pinned.inputs == page.inputs_hash() => PageStatus {
                        path: page.rel_path.clone(),
                        state: PageState::Current,
                        detail: "Pinned inputs match.".to_string(),
                    },
                    Some(pinned) => PageStatus {
                        path: page.rel_path.clone(),
                        state: PageState::Stale,
                        detail: format!(
                            "Inputs changed (pin `{}`, live `{}`).",
                            short(&pinned.inputs),
                            short(&page.inputs_hash())
                        ),
                    },
                    None => PageStatus {
                        path: page.rel_path.clone(),
                        state: PageState::Stale,
                        detail: "Page is not pinned (never generated).".to_string(),
                    },
                }
            }
        };
        states.push(state);
    }

    // Manifest entries with no live page: inputs gone.
    let mut orphans: Vec<&String> = manifest
        .pages
        .keys()
        .filter(|p| !live.contains(p.as_str()))
        .collect();
    orphans.sort();
    for orphan in orphans {
        let target = out_dir.join(orphan);
        let detail = if !target.exists() {
            "Inputs gone; file already removed.".to_string()
        } else {
            let text = std::fs::read_to_string(&target).unwrap_or_default();
            let human = crate::wiki::render::human_content(&text, orphan)
                .unwrap_or_default();
            if human.is_empty() {
                "Inputs gone; `wiki update` will delete this generated-only page.".to_string()
            } else {
                "Inputs gone; page holds human content and will be kept.".to_string()
            }
        };
        states.push(PageStatus {
            path: orphan.clone(),
            state: PageState::Orphaned,
            detail,
        });
    }

    states.sort_by(|a, b| a.path.cmp(&b.path));
    let mut notes = Vec::new();
    if !manifest_path.exists() {
        notes.push("No manifest found: run `keel wiki generate` first.".to_string());
    } else if states.iter().any(|s| {
        matches!(s.state, PageState::Stale | PageState::Missing | PageState::Orphaned)
    }) {
        notes.push("Stale pages found: run `keel wiki update`.".to_string());
    } else {
        notes.push("Wiki is current.".to_string());
    }
    Ok(WikiStatusReport { pages: states, notes })
}

fn short(hash: &str) -> String {
    hash.chars().take(12).collect()
}
