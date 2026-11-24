//! The query planner: the policy-filtered view, the retrieval cascade, rank fusion,
//! graph priors and the token-budget packing.
//!
//! Every tier runs on the atoms of a [`View`], which the policy gate computes before
//! anything is ranked. Nothing outside the view is scored, counted for statistics or
//! returned.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::Arc;

use roaring::RoaringBitmap;

use crate::api::*;
use crate::error::{KiwiError, Result};
use crate::model::{ArchivedAtomRecord, ArchivedDocRecord, EdgeKind, NO_UID, key_kind};
use crate::pack::Pack;
use crate::plugin::{Retriever, Scope};
use crate::rerank::{AttachedReranker, RERANK_TIER};
use crate::text;

/// How atoms the caller may not see are shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HiddenMode {
    /// As if they did not exist (the default).
    #[default]
    Invisible,
    /// As stubs that name the owner, and as `redacted` counts.
    Stub,
}

/// Planner settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EngineConfig {
    /// Budget when a request names none.
    pub default_budget: u32,
    /// Results considered before the budget cut when a request names no limit.
    pub default_limit: u32,
    /// Upper bound for `limit`.
    pub max_limit: u32,
    /// The `k` of reciprocal rank fusion.
    pub rrf_k: f64,
    /// Score multiplier for atoms of approved documents.
    pub approved_boost: f64,
    /// Hidden-atom display.
    pub hidden: HiddenMode,
}

impl EngineConfig {
    /// The token budget of a request under these settings.
    pub fn budget(&self, requested: Option<u32>) -> u32 {
        requested.unwrap_or(self.default_budget).clamp(50, 32_000)
    }

    /// The results considered before the budget cut under these settings.
    pub fn limit(&self, req: &SearchRequest) -> usize {
        req.limit.unwrap_or(self.default_limit).clamp(1, self.max_limit) as usize
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            default_budget: 800,
            default_limit: 10,
            max_limit: 50,
            rrf_k: 60.0,
            approved_boost: 1.1,
            hidden: HiddenMode::Invisible,
        }
    }
}

/// The atoms one caller may see: the OR of the bitmaps of its permitted classes.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    visible: RoaringBitmap,
}

impl View {
    /// The view of a set of permitted classes.
    pub fn from_classes(pack: &Pack, classes: impl IntoIterator<Item = u32>) -> Self {
        let mut visible = RoaringBitmap::new();
        for class in classes {
            if let Some(bitmap) = pack.class_bitmap(class) {
                visible |= bitmap;
            }
        }
        Self { visible }
    }

    /// A view of every atom (navigation without governance, tests and tooling).
    pub fn everything(pack: &Pack) -> Self {
        Self {
            visible: pack.all_atoms().clone(),
        }
    }

    /// The visible uids.
    pub fn visible(&self) -> &RoaringBitmap {
        &self.visible
    }

    /// Whether `uid` is visible.
    pub fn contains(&self, uid: u32) -> bool {
        self.visible.contains(uid)
    }
}

const MAX_QUERIES: usize = 8;
const MAX_QUERY_CHARS: usize = 512;
const MAX_READ_IDS: usize = 20;
const MAX_TRACE_NODES: usize = 200;
const EXACT_WEIGHT: f64 = 2.0;
const ID_WEIGHT: f64 = 1.5;
const TERM_WEIGHT: f64 = 0.75;
const GRAPH_WEIGHT: f64 = 0.5;
const RETRIEVER_WEIGHT: f64 = 1.0;
const ITEM_OVERHEAD: u32 = 12;
const SUMMARY_OVERHEAD: u32 = 4;
const NODE_OVERHEAD: u32 = 6;
/// Characters of an atom a reranker reads.
const PASSAGE_CHARS: usize = 1_500;

/// The query engine over one pack.
pub struct Engine {
    pack: Arc<Pack>,
    retrievers: Vec<Box<dyn Retriever>>,
    reranker: Option<AttachedReranker>,
    config: EngineConfig,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("pack", &self.pack)
            .field("retrievers", &self.retriever_names())
            .finish()
    }
}

struct Fusion {
    k: f64,
    scores: HashMap<u32, (f64, BTreeSet<String>)>,
}

impl Fusion {
    fn add(&mut self, label: &str, weight: f64, ranked: &[u32]) {
        let mut seen = BTreeSet::new();
        let mut rank = 0usize;
        for &uid in ranked {
            if !seen.insert(uid) {
                continue;
            }
            rank += 1;
            let entry = self.scores.entry(uid).or_insert((0.0, BTreeSet::new()));
            entry.0 += weight / (self.k + rank as f64);
            entry.1.insert(label.to_owned());
        }
    }
}

/// One ranked atom of a search.
#[derive(Debug, Clone, PartialEq)]
pub struct RankedAtom {
    /// Atom uid.
    pub uid: u32,
    /// Fused score.
    pub score: f64,
    /// Why it matched: tiers and retrievers (`rerank` when a reranker scored it).
    pub why: BTreeSet<String>,
    /// Deprecated atoms this one stands in for.
    pub replaces: BTreeSet<u32>,
}

/// Every atom a search ranked, best first, and the matches the view withheld.
#[derive(Debug, Clone, PartialEq)]
pub struct Ranking {
    /// Ranked atoms.
    pub items: Vec<RankedAtom>,
    /// Exact matches outside the caller's view.
    pub blocked: u32,
    /// Retrievers that failed, so their tier is missing from the ranking.
    pub degraded: Vec<String>,
}

/// What fits in a token budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetFit {
    /// How many items, from the top, are listed.
    pub included: usize,
    /// For each listed item, whether its L1 fits too.
    pub summaries: Vec<bool>,
    /// Tokens used.
    pub used: u32,
}

/// Fits ranked items into a budget, breadth first: every item that fits gets its L0 in
/// rank order (stopping at the first that does not), then the best get their L1 while
/// the budget lasts. `costs` holds each item's L0 cost and L1 extra cost.
pub fn fit_budget(costs: &[(u32, Option<u32>)], budget: u32) -> BudgetFit {
    let mut used = 0u32;
    let mut included = 0usize;
    for (base, _) in costs {
        if used + base > budget {
            break;
        }
        used += base;
        included += 1;
    }
    let mut summaries = Vec::with_capacity(included);
    for (_, extra) in costs.iter().take(included) {
        let fits = extra.is_some_and(|extra| used + extra <= budget);
        if let (true, Some(extra)) = (fits, extra) {
            used += extra;
        }
        summaries.push(fits);
    }
    BudgetFit {
        included,
        summaries,
        used,
    }
}

impl Engine {
    /// An engine over `pack` with the given retriever plugins.
    pub fn new(pack: Arc<Pack>, retrievers: Vec<Box<dyn Retriever>>, config: EngineConfig) -> Self {
        Self {
            pack,
            retrievers,
            reranker: None,
            config,
        }
    }

    /// Attaches a reranker for the top of every search.
    pub fn with_reranker(mut self, reranker: AttachedReranker) -> Self {
        self.reranker = Some(reranker);
        self
    }

    /// The attached reranker.
    pub fn reranker(&self) -> Option<&AttachedReranker> {
        self.reranker.as_ref()
    }

    /// The pack.
    pub fn pack(&self) -> &Arc<Pack> {
        &self.pack
    }

    /// The settings.
    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// Names of the attached retrievers.
    pub fn retriever_names(&self) -> Vec<&str> {
        self.retrievers.iter().map(|r| r.name()).collect()
    }

    fn budget(&self, requested: Option<u32>) -> u32 {
        self.config.budget(requested)
    }

    fn atom(&self, uid: u32) -> Result<&ArchivedAtomRecord> {
        self.pack
            .atom(uid)
            .ok_or_else(|| KiwiError::Corrupt(format!("atom {uid} out of range")))
    }

    fn doc_of(&self, uid: u32) -> Result<&ArchivedDocRecord> {
        self.pack
            .doc_of(uid)
            .ok_or_else(|| KiwiError::Corrupt(format!("atom {uid} has no document")))
    }

    fn stub(&self) -> bool {
        self.config.hidden == HiddenMode::Stub
    }

    /// The candidates after structural filters, always inside the view.
    pub fn candidates(&self, view: &View, filters: &Filters) -> Result<RoaringBitmap> {
        if !filters.namespace.is_empty()
            && !filters
                .namespace
                .iter()
                .any(|ns| ns.as_str() == self.pack.data().namespace.as_str())
        {
            return Ok(RoaringBitmap::new());
        }
        let mut set = view.visible().clone();
        let groups: [(&str, &Vec<String>); 4] = [
            ("type", &filters.doc_type),
            ("status", &filters.status),
            ("owner", &filters.owner),
            ("tag", &filters.tags),
        ];
        for (field, values) in groups {
            if values.is_empty() {
                continue;
            }
            let mut any = RoaringBitmap::new();
            for value in values {
                if let Some(bitmap) = self.pack.facet(field, &text::key(value))? {
                    any |= bitmap;
                }
            }
            set &= any;
        }
        if let Some(prefix) = &filters.path {
            let prefix = prefix.trim_start_matches("./").trim_start_matches('/');
            let mut any = RoaringBitmap::new();
            for doc in self.pack.data().docs.iter() {
                if doc.path.as_str().starts_with(prefix) {
                    let first = doc.first.to_native();
                    any.insert_range(first..first + doc.count.to_native());
                }
            }
            set &= any;
        }
        Ok(set)
    }

    /// The number of results a search considers before the budget cut.
    pub fn limit(&self, req: &SearchRequest) -> usize {
        self.config.limit(req)
    }

    /// The ranking behind `kiwi_search`: the cascade exact → structural → plugins, fused
    /// with RRF, with status priors and deprecation redirects applied. Every ranked atom,
    /// best first; nothing is cut yet.
    pub fn rank(&self, view: &View, req: &SearchRequest) -> Result<Ranking> {
        if req.queries.len() > MAX_QUERIES {
            return Err(KiwiError::InvalidInput(format!(
                "at most {MAX_QUERIES} queries per search"
            )));
        }
        if req.queries.iter().any(|q| q.chars().count() > MAX_QUERY_CHARS) {
            return Err(KiwiError::InvalidInput(format!(
                "a query is limited to {MAX_QUERY_CHARS} characters"
            )));
        }
        let limit = self.limit(req);
        let candidates = self.candidates(view, &req.filters)?;
        let mut fusion = Fusion {
            k: self.config.rrf_k,
            scores: HashMap::new(),
        };
        let mut blocked = 0u32;
        let mut lexical: Vec<String> = Vec::new();
        let mut originals: Vec<String> = Vec::new();
        let mut seen_lexical: BTreeSet<String> = BTreeSet::new();
        let mut exact_all: Vec<u32> = Vec::new();
        let mut term_uids: BTreeSet<u32> = BTreeSet::new();
        let mut all_resolved = true;
        let mut any_query = false;
        let mut degraded: BTreeSet<String> = BTreeSet::new();

        // Per query: atoms of documents named inside it (`steps of RFC-010`), and the
        // query text without those ids, for a retrieval scoped to those documents.
        let mut scoped: Vec<(RoaringBitmap, String)> = Vec::new();
        for query in &req.queries {
            let folded = text::key(query);
            if folded.is_empty() {
                continue;
            }
            any_query = true;
            if seen_lexical.insert(folded.clone()) {
                lexical.push(query.trim().to_owned());
                originals.push(query.trim().to_owned());
            }
            let mut whole: Vec<u32> = Vec::new();
            if let Some(targets) = self.pack.lookup(&folded)? {
                // Why a key matched is decided by the atoms in the view only: a hidden
                // atom sharing the key must not change how visible ones rank.
                let kinds = visible_kinds(&targets, view);
                for (uid, _) in targets {
                    if !view.contains(uid) {
                        blocked += 1;
                    } else if candidates.contains(uid) {
                        whole.push(uid);
                        if kinds & key_kind::TERM != 0 {
                            term_uids.insert(uid);
                        }
                    }
                }
            }
            if whole.is_empty() {
                all_resolved = false;
            }
            let words = text::words(&folded);
            let mut ids: Vec<u32> = Vec::new();
            let mut terms: Vec<u32> = Vec::new();
            let mut scope_docs = RoaringBitmap::new();
            let mut id_words: BTreeSet<&str> = BTreeSet::new();
            for n in (1..=words.len().min(4)).rev() {
                if n == words.len() && words.join(" ") == folded {
                    continue;
                }
                for window in words.windows(n) {
                    let key = window.join(" ");
                    let Some(targets) = self.pack.lookup(&key)? else {
                        continue;
                    };
                    let kinds = visible_kinds(&targets, view);
                    let is_id = kinds & (key_kind::ATOM_ID | key_kind::DOC_ID) != 0;
                    let is_term = kinds & key_kind::TERM != 0 || (n >= 2 && kinds & key_kind::TITLE != 0);
                    if !(is_id || is_term) {
                        continue;
                    }
                    for (uid, _) in targets {
                        if !view.contains(uid) {
                            blocked += 1;
                            continue;
                        }
                        if !candidates.contains(uid) {
                            continue;
                        }
                        if is_id {
                            ids.push(uid);
                            id_words.extend(window.iter().copied());
                            let doc = self.doc_of(uid)?;
                            let first = doc.first.to_native();
                            scope_docs.insert_range(first..first + doc.count.to_native());
                        } else {
                            terms.push(uid);
                            if kinds & key_kind::TERM != 0 {
                                term_uids.insert(uid);
                            }
                        }
                    }
                }
            }
            fusion.add("exact", EXACT_WEIGHT, &whole);
            fusion.add("exact", ID_WEIGHT, &ids);
            fusion.add("term", TERM_WEIGHT, &terms);
            exact_all.extend(whole.iter().chain(&ids).copied());
            if !scope_docs.is_empty() {
                let rest: Vec<&str> = words.iter().copied().filter(|w| !id_words.contains(w)).collect();
                if !rest.is_empty() {
                    scoped.push((&scope_docs & &candidates, rest.join(" ")));
                }
            }
        }

        // Glossary expansion: a matched term or alias adds its synonyms as queries.
        for entry in self.pack.data().glossary.iter() {
            if !term_uids.contains(&entry.uid.to_native()) {
                continue;
            }
            let names =
                std::iter::once(entry.term.as_str()).chain(entry.aliases.iter().map(|alias| alias.as_str()));
            for name in names {
                if seen_lexical.insert(text::key(name)) && lexical.len() < MAX_QUERIES * 2 {
                    lexical.push(name.to_owned());
                }
            }
        }

        // Structural graph prior: documents one typed hop from an exact match.
        let graph = self.graph_neighbours(view, &candidates, &exact_all)?;
        fusion.add("graph", GRAPH_WEIGHT, &graph);

        if !any_query {
            let listing: Vec<u32> = candidates.iter().take(limit * 4).collect();
            fusion.add("filter", RETRIEVER_WEIGHT, &listing);
        } else if !candidates.is_empty() && (!all_resolved || exact_all.is_empty()) {
            // Retrievers run only when some query is not answered exactly and there is
            // something in scope to rank: an empty view or filters that match nothing
            // call no plugin (and no embeddings API).
            let scope = Scope {
                visible: view.visible(),
                candidates: &candidates,
            };
            for retriever in &self.retrievers {
                let expands = retriever.wants_expansions();
                let queries: &[String] = if expands { &lexical } else { &originals };
                // A retriever that fails (an embedding service that is down) leaves the
                // other tiers to answer; the response says which tier is missing.
                let Ok(batches) = retriever.search_batch(queries, scope, limit * 3) else {
                    degraded.insert(retriever.name().to_owned());
                    continue;
                };
                for hits in batches {
                    let ranked: Vec<u32> = hits
                        .iter()
                        .map(|hit| hit.uid)
                        .filter(|uid| candidates.contains(*uid))
                        .collect();
                    fusion.add(retriever.name(), RETRIEVER_WEIGHT, &ranked);
                }
                if !expands {
                    continue;
                }
                // Structural scope: the rest of a query, within the documents it names.
                for (docs, rest) in &scoped {
                    let scope = Scope {
                        visible: view.visible(),
                        candidates: docs,
                    };
                    let Ok(hits) = retriever.search(rest, scope, limit) else {
                        degraded.insert(retriever.name().to_owned());
                        continue;
                    };
                    let ranked: Vec<u32> = hits
                        .iter()
                        .map(|hit| hit.uid)
                        .filter(|uid| docs.contains(*uid))
                        .collect();
                    fusion.add("scoped", RETRIEVER_WEIGHT, &ranked);
                }
            }
        }

        let items = self.apply_priors(view, fusion)?;
        Ok(Ranking {
            items,
            blocked,
            degraded: degraded.into_iter().collect(),
        })
    }

    /// `kiwi_search`: the ranking, reranked at the top when a reranker is attached, cut
    /// to the token budget.
    pub fn search(&self, view: &View, req: &SearchRequest) -> Result<(SearchResponse, AuditFacts)> {
        let ranking = self.rank(view, req)?;
        let budget = self.budget(req.budget);
        let total = ranking.items.len();
        let limit = self.limit(req);
        let mut degraded = ranking.degraded;
        // The reranker reorders the first `top_k` fused results; the first `limit` of
        // the new order are kept.
        let head = self.reranker.as_ref().map_or(limit, |r| limit.max(r.top_k()));
        let mut top: Vec<RankedAtom> = ranking.items.into_iter().take(head).collect();
        if let Some(attached) = &self.reranker {
            let outcome = attached.apply(&req.queries, &mut top, |atom| {
                attached
                    .allows(self.atom(atom.uid)?.class.to_native())
                    .then(|| self.passage(atom.uid))
                    .transpose()
            });
            if let Err(error) = outcome {
                // The fused order stands; the answer says so and is not cached.
                tracing::warn!(%error, "rerank failed; the fused order stands");
                degraded.push(RERANK_TIER.to_owned());
            }
        }
        top.truncate(limit);
        let mut costs = Vec::with_capacity(top.len());
        for item in &top {
            costs.push(self.costs(item.uid, 0)?);
        }
        let fit = fit_budget(&costs, budget);
        let mut warnings: BTreeSet<Warning> = degraded
            .iter()
            .map(|tier| Warning::Degraded { tier: tier.clone() })
            .collect();
        let mut results = Vec::with_capacity(fit.included);
        let mut facts = AuditFacts {
            atoms: Vec::with_capacity(fit.included),
            blocked: ranking.blocked,
            degraded: !degraded.is_empty(),
        };
        for (item, &summary) in top.iter().zip(&fit.summaries) {
            results.push(self.item(item, summary, &mut warnings)?);
            facts.atoms.push(item.uid);
        }
        let response = SearchResponse {
            snapshot: self.pack.snapshot().to_owned(),
            snapshots: BTreeMap::new(),
            results,
            omitted: (total - fit.included) as u32,
            budget: BudgetUse {
                limit: budget,
                used: fit.used,
            },
            warnings: warnings.into_iter().collect(),
            redacted: self.stub().then_some(ranking.blocked),
        };
        Ok((response, facts))
    }

    /// Token cost of listing an atom at L0 (`extra_id` tokens added to its id, such as a
    /// namespace prefix) and the extra cost of its L1, when it has one.
    pub fn costs(&self, uid: u32, extra_id: u32) -> Result<(u32, Option<u32>)> {
        let atom = self.atom(uid)?;
        let tokens = &atom.tokens;
        let base = tokens.id.to_native() + extra_id + tokens.l0.to_native() + ITEM_OVERHEAD;
        let summary = (!atom.l1.is_empty()).then(|| tokens.l1.to_native() + SUMMARY_OVERHEAD);
        Ok((base, summary))
    }

    /// The search result for a ranked atom (at L1 when `summary`), adding its warnings.
    pub fn item(
        &self,
        ranked: &RankedAtom,
        summary: bool,
        warnings: &mut BTreeSet<Warning>,
    ) -> Result<SearchItem> {
        let atom = self.atom(ranked.uid)?;
        let doc = self.doc_of(ranked.uid)?;
        self.note_warnings(ranked.uid, warnings)?;
        for &old in &ranked.replaces {
            warnings.insert(Warning::Redirected {
                id: self.atom(old)?.id.as_str().to_owned(),
                to: atom.id.as_str().to_owned(),
            });
        }
        Ok(SearchItem {
            id: atom.id.as_str().to_owned(),
            level: if summary { Level::L1 } else { Level::L0 },
            title: atom.l0.as_str().to_owned(),
            summary: summary.then(|| atom.l1.as_str().to_owned()),
            status: doc.meta.status.as_str().to_owned(),
            score: (ranked.score * 10_000.0).round() / 10_000.0,
            why: ranked.why.iter().cloned().collect(),
        })
    }

    /// The text a reranker reads for an atom: its L0, its L1 and the start of its L2.
    pub fn passage(&self, uid: u32) -> Result<String> {
        let atom = self.atom(uid)?;
        let mut text = format!("{}\n{}\n", atom.l0, atom.l1);
        text.push_str(&self.pack.body(uid)?);
        Ok(text::truncate_chars(&text, PASSAGE_CHARS))
    }

    /// The permission class of an atom.
    pub fn class_of(&self, uid: u32) -> Result<u32> {
        Ok(self.atom(uid)?.class.to_native())
    }

    fn graph_neighbours(&self, view: &View, candidates: &RoaringBitmap, seeds: &[u32]) -> Result<Vec<u32>> {
        let mut out = Vec::new();
        let mut seen: BTreeSet<u32> = seeds.iter().copied().collect();
        for &seed in seeds {
            let root = self.doc_of(seed)?.root.to_native();
            let mut neighbours: Vec<u32> = Vec::new();
            for edge in self.pack.edges_from(root) {
                let kind = EdgeKind::from_u8(edge.kind);
                if matches!(kind, Some(EdgeKind::Mentions | EdgeKind::Code) | None) {
                    continue;
                }
                neighbours.push(edge.to.to_native());
            }
            for edge in self.pack.edges_to(root) {
                let kind = EdgeKind::from_u8(edge.kind);
                if matches!(kind, Some(EdgeKind::Mentions | EdgeKind::Code) | None) {
                    continue;
                }
                let from = edge.from.to_native();
                neighbours.push(self.doc_of(from)?.root.to_native());
            }
            for uid in neighbours {
                if uid != NO_UID && view.contains(uid) && candidates.contains(uid) && seen.insert(uid) {
                    out.push(uid);
                }
            }
        }
        Ok(out)
    }

    fn apply_priors(&self, view: &View, fusion: Fusion) -> Result<Vec<RankedAtom>> {
        let mut merged: HashMap<u32, RankedAtom> = HashMap::new();
        for (uid, (mut score, mut why)) in fusion.scores {
            let doc = self.doc_of(uid)?;
            let mut target = uid;
            let mut replaced = None;
            match doc.meta.status.as_str() {
                "approved" => score *= self.config.approved_boost,
                "deprecated" => {
                    if let Some(successor) = self.successor(view, uid)? {
                        if why.contains("exact") {
                            // Asked for by id: keep it, and rank its replacement just
                            // below it.
                            let next = merged.entry(successor).or_insert_with(|| RankedAtom {
                                uid: successor,
                                score: 0.0,
                                why: BTreeSet::new(),
                                replaces: BTreeSet::new(),
                            });
                            next.score = next.score.max(score * 0.999);
                            next.why.insert("successor".to_owned());
                            next.replaces.insert(uid);
                        } else {
                            why.insert("redirect".to_owned());
                            target = successor;
                            replaced = Some(uid);
                        }
                    }
                }
                _ => {}
            }
            let ranked = merged.entry(target).or_insert_with(|| RankedAtom {
                uid: target,
                score: 0.0,
                why: BTreeSet::new(),
                replaces: BTreeSet::new(),
            });
            ranked.score = ranked.score.max(score);
            ranked.why.extend(why);
            ranked.replaces.extend(replaced);
        }
        let mut ranked: Vec<RankedAtom> = merged.into_values().collect();
        ranked.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.uid.cmp(&b.uid)));
        Ok(ranked)
    }

    /// The visible atom that replaces a deprecated one: the same section of the newest
    /// visible document that supersedes it, else that document's root.
    fn successor(&self, view: &View, uid: u32) -> Result<Option<u32>> {
        let atom = self.atom(uid)?;
        let root = self.doc_of(uid)?.root.to_native();
        let mut best: Option<u32> = None;
        for edge in self.pack.edges_to(root) {
            if EdgeKind::from_u8(edge.kind) != Some(EdgeKind::Supersedes) {
                continue;
            }
            let from_root = self.doc_of(edge.from.to_native())?.root.to_native();
            if view.contains(from_root) && best.is_none_or(|b| from_root < b) {
                best = Some(from_root);
            }
        }
        let Some(successor_root) = best else {
            return Ok(None);
        };
        if atom.anchor.is_empty() {
            return Ok(Some(successor_root));
        }
        let doc = self.doc_of(successor_root)?;
        let first = doc.first.to_native();
        for candidate in first..first + doc.count.to_native() {
            if self.atom(candidate)?.anchor == atom.anchor && view.contains(candidate) {
                return Ok(Some(candidate));
            }
        }
        Ok(Some(successor_root))
    }

    fn note_warnings(&self, uid: u32, warnings: &mut BTreeSet<Warning>) -> Result<()> {
        let atom = self.atom(uid)?;
        let doc = self.doc_of(uid)?;
        let id = atom.id.as_str().to_owned();
        if let Some(review_by) = doc.meta.review_by.as_ref()
            && review_by.as_str() < self.pack.as_of()
        {
            warnings.insert(Warning::ReviewOverdue {
                id: id.clone(),
                review_by: review_by.as_str().to_owned(),
            });
        }
        match doc.meta.status.as_str() {
            "deprecated" => {
                warnings.insert(Warning::Deprecated { id });
            }
            status @ ("draft" | "review") => {
                warnings.insert(Warning::Unapproved {
                    id,
                    status: status.to_owned(),
                });
            }
            _ => {}
        }
        Ok(())
    }

    fn children(&self, uid: u32) -> Result<Vec<u32>> {
        let doc = self.doc_of(uid)?;
        let first = doc.first.to_native();
        let mut out = Vec::new();
        for candidate in first..first + doc.count.to_native() {
            if self.atom(candidate)?.parent.to_native() == uid {
                out.push(candidate);
            }
        }
        Ok(out)
    }

    /// `kiwi_map`: the L0/L1 tree of a scope, breadth first within the budget.
    pub fn map(&self, view: &View, req: &MapRequest) -> Result<(MapResponse, AuditFacts)> {
        self.map_within(view, req, self.budget(req.budget))
    }

    /// `kiwi_map` within exactly `budget` tokens (a federation's share of one budget,
    /// which may be below the smallest budget a caller can ask for).
    pub fn map_within(
        &self,
        view: &View,
        req: &MapRequest,
        budget: u32,
    ) -> Result<(MapResponse, AuditFacts)> {
        let depth = req.depth.unwrap_or(2).clamp(1, 8);
        let scope = req.scope.as_deref().map(str::trim).unwrap_or("");
        let (roots, label) = self.map_roots(view, scope)?;

        // Breadth-first walk over visible nodes, with their depth relative to the scope.
        let mut order: Vec<(u32, u32, u8)> = Vec::new(); // (uid, parent index or MAX, rel depth)
        let mut queue: VecDeque<(u32, u32, u8)> = roots.iter().map(|&uid| (uid, u32::MAX, 0u8)).collect();
        let mut omitted = 0u32;
        let mut hidden = 0u32;
        while let Some((uid, parent, rel)) = queue.pop_front() {
            if rel >= depth {
                omitted += self.count_visible_subtree(view, uid)?;
                continue;
            }
            let index = order.len() as u32;
            order.push((uid, parent, rel));
            for child in self.children(uid)? {
                if view.contains(child) {
                    queue.push_back((child, index, rel + 1));
                } else {
                    hidden += 1;
                }
            }
        }

        let mut included = vec![false; order.len()];
        let mut used = 0u32;
        for (i, &(uid, parent, _)) in order.iter().enumerate() {
            if parent != u32::MAX && !included[parent as usize] {
                omitted += 1;
                continue;
            }
            let tokens = &self.atom(uid)?.tokens;
            let cost = tokens.id.to_native() + tokens.l0.to_native() + NODE_OVERHEAD;
            if used + cost <= budget {
                used += cost;
                included[i] = true;
            } else {
                omitted += 1;
            }
        }
        let mut with_summary = vec![false; order.len()];
        for (i, &(uid, _, _)) in order.iter().enumerate() {
            if !included[i] {
                continue;
            }
            let atom = self.atom(uid)?;
            let cost = atom.tokens.l1.to_native() + SUMMARY_OVERHEAD;
            if !atom.l1.is_empty() && used + cost <= budget {
                used += cost;
                with_summary[i] = true;
            }
        }

        let mut warnings = BTreeSet::new();
        let mut facts = AuditFacts::default();
        let mut nodes: Vec<Option<MapNode>> = Vec::with_capacity(order.len());
        for (i, &(uid, _, rel)) in order.iter().enumerate() {
            if !included[i] {
                nodes.push(None);
                continue;
            }
            let atom = self.atom(uid)?;
            let is_root = atom.parent.to_native() == NO_UID;
            let doc = self.doc_of(uid)?;
            if rel == 0 || is_root {
                self.note_warnings(uid, &mut warnings)?;
            }
            facts.atoms.push(uid);
            nodes.push(Some(MapNode {
                id: atom.id.as_str().to_owned(),
                title: if is_root {
                    atom.l0.as_str().to_owned()
                } else {
                    atom.title.as_str().to_owned()
                },
                doc: is_root.then(|| format!("{} · {}", doc.meta.doc_type, doc.meta.status)),
                summary: with_summary[i].then(|| atom.l1.as_str().to_owned()),
                children: Vec::new(),
            }));
        }
        // Attach children to parents, deepest first so every subtree is complete.
        for i in (0..order.len()).rev() {
            let parent = order[i].1;
            if parent == u32::MAX {
                continue;
            }
            if let Some(node) = nodes[i].take()
                && let Some(Some(parent_node)) = nodes.get_mut(parent as usize)
            {
                parent_node.children.insert(0, node);
            }
        }
        let top: Vec<MapNode> = order
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.1 == u32::MAX)
            .filter_map(|(i, _)| nodes[i].take())
            .collect();
        let response = MapResponse {
            snapshot: self.pack.snapshot().to_owned(),
            snapshots: BTreeMap::new(),
            scope: label,
            nodes: top,
            omitted,
            budget: BudgetUse { limit: budget, used },
            warnings: warnings.into_iter().collect(),
            redacted: self.stub().then_some(hidden),
        };
        facts.blocked = hidden;
        Ok((response, facts))
    }

    fn map_roots(&self, view: &View, scope: &str) -> Result<(Vec<u32>, String)> {
        if scope.is_empty() {
            let roots = self
                .pack
                .data()
                .docs
                .iter()
                .map(|doc| doc.root.to_native())
                .filter(|uid| view.contains(*uid))
                .collect();
            return Ok((roots, String::new()));
        }
        if let Some(uid) = self.pack.resolve_id(scope)?
            && view.contains(uid)
        {
            let atom = self.atom(uid)?;
            return Ok((vec![uid], atom.id.as_str().to_owned()));
        }
        let prefix = scope.trim_start_matches("./").trim_start_matches('/');
        let roots = self
            .pack
            .data()
            .docs
            .iter()
            .filter(|doc| doc.path.as_str().starts_with(prefix))
            .map(|doc| doc.root.to_native())
            .filter(|uid| view.contains(*uid))
            .collect();
        Ok((roots, prefix.to_owned()))
    }

    fn count_visible_subtree(&self, view: &View, uid: u32) -> Result<u32> {
        let mut count = 0;
        let mut stack = vec![uid];
        while let Some(next) = stack.pop() {
            if !view.contains(next) {
                continue;
            }
            count += 1;
            stack.extend(self.children(next)?);
        }
        Ok(count)
    }

    /// `kiwi_read`: atoms at the asked level, with typed links and optional context.
    pub fn read(&self, view: &View, req: &ReadRequest) -> Result<(ReadResponse, AuditFacts)> {
        if req.ids.is_empty() {
            return Err(KiwiError::InvalidInput("ids must not be empty".into()));
        }
        if req.ids.len() > MAX_READ_IDS {
            return Err(KiwiError::InvalidInput(format!(
                "at most {MAX_READ_IDS} ids per read"
            )));
        }
        let level = req.level.unwrap_or(Level::L2);
        let context = req.context.unwrap_or_default();
        let mut atoms = Vec::new();
        let mut not_found = Vec::new();
        let mut hidden = Vec::new();
        let mut warnings = BTreeSet::new();
        let mut facts = AuditFacts::default();
        let mut seen = BTreeSet::new();
        for id in &req.ids {
            let resolved = self.pack.resolve_id(id)?;
            let Some(uid) = resolved else {
                not_found.push(id.clone());
                continue;
            };
            if !view.contains(uid) {
                facts.blocked += 1;
                if self.stub() {
                    let doc = self.doc_of(uid)?;
                    hidden.push(HiddenItem {
                        id: id.clone(),
                        owner: doc.meta.owner.as_ref().map(|o| o.as_str().to_owned()),
                        note: "hidden by policy; ask the owner for access".into(),
                    });
                } else {
                    not_found.push(id.clone());
                }
                continue;
            }
            if !seen.insert(uid) {
                continue;
            }
            atoms.push(self.read_item(view, uid, level, context)?);
            self.note_warnings(uid, &mut warnings)?;
            facts.atoms.push(uid);
        }
        let redacted = self.stub().then_some(hidden.len() as u32);
        Ok((
            ReadResponse {
                snapshot: self.pack.snapshot().to_owned(),
                snapshots: BTreeMap::new(),
                atoms,
                not_found,
                hidden,
                warnings: warnings.into_iter().collect(),
                redacted,
            },
            facts,
        ))
    }

    fn read_item(&self, view: &View, uid: u32, level: Level, context: ReadContext) -> Result<ReadItem> {
        let atom = self.atom(uid)?;
        let doc = self.doc_of(uid)?;
        let (text, tokens) = match level {
            Level::L0 => (atom.l0.as_str().to_owned(), atom.tokens.l0.to_native()),
            Level::L1 => (atom.l1.as_str().to_owned(), atom.tokens.l1.to_native()),
            Level::L2 => (self.pack.body(uid)?, atom.tokens.l2.to_native()),
        };
        let mut links = BTreeSet::new();
        for edge in self.pack.edges_from(uid) {
            let Some(kind) = EdgeKind::from_u8(edge.kind) else {
                continue;
            };
            let to = edge.to.to_native();
            if to == NO_UID && kind != EdgeKind::Code {
                continue;
            }
            if to == NO_UID {
                links.insert(Link {
                    rel: kind.name().to_owned(),
                    id: format!("code:{}", edge.external),
                });
            } else if view.contains(to) {
                links.insert(Link {
                    rel: kind.name().to_owned(),
                    id: self.atom(to)?.id.as_str().to_owned(),
                });
            }
        }
        for edge in self.pack.edges_to(uid) {
            let Some(kind) = EdgeKind::from_u8(edge.kind) else {
                continue;
            };
            let from = edge.from.to_native();
            if view.contains(from) {
                links.insert(Link {
                    rel: format!("~{}", kind.name()),
                    id: self.atom(from)?.id.as_str().to_owned(),
                });
            }
        }
        let mut items = Vec::new();
        let parent = atom.parent.to_native();
        let push = |items: &mut Vec<ContextItem>, relation: &str, other: u32| -> Result<()> {
            if view.contains(other) {
                let other_atom = self.atom(other)?;
                items.push(ContextItem {
                    relation: relation.to_owned(),
                    id: other_atom.id.as_str().to_owned(),
                    title: other_atom.l0.as_str().to_owned(),
                });
            }
            Ok(())
        };
        match context {
            ReadContext::None => {}
            ReadContext::Parent => {
                if parent != NO_UID {
                    push(&mut items, "parent", parent)?;
                }
            }
            ReadContext::Siblings => {
                if parent != NO_UID {
                    for sibling in self.children(parent)? {
                        if sibling != uid {
                            push(&mut items, "sibling", sibling)?;
                        }
                    }
                }
            }
            ReadContext::Children => {
                for child in self.children(uid)? {
                    push(&mut items, "child", child)?;
                }
            }
        }
        Ok(ReadItem {
            id: atom.id.as_str().to_owned(),
            title: atom.l0.as_str().to_owned(),
            level,
            text,
            tokens,
            doc_type: doc.meta.doc_type.as_str().to_owned(),
            status: doc.meta.status.as_str().to_owned(),
            owner: doc.meta.owner.as_ref().map(|o| o.as_str().to_owned()),
            review_by: doc.meta.review_by.as_ref().map(|r| r.as_str().to_owned()),
            cite: format!("kiwi://atom/{}@{}", atom.id, self.pack.snapshot()),
            links: links.into_iter().collect(),
            context: items,
        })
    }

    /// `kiwi_trace`: a breadth-first walk of the typed graph from one atom.
    pub fn trace(&self, view: &View, req: &TraceRequest) -> Result<(TraceResponse, AuditFacts)> {
        // `typed` (the default) follows every declared relation but not text mentions,
        // which are many and weak; `any` follows mentions too.
        let (relation, mentions) = match req.relation.as_deref().unwrap_or("typed") {
            "typed" | "" => (None, false),
            "any" => (None, true),
            name => (
                Some(
                    EdgeKind::parse(name)
                        .ok_or_else(|| KiwiError::InvalidInput(format!("unknown relation `{name}`")))?,
                ),
                true,
            ),
        };
        let direction = req.direction.unwrap_or_default();
        let depth = req.depth.unwrap_or(3).clamp(1, 6);
        let start = self
            .pack
            .resolve_id(&req.id)?
            .filter(|uid| view.contains(*uid))
            .ok_or_else(|| KiwiError::InvalidInput(format!("no atom `{}`", req.id)))?;

        let mut nodes = Vec::new();
        let mut edges = BTreeSet::new();
        let mut visited: BTreeSet<u32> = BTreeSet::new();
        let mut code_nodes: BTreeSet<String> = BTreeSet::new();
        let mut warnings = BTreeSet::new();
        let mut facts = AuditFacts::default();
        let mut queue = VecDeque::from([(start, 0u8)]);
        visited.insert(start);
        let wants = |kind: EdgeKind| match relation {
            Some(r) => r == kind,
            None => mentions || kind != EdgeKind::Mentions,
        };
        while let Some((uid, hops)) = queue.pop_front() {
            let atom = self.atom(uid)?;
            let doc = self.doc_of(uid)?;
            nodes.push(TraceNode {
                id: atom.id.as_str().to_owned(),
                title: atom.l0.as_str().to_owned(),
                doc_type: doc.meta.doc_type.as_str().to_owned(),
                status: doc.meta.status.as_str().to_owned(),
                hops,
            });
            facts.atoms.push(uid);
            self.note_warnings(uid, &mut warnings)?;
            if hops >= depth || nodes.len() >= MAX_TRACE_NODES {
                continue;
            }
            // A document root speaks for every section of its document.
            let sources: Vec<u32> = if atom.parent.to_native() == NO_UID {
                let first = doc.first.to_native();
                (first..first + doc.count.to_native())
                    .filter(|u| view.contains(*u))
                    .collect()
            } else {
                vec![uid]
            };
            let mut next: Vec<u32> = Vec::new();
            for source in sources {
                let source_id = self.atom(source)?.id.as_str().to_owned();
                if matches!(direction, Direction::Out | Direction::Both) {
                    for edge in self.pack.edges_from(source) {
                        let Some(kind) = EdgeKind::from_u8(edge.kind) else {
                            continue;
                        };
                        if !wants(kind) {
                            continue;
                        }
                        let to = edge.to.to_native();
                        if to == NO_UID && kind != EdgeKind::Code {
                            continue;
                        }
                        if to == NO_UID {
                            let code_id = format!("code:{}", edge.external);
                            edges.insert(TraceEdge {
                                from: source_id.clone(),
                                rel: kind.name().to_owned(),
                                to: code_id.clone(),
                            });
                            if code_nodes.insert(code_id.clone()) {
                                nodes.push(TraceNode {
                                    id: code_id,
                                    title: edge.external.as_str().to_owned(),
                                    doc_type: "code".into(),
                                    status: String::new(),
                                    hops: hops + 1,
                                });
                            }
                        } else if view.contains(to) {
                            edges.insert(TraceEdge {
                                from: source_id.clone(),
                                rel: kind.name().to_owned(),
                                to: self.atom(to)?.id.as_str().to_owned(),
                            });
                            next.push(to);
                        }
                    }
                }
                if matches!(direction, Direction::In | Direction::Both) {
                    for edge in self.pack.edges_to(source) {
                        let Some(kind) = EdgeKind::from_u8(edge.kind) else {
                            continue;
                        };
                        let from = edge.from.to_native();
                        if !wants(kind) || !view.contains(from) {
                            continue;
                        }
                        edges.insert(TraceEdge {
                            from: self.atom(from)?.id.as_str().to_owned(),
                            rel: kind.name().to_owned(),
                            to: source_id.clone(),
                        });
                        next.push(from);
                    }
                }
            }
            for uid in next {
                if visited.insert(uid) {
                    queue.push_back((uid, hops + 1));
                }
            }
        }
        // Edges may name nodes beyond the depth limit; keep only edges between nodes.
        let names: BTreeSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        let edges: Vec<TraceEdge> = edges
            .into_iter()
            .filter(|e| names.contains(e.from.as_str()) && names.contains(e.to.as_str()))
            .collect();
        Ok((
            TraceResponse {
                snapshot: self.pack.snapshot().to_owned(),
                snapshots: BTreeMap::new(),
                root: self.atom(start)?.id.as_str().to_owned(),
                nodes,
                edges,
                warnings: warnings.into_iter().collect(),
            },
            facts,
        ))
    }
}

/// The union of the key kinds of the targets inside `view`.
fn visible_kinds(targets: &[(u32, u8)], view: &View) -> u8 {
    targets
        .iter()
        .filter(|(uid, _)| view.contains(*uid))
        .fold(0, |kinds, (_, k)| kinds | k)
}
