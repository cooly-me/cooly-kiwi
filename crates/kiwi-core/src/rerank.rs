//! Reranking the top of a search with a cross-encoder plugin.
//!
//! The reranker reads the passages of the first `top_k` results, which are already
//! inside the caller's view, and only those whose permission class the policy lets the
//! plugin read. Those it may not read keep their positions; the others are reordered
//! among the positions they held. On any failure the fused order stands.

use std::sync::Arc;

use crate::engine::RankedAtom;
use crate::error::{KiwiError, Result};
use crate::plugin::Reranker;
use crate::text;

/// The tier a failed rerank is reported under in `degraded` warnings.
pub const RERANK_TIER: &str = "rerank";

/// The longest query text a reranker receives.
const QUERY_CHARS: usize = 1_000;

/// A reranker attached to an engine (or to a federation of engines).
#[derive(Clone)]
pub struct AttachedReranker {
    reranker: Arc<dyn Reranker>,
    top_k: usize,
    allowed: Vec<bool>,
}

impl std::fmt::Debug for AttachedReranker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachedReranker")
            .field("name", &self.reranker.name())
            .field("top_k", &self.top_k)
            .finish()
    }
}

impl AttachedReranker {
    /// Attaches `reranker` to the first `top_k` results; `allowed[class]` says whether
    /// it may read atoms of each permission class of the pack.
    pub fn new(reranker: Arc<dyn Reranker>, top_k: usize, allowed: Vec<bool>) -> Self {
        Self {
            reranker,
            top_k,
            allowed,
        }
    }

    /// The plugin.
    pub fn reranker(&self) -> &Arc<dyn Reranker> {
        &self.reranker
    }

    /// How many results from the top are reranked.
    pub fn top_k(&self) -> usize {
        self.top_k
    }

    /// Whether the plugin may read atoms of `class`.
    pub fn allows(&self, class: u32) -> bool {
        self.allowed.get(class as usize).copied().unwrap_or(false)
    }

    /// Reranks the head of `items` in place (see [`rerank`]).
    pub fn apply(
        &self,
        queries: &[String],
        items: &mut [RankedAtom],
        passage: impl FnMut(&RankedAtom) -> Result<Option<String>>,
    ) -> Result<bool> {
        rerank(
            self.reranker.as_ref(),
            &query_text(queries),
            items,
            self.top_k,
            passage,
            mark_reranked,
        )
    }
}

/// Marks an atom a reranker scored.
pub fn mark_reranked(item: &mut RankedAtom) {
    item.why.insert("rerank".to_owned());
}

/// The query a reranker scores against: the agent's queries joined, bounded.
pub fn query_text(queries: &[String]) -> String {
    let joined: Vec<&str> = queries
        .iter()
        .map(|q| q.trim())
        .filter(|q| !q.is_empty())
        .collect();
    text::truncate_chars(&joined.join("; "), QUERY_CHARS)
}

/// Reorders the first `top_k` of `items` by the reranker's scores. `passage` gives the
/// text of an item, or `None` when the plugin may not read it (the item then keeps its
/// position). Every item the reranker scored is passed to `mark`. Returns whether
/// anything was reranked; on an error the order is left as it was.
pub fn rerank<T: Clone>(
    reranker: &dyn Reranker,
    query: &str,
    items: &mut [T],
    top_k: usize,
    mut passage: impl FnMut(&T) -> Result<Option<String>>,
    mut mark: impl FnMut(&mut T),
) -> Result<bool> {
    let head = top_k.min(items.len());
    if head < 2 || query.is_empty() {
        return Ok(false);
    }
    let mut slots = Vec::new();
    let mut passages = Vec::new();
    for (slot, item) in items[..head].iter().enumerate() {
        if let Some(text) = passage(item)? {
            slots.push(slot);
            passages.push(text);
        }
    }
    if slots.len() < 2 {
        return Ok(false);
    }
    let scores = reranker.score(query, &passages)?;
    if scores.len() != passages.len() || scores.iter().any(|s| !s.is_finite()) {
        return Err(KiwiError::plugin(
            reranker.name(),
            format!("returned {} scores for {} passages", scores.len(), passages.len()),
        ));
    }
    // Stable: equal scores keep the fused order.
    let mut order: Vec<usize> = (0..slots.len()).collect();
    order.sort_by(|a, b| scores[*b].total_cmp(&scores[*a]).then(a.cmp(b)));
    let picked: Vec<T> = order.iter().map(|&i| items[slots[i]].clone()).collect();
    for (&slot, mut item) in slots.iter().zip(picked) {
        mark(&mut item);
        items[slot] = item;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::plugin::Capabilities;

    struct ByLength;

    impl Reranker for ByLength {
        fn name(&self) -> &str {
            "by-length"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
        fn score(&self, _query: &str, passages: &[String]) -> Result<Vec<f32>> {
            Ok(passages.iter().map(|p| p.len() as f32).collect())
        }
    }

    struct Failing;

    impl Reranker for Failing {
        fn name(&self) -> &str {
            "failing"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
        fn score(&self, _query: &str, _passages: &[String]) -> Result<Vec<f32>> {
            Err(KiwiError::plugin("failing", "timed out"))
        }
    }

    fn items(n: u32) -> Vec<RankedAtom> {
        (0..n)
            .map(|uid| RankedAtom {
                uid,
                score: 1.0 / f64::from(uid + 1),
                why: BTreeSet::new(),
                replaces: BTreeSet::new(),
            })
            .collect()
    }

    #[test]
    fn reorders_the_head_and_keeps_forbidden_slots() {
        let mut list = items(5);
        // Passage length grows with the uid; uid 1 may not be read.
        let changed = rerank(
            &ByLength,
            "q",
            &mut list,
            4,
            |item| Ok((item.uid != 1).then(|| "x".repeat(item.uid as usize + 1))),
            mark_reranked,
        )
        .unwrap();
        assert!(changed);
        let uids: Vec<u32> = list.iter().map(|i| i.uid).collect();
        assert_eq!(uids, vec![3, 1, 2, 0, 4]);
        assert!(list[0].why.contains("rerank"));
        assert!(!list[1].why.contains("rerank"));
        assert!(!list[4].why.contains("rerank"));
    }

    #[test]
    fn failure_keeps_the_fused_order() {
        let mut list = items(4);
        let before = list.clone();
        assert!(
            rerank(
                &Failing,
                "q",
                &mut list,
                4,
                |_| Ok(Some("p".into())),
                mark_reranked
            )
            .is_err()
        );
        assert_eq!(list, before);
    }

    #[test]
    fn query_text_joins_and_bounds() {
        assert_eq!(query_text(&["a b".into(), " ".into(), "c".into()]), "a b; c");
        assert!(query_text(&["w ".repeat(2_000)]).chars().count() <= QUERY_CHARS + 1);
    }
}
