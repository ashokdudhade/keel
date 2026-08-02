//! Query result envelope: hits plus confidence metadata for agents.

use serde::Serialize;

/// How strongly the resolver trusts the returned hits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

/// Resolution tier summary for a query response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ResolutionTier {
    /// Single tier used for all accepted hits (1, 2, or 3).
    Single(u8),
    /// Mixed tiers across hits.
    Mixed,
}

impl ResolutionTier {
    /// Build from the set of tiers observed (ignores empty → treated as low/unknown).
    pub fn from_tiers(tiers: &[u8]) -> Self {
        let mut uniq: Vec<u8> = tiers.to_vec();
        uniq.sort_unstable();
        uniq.dedup();
        match uniq.as_slice() {
            [] => Self::Single(3),
            [only] => Self::Single(*only),
            _ => Self::Mixed,
        }
    }
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

    pub fn from_tiers(results: Vec<T>, tiers: &[u8], multi_def: bool, mut notes: Vec<String>) -> Self {
        let confidence = confidence_from_tiers(tiers, multi_def);
        if matches!(confidence, Confidence::Low) && notes.is_empty() {
            notes.push(
                "Resolution used name-only fallback; results may include unrelated same-named symbols."
                    .into(),
            );
        } else if multi_def && !notes.iter().any(|n| n.contains("multiple definitions")) {
            notes.push(
                "Target has multiple definitions; expansion may over-approximate impact.".into(),
            );
        }
        Self::new(results, confidence, ResolutionTier::from_tiers(tiers), notes)
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
        assert_eq!(ResolutionTier::from_tiers(&[2, 2]), ResolutionTier::Single(2));
    }
}
