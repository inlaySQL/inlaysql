//! Rank fusion.
//!
//! A vector index and a BM25 index produce scores on incomparable scales:
//! cosine similarity lives in `[-1, 1]`, BM25 is unbounded and depends on
//! corpus statistics. Normalising them against each other needs corpus-wide
//! calibration we do not have at query time.
//!
//! Reciprocal rank fusion sidesteps that by throwing the raw scores away and
//! combining *ranks* only:
//!
//! ```text
//! score(d) = Σ_lists 1 / (k + rank_list(d))
//! ```
//!
//! It needs no tuning, is robust to one retriever being badly calibrated, and
//! is what `fuse()` in the SQL dialect compiles to.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::cmp::Ordering;

use crate::traits::{RowId, Scored};

/// The `k` constant from the original RRF paper (Cormack et al., 2009).
///
/// It damps the influence of the very top ranks so a single retriever cannot
/// dominate the fused ordering on its own.
pub const DEFAULT_RRF_K: f32 = 60.0;

/// Fuse ranked lists into one ordering, best first.
///
/// Each input must already be sorted best-first. Documents missing from a list
/// simply contribute nothing for that list. Ties are broken by ascending row
/// id so the output is deterministic regardless of input ordering.
pub fn reciprocal_rank_fusion(lists: &[Vec<Scored>], k: f32) -> Vec<Scored> {
    let mut fused: BTreeMap<RowId, f32> = BTreeMap::new();
    for list in lists {
        for (rank, scored) in list.iter().enumerate() {
            *fused.entry(scored.id).or_insert(0.0) += 1.0 / (k + rank as f32 + 1.0);
        }
    }

    let mut out: Vec<Scored> = fused
        .into_iter()
        .map(|(id, score)| Scored::new(id, score))
        .collect();
    sort_by_score_desc(&mut out);
    out
}

/// Sort scored rows best-first, breaking ties by ascending row id.
///
/// Used everywhere a ranked list is produced so that every code path in the
/// engine agrees on the same total order.
pub fn sort_by_score_desc(scores: &mut [Scored]) {
    scores.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then(a.id.cmp(&b.id))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn a_row_ranked_well_by_both_retrievers_wins() {
        // Row 2 is second in both lists; rows 1 and 3 each top one list only.
        let vector = vec![Scored::new(1, 0.9), Scored::new(2, 0.8)];
        let text = vec![Scored::new(3, 12.0), Scored::new(2, 11.0)];
        let fused = reciprocal_rank_fusion(&[vector, text], DEFAULT_RRF_K);
        assert_eq!(fused[0].id, 2);
    }

    #[test]
    fn raw_score_scale_does_not_matter() {
        let small = vec![Scored::new(1, 0.01), Scored::new(2, 0.009)];
        let huge = vec![Scored::new(1, 9_000.0), Scored::new(2, 8_000.0)];
        let a = reciprocal_rank_fusion(&[small.clone(), huge.clone()], DEFAULT_RRF_K);
        let b = reciprocal_rank_fusion(&[huge, small], DEFAULT_RRF_K);
        assert_eq!(
            a.iter().map(|s| s.id).collect::<Vec<_>>(),
            b.iter().map(|s| s.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn ties_break_on_row_id() {
        let left = vec![Scored::new(7, 1.0)];
        let right = vec![Scored::new(3, 1.0)];
        let fused = reciprocal_rank_fusion(&[left, right], DEFAULT_RRF_K);
        assert_eq!(fused.iter().map(|s| s.id).collect::<Vec<_>>(), vec![3, 7]);
    }

    #[test]
    fn empty_input_yields_empty_output() {
        assert!(reciprocal_rank_fusion(&[], DEFAULT_RRF_K).is_empty());
    }
}
