//! Query result envelope: hits plus confidence metadata for agents.

use serde::{Serialize, Serializer};

/// How strongly the resolver trusts the returned hits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

/// Resolution tier summary for a query response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolutionTier {
    /// Single tier used for all accepted hits (1, 2, or 3).
    Single(u8),
    /// Mixed tiers across hits.
    Mixed,
}

impl ResolutionTier {
    /// Build from the set of tiers observed.
    /// Empty input → `Single(0)` (no resolution ran / not applicable).
    pub fn from_tiers(tiers: &[u8]) -> Self {
        let mut uniq: Vec<u8> = tiers.to_vec();
        uniq.sort_unstable();
        uniq.dedup();
        match uniq.as_slice() {
            [] => Self::Single(0),
            [only] => Self::Single(*only),
            _ => Self::Mixed,
        }
    }
}

impl Serialize for ResolutionTier {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::Single(n) => serializer.serialize_u8(*n),
            Self::Mixed => serializer.serialize_str("mixed"),
        }
    }
}

/// Default cap for unbounded hit lists (`references`, `callers`, `impact`).
///
/// Hot names (`clone`, `to_string`) can otherwise return tens of thousands
/// of hits and blow agent context windows. Small result sets are unaffected;
/// capped responses always carry an explanatory note, so completeness is
/// never silently lost.
pub const DEFAULT_RESULT_LIMIT: usize = 500;

/// Upper bound for explicit `--limit`/`limit` overrides (still bounded:
/// million-hit payloads help nobody; narrow with `module` instead).
pub const MAX_RESULT_LIMIT: usize = 100_000;

/// Resolve an optional caller limit to an effective cap.
pub fn resolve_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_RESULT_LIMIT).clamp(1, MAX_RESULT_LIMIT)
}

/// Structured query response with backward-compatible `results` plus metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QueryResult<T> {
    pub results: Vec<T>,
    pub confidence: Confidence,
    pub resolution_tier: ResolutionTier,
    pub notes: Vec<String>,
}

impl<T> QueryResult<T> {
    pub fn new(
        results: Vec<T>,
        confidence: Confidence,
        resolution_tier: ResolutionTier,
        notes: Vec<String>,
    ) -> Self {
        Self {
            results,
            confidence,
            resolution_tier,
            notes,
        }
    }

    pub fn map_results<U, F: FnMut(T) -> U>(self, f: F) -> QueryResult<U> {
        QueryResult {
            results: self.results.into_iter().map(f).collect(),
            confidence: self.confidence,
            resolution_tier: self.resolution_tier,
            notes: self.notes,
        }
    }

    /// Cap `results` at `limit` hits (head of the deterministic order),
    /// appending a truncation note when anything is dropped. No-op when the
    /// list already fits. Confidence/tier summaries are kept as computed
    /// over the full set (conservative: never over-claimed by truncation).
    pub fn truncated(mut self, limit: usize) -> Self {
        let total = self.results.len();
        if total > limit {
            self.results.truncate(limit);
            self.notes.push(format!(
                "Showing first {limit} of {total} matches; narrow with module or raise the limit."
            ));
        }
        self
    }

    pub fn from_tiers(
        results: Vec<T>,
        tiers: &[u8],
        multi_def: bool,
        mut notes: Vec<String>,
    ) -> Self {
        // Empty hit lists are confident misses (or caller-supplied empty notes),
        // not name-only fallback. Low confidence applies only when hits exist.
        if results.is_empty() {
            if notes.is_empty() {
                notes.push("No matching symbols found.".into());
            }
            return Self::new(
                results,
                Confidence::High,
                ResolutionTier::from_tiers(tiers),
                notes,
            );
        }

        let confidence = confidence_from_tiers(tiers, multi_def);
        if matches!(confidence, Confidence::Low) && notes.is_empty() {
            notes.push(
                "Resolution used name-only fallback; results may include unrelated same-named symbols."
                    .into(),
            );
        }
        // Do not inject impact-only wording here — callers that expand transitively
        // (impact_with_meta) attach their own multi-def notes.
        Self::new(
            results,
            confidence,
            ResolutionTier::from_tiers(tiers),
            notes,
        )
    }
}

/// Deterministic confidence from observed resolve tiers.
///
/// - **high** — all tiers ≤ 2 and not multi-def
/// - **medium** — mix of ≤2 and 3, or multi-def with some precise tiers
/// - **low** — empty tiers, or tier-3 dominated
pub fn confidence_from_tiers(tiers: &[u8], multi_def: bool) -> Confidence {
    if tiers.is_empty() {
        return Confidence::Low;
    }
    let all_precise = tiers.iter().all(|t| *t <= 2);
    let any_precise = tiers.iter().any(|t| *t <= 2);
    let all_fallback = tiers.iter().all(|t| *t >= 3);

    if all_precise && !multi_def {
        Confidence::High
    } else if all_fallback || !any_precise {
        Confidence::Low
    } else {
        Confidence::Medium
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn high_when_all_tier_one_or_two() {
        assert_eq!(confidence_from_tiers(&[1, 2], false), Confidence::High);
    }

    #[test]
    fn truncated_caps_with_note_and_leaves_small_lists_alone() {
        let full: QueryResult<u8> =
            QueryResult::from_tiers(vec![1, 2, 3], &[1, 1, 1], false, Vec::new());
        let capped = full.truncated(2);
        assert_eq!(capped.results, vec![1, 2]);
        assert_eq!(capped.notes, vec!["Showing first 2 of 3 matches; narrow with module or raise the limit."]);
        // Summaries stay as computed (conservative under truncation).
        assert_eq!(capped.confidence, Confidence::High);

        let small: QueryResult<u8> =
            QueryResult::from_tiers(vec![1], &[1], false, Vec::new());
        let kept = small.truncated(2);
        assert_eq!(kept.results, vec![1]);
        assert!(kept.notes.is_empty());
    }

    #[test]
    fn resolve_limit_defaults_and_clamps() {
        assert_eq!(resolve_limit(None), DEFAULT_RESULT_LIMIT);
        assert_eq!(resolve_limit(Some(0)), 1);
        assert_eq!(resolve_limit(Some(10)), 10);
        assert_eq!(resolve_limit(Some(usize::MAX)), MAX_RESULT_LIMIT);
    }

    #[test]
    fn low_when_tier_three_dominates() {
        assert_eq!(confidence_from_tiers(&[3, 3], false), Confidence::Low);
    }

    #[test]
    fn medium_when_mixed_or_multi_def() {
        assert_eq!(confidence_from_tiers(&[1, 3], false), Confidence::Medium);
        assert_eq!(confidence_from_tiers(&[1, 2], true), Confidence::Medium);
    }

    #[test]
    fn resolution_tiers_mixed() {
        assert_eq!(ResolutionTier::from_tiers(&[1, 2]), ResolutionTier::Mixed);
        assert_eq!(
            ResolutionTier::from_tiers(&[2, 2]),
            ResolutionTier::Single(2)
        );
    }

    #[test]
    fn resolution_tiers_serialize() {
        let single = serde_json::to_value(ResolutionTier::Single(2)).unwrap();
        assert_eq!(single, serde_json::json!(2));
        let mixed = serde_json::to_value(ResolutionTier::Mixed).unwrap();
        assert_eq!(mixed, serde_json::json!("mixed"));
    }

    #[test]
    fn empty_results_are_not_found_not_fallback_lie() {
        let qr = QueryResult::<()>::from_tiers(vec![], &[], false, vec![]);
        assert!(qr.results.is_empty());
        assert_eq!(qr.confidence, Confidence::High);
        assert_eq!(qr.resolution_tier, ResolutionTier::Single(0));
        assert_eq!(qr.notes, vec!["No matching symbols found.".to_string()]);
        assert!(!qr.notes.iter().any(|n| n.contains("name-only fallback")));
    }

    #[test]
    fn empty_preserves_caller_notes() {
        let qr = QueryResult::<()>::from_tiers(
            vec![],
            &[],
            false,
            vec!["No indexed files found for target `x`.".into()],
        );
        assert_eq!(qr.confidence, Confidence::High);
        assert_eq!(qr.notes.len(), 1);
        assert!(qr.notes[0].contains("No indexed files"));
    }

    #[test]
    fn low_confidence_with_hits_keeps_fallback_note() {
        let qr = QueryResult::from_tiers(vec!["hit"], &[3], false, vec![]);
        assert_eq!(qr.confidence, Confidence::Low);
        assert!(qr.notes.iter().any(|n| n.contains("name-only fallback")));
    }

    #[test]
    fn multi_def_does_not_inject_impact_note() {
        let qr = QueryResult::from_tiers(
            vec!["a", "b"],
            &[3, 3],
            true,
            vec!["Found 2 definitions for `serve`; disambiguate by module if needed.".into()],
        );
        assert!(!qr.notes.iter().any(|n| n.contains("over-approximate impact")));
    }
}
