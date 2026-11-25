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

/// The smallest token budget a request may name.
pub const MIN_BUDGET: u32 = 50;
/// The largest token budget of one answer, whatever a request or the configuration
/// names.
pub const MAX_BUDGET: u32 = 32_000;
/// `kiwi_map` budget when a request names none: the titles of about 150 documents, so
/// one call orients in a corpus or a folder of one.
pub const DEFAULT_MAP_BUDGET: u32 = 4_000;
/// `kiwi_search` budget for each query when a request names none: ten results, each
/// with its abstract (about 90 tokens a result). A search of several queries gets this
/// much for every query, up to [`MAX_BUDGET`].
pub const DEFAULT_SEARCH_BUDGET: u32 = 1_000;
/// `kiwi_search` results considered for each query before the budget cut when a
/// request names no limit.
pub const DEFAULT_SEARCH_LIMIT: u32 = 10;
/// The most results for each query a request may ask for.
pub const MAX_SEARCH_LIMIT: u32 = 50;
/// `kiwi_read` budget when a request names none: a few long documents whole, still
/// well under the 25,000-token ceiling Claude Code puts on one tool's output by default.
pub const DEFAULT_READ_BUDGET: u32 = 12_000;

/// Planner settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EngineConfig {
    /// `kiwi_map` budget when a request names none.
    pub map_budget: u32,
    /// `kiwi_search` budget for each query when a request names none.
    pub search_budget: u32,
    /// `kiwi_search` results for each query when a request names no limit.
    pub search_limit: u32,
    /// `kiwi_read` budget when a request names none.
    pub read_budget: u32,
    /// The `k` of reciprocal rank fusion.
    pub rrf_k: f64,
    /// Score multiplier for atoms of approved documents. Fused scores near the top
    /// differ by about 1.6 % per rank (`1 / (rrf_k + rank)`), so 1.02 moves an approved
    /// atom up by about one place: it breaks near ties, never outranks relevance.
    pub approved_boost: f64,
    /// Hidden-atom display.
    pub hidden: HiddenMode,
}

impl EngineConfig {
    /// The token budget of a `kiwi_map` request under these settings.
    pub fn map_budget(&self, requested: Option<u32>) -> u32 {
        requested.unwrap_or(self.map_budget).clamp(MIN_BUDGET, MAX_BUDGET)
    }

    /// The token budget of a `kiwi_read` request under these settings.
    pub fn read_budget(&self, requested: Option<u32>) -> u32 {
        requested
            .unwrap_or(self.read_budget)
            .clamp(MIN_BUDGET, MAX_BUDGET)
    }

    /// The token budget of a `kiwi_search` request: its budget for each query (or the
    /// default) times the number of queries, at most [`MAX_BUDGET`].
    pub fn search_budget(&self, req: &SearchRequest) -> u32 {
        let each = req
            .budget
            .unwrap_or(self.search_budget)
            .clamp(MIN_BUDGET, MAX_BUDGET);
        let queries = u32::try_from(query_count(req)).unwrap_or(u32::MAX);
        each.saturating_mul(queries).min(MAX_BUDGET)
    }

    /// The results a search considers for each of its queries.
    pub fn query_limit(&self, req: &SearchRequest) -> usize {
        req.limit.unwrap_or(self.search_limit).clamp(1, MAX_SEARCH_LIMIT) as usize
    }

    /// The results a search considers before the budget cut: its limit for each query
    /// times the number of queries.
    pub fn search_limit(&self, req: &SearchRequest) -> usize {
        self.query_limit(req) * query_count(req)
    }
}

/// The queries a search answers: its non-blank queries, and at least one (a search by
/// filters alone).
fn query_count(req: &SearchRequest) -> usize {
    req.queries.iter().filter(|q| !q.trim().is_empty()).count().max(1)
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            map_budget: DEFAULT_MAP_BUDGET,
            search_budget: DEFAULT_SEARCH_BUDGET,
            search_limit: DEFAULT_SEARCH_LIMIT,
            read_budget: DEFAULT_READ_BUDGET,
            rrf_k: 60.0,
            approved_boost: 1.02,
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

/// The most queries one search may pass.
pub const MAX_QUERIES: usize = 8;
const MAX_QUERY_CHARS: usize = 512;
/// The most ids one read may name.
pub const MAX_READ_IDS: usize = 20;
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

/// One fused atom: its score, the tiers that found it, and its score from each query's
/// own lists.
type Fused = (f64, BTreeSet<String>, [f64; MAX_QUERIES]);

struct Fusion {
    k: f64,
    scores: HashMap<u32, Fused>,
}

impl Fusion {
    /// Adds one ranked list, found for the queries in `queries` (none for lists no
    /// single query produced, such as graph neighbours).
    fn add(&mut self, label: &str, weight: f64, ranked: &[u32], queries: QueryMask) {
        let mut seen = BTreeSet::new();
        let mut rank = 0usize;
        for &uid in ranked {
            if !seen.insert(uid) {
                continue;
            }
            rank += 1;
            let share = weight / (self.k + rank as f64);
            let entry = self
                .scores
                .entry(uid)
                .or_insert((0.0, BTreeSet::new(), [0.0; MAX_QUERIES]));
            entry.0 += share;
            entry.1.insert(label.to_owned());
            for (index, own) in entry.2.iter_mut().enumerate() {
                if queries & query_bit(index) != 0 {
                    *own += share;
                }
            }
        }
    }
}

/// A set of the queries of one search: bit `i` is query `i` of the request.
pub type QueryMask = u32;

/// The results each query of a search keeps, whatever the other queries rank: its own
/// best three. Fusion favours what several queries agree on, which suits synonyms of one
/// question; this floor keeps a query about something else from being crowded out.
pub const QUERY_FLOOR: usize = 3;

/// The mask of query `index` alone.
fn query_bit(index: usize) -> QueryMask {
    1 << index
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
    /// The queries it is one of the own best results of (at most [`QUERY_FLOOR`] each),
    /// which a search keeps above its cut.
    pub queries: QueryMask,
}

impl RankedAtom {
    /// An atom with no score yet.
    fn new(uid: u32) -> Self {
        Self {
            uid,
            score: 0.0,
            why: BTreeSet::new(),
            replaces: BTreeSet::new(),
            queries: 0,
        }
    }
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

/// Keeps every item that is one of its query's own best (a non-empty mask) among the
/// first `limit` of `items` (ranked best first), as far as `limit` allows: each one found
/// below the cut takes the place of the lowest kept item that is no query's own best,
/// which moves to just below the cut. The order is otherwise kept, so a search whose
/// queries all rank well already changes nothing.
pub fn cover_queries<T>(items: &mut Vec<T>, limit: usize, queries: impl Fn(&T) -> QueryMask) {
    if limit == 0 {
        return;
    }
    let mut next = limit;
    while next < items.len() {
        if queries(&items[next]) == 0 {
            next += 1;
            continue;
        }
        let Some(spare) = (0..limit).rev().find(|&slot| queries(&items[slot]) == 0) else {
            return;
        };
        let promoted = items.remove(next);
        let demoted = items.remove(spare);
        items.insert(limit - 1, promoted);
        items.insert(limit, demoted);
        next += 1;
    }
}

/// The ids of a read: at least one, at most [`MAX_READ_IDS`].
pub fn check_read_ids(req: &ReadRequest) -> Result<()> {
    if req.ids.is_empty() {
        return Err(KiwiError::InvalidInput("ids must not be empty".into()));
    }
    if req.ids.len() > MAX_READ_IDS {
        return Err(KiwiError::InvalidInput(format!(
            "at most {MAX_READ_IDS} ids per read"
        )));
    }
    Ok(())
}

/// Whether `anchor` names a part a long section with anchor `head` was split into
/// (`head~2`, `head~3`…).
fn is_part(head: &str, anchor: &str) -> bool {
    anchor
        .strip_prefix(head)
        .and_then(|rest| rest.strip_prefix('~'))
        .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
}

/// The budget of one `kiwi_read` answer and how much of it is spent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadBudget {
    /// Tokens of text the answer may hold.
    pub limit: u32,
    /// Tokens of text given so far.
    pub used: u32,
    /// Whether any section was given: the first always is, whatever its size.
    pub started: bool,
}

impl ReadBudget {
    /// A budget of `limit` tokens, nothing spent.
    pub fn new(limit: u32) -> Self {
        Self {
            limit,
            used: 0,
            started: false,
        }
    }

    /// The answer's token use.
    pub fn usage(&self) -> BudgetUse {
        BudgetUse {
            limit: self.limit,
            used: self.used,
        }
    }
}

/// What reading one asked id gave.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReadStep {
    /// Its text, when any of it was given.
    pub item: Option<ReadItem>,
    /// The atoms whose text was given.
    pub atoms: Vec<u32>,
    /// The id names nothing the caller can see (or, with `hidden = "invisible"`,
    /// something hidden).
    pub not_found: bool,
    /// The stub of a hidden atom (with `hidden = "stub"`).
    pub hidden: Option<HiddenItem>,
    /// Asked atoms withheld by the policy.
    pub blocked: u32,
    /// Hidden atoms to count as redacted (with `hidden = "stub"`).
    pub redacted: u32,
    /// Notices.
    pub warnings: BTreeSet<Warning>,
    /// Set when the budget ran out inside this id's text.
    pub cut: Option<ReadCut>,
}

/// Where a read ran out of budget, inside the text of one asked id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadCut {
    /// The last section of that text given, to read on after.
    pub after: Option<String>,
    /// The highest sections of that text left out.
    pub left_out: Vec<String>,
    /// Sections of that text left out.
    pub sections: u32,
    /// Their tokens.
    pub tokens: u32,
}

impl ReadCut {
    /// What the answer to `req` left out when the cut is in the text of its id at
    /// `index`, before the later ids are counted in with [`ReadMore::add`].
    pub fn into_more(self, req: &ReadRequest, index: usize) -> ReadMore {
        ReadMore {
            sections: self.sections,
            tokens: self.tokens,
            left_out: self.left_out,
            next: ReadRequest {
                ids: req.ids[index..].to_vec(),
                level: req.level,
                context: req.context,
                budget: req.budget,
                after: self.after,
            },
        }
    }
}

impl ReadMore {
    /// Counts in an asked id the answer did not reach, of `sections` sections and
    /// `tokens` tokens (none when it names nothing the caller can see).
    pub fn add(&mut self, id: &str, sections: u32, tokens: u32) {
        if sections > 0 {
            self.sections += sections;
            self.tokens += tokens;
            self.left_out.push(id.to_owned());
        }
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
        let limit = self.config.query_limit(req);
        let candidates = self.candidates(view, &req.filters)?;
        let mut fusion = Fusion {
            k: self.config.rrf_k,
            scores: HashMap::new(),
        };
        let mut blocked = 0u32;
        // The retrievers' queries, and for each the request's queries it stands for
        // (several when they fold to one text; none for a glossary expansion). The
        // first `originals.len()` of `lexical` are the originals.
        let mut lexical: Vec<String> = Vec::new();
        let mut originals: Vec<String> = Vec::new();
        let mut origins: Vec<QueryMask> = Vec::new();
        let mut seen_lexical: BTreeMap<String, usize> = BTreeMap::new();
        let mut exact_all: Vec<u32> = Vec::new();
        // Glossary terms matched, and the queries that matched each.
        let mut term_uids: BTreeMap<u32, QueryMask> = BTreeMap::new();
        let mut all_resolved = true;
        let mut any_query = false;
        let mut degraded: BTreeSet<String> = BTreeSet::new();

        // Per query: atoms of documents named inside it (`steps of RFC-010`), and the
        // query text without those ids, for a retrieval scoped to those documents.
        let mut scoped: Vec<(RoaringBitmap, String, QueryMask)> = Vec::new();
        for (index, query) in req.queries.iter().enumerate() {
            let folded = text::key(query);
            if folded.is_empty() {
                continue;
            }
            any_query = true;
            let bit = query_bit(index);
            match seen_lexical.get(&folded) {
                Some(&at) => origins[at] |= bit,
                None => {
                    seen_lexical.insert(folded.clone(), lexical.len());
                    lexical.push(query.trim().to_owned());
                    originals.push(query.trim().to_owned());
                    origins.push(bit);
                }
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
                            *term_uids.entry(uid).or_default() |= bit;
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
                                *term_uids.entry(uid).or_default() |= bit;
                            }
                        }
                    }
                }
            }
            fusion.add("exact", EXACT_WEIGHT, &whole, bit);
            fusion.add("exact", ID_WEIGHT, &ids, bit);
            fusion.add("term", TERM_WEIGHT, &terms, bit);
            exact_all.extend(whole.iter().chain(&ids).copied());
            if !scope_docs.is_empty() {
                let rest: Vec<&str> = words.iter().copied().filter(|w| !id_words.contains(w)).collect();
                if !rest.is_empty() {
                    scoped.push((&scope_docs & &candidates, rest.join(" "), bit));
                }
            }
        }

        // Glossary expansion: a matched term or alias adds its synonyms as queries.
        for entry in self.pack.data().glossary.iter() {
            let Some(&asked) = term_uids.get(&entry.uid.to_native()) else {
                continue;
            };
            let names =
                std::iter::once(entry.term.as_str()).chain(entry.aliases.iter().map(|alias| alias.as_str()));
            for name in names {
                let key = text::key(name);
                if !seen_lexical.contains_key(&key) && lexical.len() < MAX_QUERIES * 2 {
                    seen_lexical.insert(key, lexical.len());
                    lexical.push(name.to_owned());
                    origins.push(asked);
                }
            }
        }

        // Structural graph prior: documents one typed hop from an exact match.
        let graph = self.graph_neighbours(view, &candidates, &exact_all)?;
        fusion.add("graph", GRAPH_WEIGHT, &graph, 0);

        if !any_query {
            let listing: Vec<u32> = candidates.iter().take(limit * 4).collect();
            fusion.add("filter", RETRIEVER_WEIGHT, &listing, 0);
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
                for (hits, &queries) in batches.iter().zip(&origins) {
                    let ranked: Vec<u32> = hits
                        .iter()
                        .map(|hit| hit.uid)
                        .filter(|uid| candidates.contains(*uid))
                        .collect();
                    fusion.add(retriever.name(), RETRIEVER_WEIGHT, &ranked, queries);
                }
                if !expands {
                    continue;
                }
                // Structural scope: the rest of a query, within the documents it names.
                for (docs, rest, queries) in &scoped {
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
                    fusion.add("scoped", RETRIEVER_WEIGHT, &ranked, *queries);
                }
            }
        }

        let items = self.apply_priors(view, fusion, QUERY_FLOOR.min(limit))?;
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
        let budget = self.config.search_budget(req);
        let total = ranking.items.len();
        let limit = self.config.search_limit(req);
        let mut degraded = ranking.degraded;
        // The reranker reorders the first `top_k` fused results; the first `limit` of
        // the new order are kept, with at least one result for every query that has one.
        let mut top = ranking.items;
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
        cover_queries(&mut top, limit, |atom| atom.queries);
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

    /// The fused atoms with status priors and deprecation redirects applied, best
    /// first, each query's own best `floor` marked in their `queries`.
    fn apply_priors(&self, view: &View, fusion: Fusion, floor: usize) -> Result<Vec<RankedAtom>> {
        let mut merged: HashMap<u32, RankedAtom> = HashMap::new();
        let mut own: HashMap<u32, [f64; MAX_QUERIES]> = HashMap::new();
        for (uid, (mut score, mut why, scores)) in fusion.scores {
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
                            let next = merged
                                .entry(successor)
                                .or_insert_with(|| RankedAtom::new(successor));
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
            let ranked = merged.entry(target).or_insert_with(|| RankedAtom::new(target));
            ranked.score = ranked.score.max(score);
            ranked.why.extend(why);
            ranked.replaces.extend(replaced);
            // A redirected atom's own scores count for its replacement.
            let best = own.entry(target).or_insert([0.0; MAX_QUERIES]);
            for (best, score) in best.iter_mut().zip(scores) {
                *best = best.max(score);
            }
        }
        let mut ranked: Vec<RankedAtom> = merged.into_values().collect();
        ranked.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.uid.cmp(&b.uid)));
        for query in 0..MAX_QUERIES {
            let score = |item: &RankedAtom| own.get(&item.uid).map_or(0.0, |scores| scores[query]);
            let mut best: Vec<usize> = (0..ranked.len()).filter(|&i| score(&ranked[i]) > 0.0).collect();
            // By the query's own score; ties in the fused order.
            best.sort_by(|&a, &b| score(&ranked[b]).total_cmp(&score(&ranked[a])).then(a.cmp(&b)));
            for &index in best.iter().take(floor) {
                ranked[index].queries |= query_bit(query);
            }
        }
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
        self.map_within(view, req, self.config.map_budget(req.budget))
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

    /// `kiwi_read`: atoms at the asked level, with typed links and optional context. At
    /// L2 each id is read whole (its subsections follow it), within the budget; the
    /// first section of an answer is always given, so reading on always progresses.
    pub fn read(&self, view: &View, req: &ReadRequest) -> Result<(ReadResponse, AuditFacts)> {
        check_read_ids(req)?;
        let level = req.level.unwrap_or(Level::L2);
        let context = req.context.unwrap_or_default();
        let mut budget = ReadBudget::new(self.config.read_budget(req.budget));
        let mut seen = BTreeSet::new();
        let mut atoms = Vec::new();
        let mut not_found = Vec::new();
        let mut hidden = Vec::new();
        let mut warnings = BTreeSet::new();
        let mut facts = AuditFacts::default();
        let mut redacted = 0u32;
        let mut more = None;
        for (index, id) in req.ids.iter().enumerate() {
            let after = if index == 0 { req.after.as_deref() } else { None };
            let step = self.read_step(view, id, after, level, context, &mut budget, &mut seen)?;
            if step.not_found {
                not_found.push(id.clone());
            }
            hidden.extend(step.hidden);
            warnings.extend(step.warnings);
            facts.blocked += step.blocked;
            facts.atoms.extend(step.atoms);
            redacted += step.redacted;
            atoms.extend(step.item);
            if let Some(cut) = step.cut {
                let mut rest = cut.into_more(req, index);
                for later in &req.ids[index + 1..] {
                    let (sections, tokens) = self.read_size(view, later, level, &seen)?;
                    rest.add(later, sections, tokens);
                }
                more = Some(rest);
                break;
            }
        }
        Ok((
            ReadResponse {
                snapshot: self.pack.snapshot().to_owned(),
                snapshots: BTreeMap::new(),
                atoms,
                not_found,
                hidden,
                warnings: warnings.into_iter().collect(),
                redacted: self.stub().then_some(redacted),
                budget: budget.usage(),
                more,
            },
            facts,
        ))
    }

    /// Reads one asked id into an answer spending `budget`: its text (at L2 the whole of
    /// it, or what follows the section `after`) as far as the budget goes, leaving out
    /// the sections in `seen` and adding to it the ones it gives.
    #[allow(clippy::too_many_arguments)]
    pub fn read_step(
        &self,
        view: &View,
        asked: &str,
        after: Option<&str>,
        level: Level,
        context: ReadContext,
        budget: &mut ReadBudget,
        seen: &mut BTreeSet<u32>,
    ) -> Result<ReadStep> {
        let mut step = ReadStep::default();
        let Some(uid) = self.pack.resolve_id(asked)? else {
            step.not_found = true;
            return Ok(step);
        };
        if !view.contains(uid) {
            step.blocked = 1;
            if self.stub() {
                let doc = self.doc_of(uid)?;
                step.hidden = Some(HiddenItem {
                    id: asked.to_owned(),
                    owner: doc.meta.owner.as_ref().map(|o| o.as_str().to_owned()),
                    note: "hidden by policy; ask the owner for access".into(),
                });
                step.redacted = 1;
            } else {
                step.not_found = true;
            }
            return Ok(step);
        }
        if after.is_none() && seen.contains(&uid) {
            return Ok(step);
        }
        let (span, hidden) = self.span(view, uid, level)?;
        if self.stub() {
            step.redacted = hidden;
        }
        let start = match after {
            None => 0,
            Some(after) => {
                let resumed = self
                    .pack
                    .resolve_id(after)?
                    .and_then(|section| span.iter().position(|&atom| atom == section));
                let Some(position) = resumed else {
                    return Err(KiwiError::InvalidInput(format!(
                        "`after` names no section of `{asked}`: copy `ids` and `after` from `more.next`"
                    )));
                };
                position + 1
            }
        };
        let mut included = Vec::new();
        for (offset, &atom) in span[start..].iter().enumerate() {
            if seen.contains(&atom) {
                continue;
            }
            let cost = self.level_tokens(atom, level)?;
            if budget.started && budget.used + cost > budget.limit {
                let rest: Vec<u32> = span[start + offset..]
                    .iter()
                    .copied()
                    .filter(|atom| !seen.contains(atom))
                    .collect();
                let last = match included.last() {
                    Some(&last) => Some(self.atom(last)?.id.as_str().to_owned()),
                    None => after.map(str::to_owned),
                };
                step.cut = Some(self.cut(&rest, level, last)?);
                break;
            }
            budget.used += cost;
            budget.started = true;
            seen.insert(atom);
            included.push(atom);
        }
        if !included.is_empty() {
            step.item = Some(self.read_item(view, uid, &included, level, context)?);
            self.note_warnings(uid, &mut step.warnings)?;
            step.atoms = included;
        }
        Ok(step)
    }

    /// The sections and tokens a read of `asked` would give, leaving out `seen`: what a
    /// cut answer had not reached.
    pub fn read_size(
        &self,
        view: &View,
        asked: &str,
        level: Level,
        seen: &BTreeSet<u32>,
    ) -> Result<(u32, u32)> {
        let Some(uid) = self.pack.resolve_id(asked)? else {
            return Ok((0, 0));
        };
        if !view.contains(uid) {
            return Ok((0, 0));
        }
        let (span, _) = self.span(view, uid, level)?;
        let (mut sections, mut tokens) = (0u32, 0u32);
        for atom in span.into_iter().filter(|atom| !seen.contains(atom)) {
            sections += 1;
            tokens += self.level_tokens(atom, level)?;
        }
        Ok((sections, tokens))
    }

    /// Tokens of an atom's text at `level`.
    fn level_tokens(&self, uid: u32, level: Level) -> Result<u32> {
        let tokens = &self.atom(uid)?.tokens;
        Ok(match level {
            Level::L0 => tokens.l0.to_native(),
            Level::L1 => tokens.l1.to_native(),
            Level::L2 => tokens.l2.to_native(),
        })
    }

    /// The visible atoms whose text a read of `uid` gives, in document order: the atom,
    /// and at L2 the parts it was split into and its visible subsections (a hidden
    /// section is left out with everything under it); and how many hidden sections were
    /// left out.
    fn span(&self, view: &View, uid: u32, level: Level) -> Result<(Vec<u32>, u32)> {
        let mut atoms = vec![uid];
        if level != Level::L2 {
            return Ok((atoms, 0));
        }
        let head = self.atom(uid)?;
        let doc = self.doc_of(uid)?;
        let end = doc.first.to_native() + doc.count.to_native();
        let mut inside: BTreeSet<u32> = BTreeSet::from([uid]);
        let mut withheld: BTreeSet<u32> = BTreeSet::new();
        let mut hidden = 0u32;
        for candidate in uid + 1..end {
            let atom = self.atom(candidate)?;
            let parent = atom.parent.to_native();
            let continues_head =
                parent == head.parent.to_native() && is_part(head.anchor.as_str(), atom.anchor.as_str());
            if withheld.contains(&parent) {
                withheld.insert(candidate);
            } else if inside.contains(&parent) || continues_head {
                if view.contains(candidate) {
                    inside.insert(candidate);
                    atoms.push(candidate);
                } else {
                    withheld.insert(candidate);
                    hidden += 1;
                }
            }
        }
        Ok((atoms, hidden))
    }

    /// Where a read stops when `rest` (the sections of one id's text not given, in
    /// document order) does not fit; `last` is the last section of that text given.
    fn cut(&self, rest: &[u32], level: Level, last: Option<String>) -> Result<ReadCut> {
        let members: BTreeSet<u32> = rest.iter().copied().collect();
        let mut cut = ReadCut {
            after: last,
            left_out: Vec::new(),
            sections: 0,
            tokens: 0,
        };
        for &uid in rest {
            let atom = self.atom(uid)?;
            cut.sections += 1;
            cut.tokens += self.level_tokens(uid, level)?;
            if !members.contains(&atom.parent.to_native()) {
                cut.left_out.push(atom.id.as_str().to_owned());
            }
        }
        Ok(cut)
    }

    /// The answer for `uid` whose text is that of `included` (the atom itself, or the
    /// sections of its text a read gives), with the typed links of those sections.
    fn read_item(
        &self,
        view: &View,
        uid: u32,
        included: &[u32],
        level: Level,
        context: ReadContext,
    ) -> Result<ReadItem> {
        let atom = self.atom(uid)?;
        let doc = self.doc_of(uid)?;
        let mut texts = Vec::with_capacity(included.len());
        let mut tokens = 0u32;
        let mut links = BTreeSet::new();
        for &section in included {
            let part = self.atom(section)?;
            texts.push(match level {
                Level::L0 => part.l0.as_str().to_owned(),
                Level::L1 => part.l1.as_str().to_owned(),
                Level::L2 => self.pack.body(section)?,
            });
            tokens += self.level_tokens(section, level)?;
            self.links_of(view, section, &mut links)?;
        }
        let text = texts.join("\n\n");
        let ids = included
            .iter()
            .map(|&section| self.atom(section).map(|a| a.id.as_str().to_owned()))
            .collect::<Result<Vec<_>>>()?;
        // Links between the sections of this one text say nothing the text does not.
        links.retain(|link: &Link| !ids.contains(&link.id));
        let sections = if included == [uid] { Vec::new() } else { ids };
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
            sections,
        })
    }

    /// Adds the typed links from and to `uid` that end at visible atoms.
    fn links_of(&self, view: &View, uid: u32, links: &mut BTreeSet<Link>) -> Result<()> {
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
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Items `(name, queries)`, best first.
    fn ranked(items: &[(&'static str, QueryMask)]) -> Vec<(&'static str, QueryMask)> {
        items.to_vec()
    }

    fn names(items: &[(&'static str, QueryMask)], limit: usize) -> Vec<&'static str> {
        items.iter().take(limit).map(|(name, _)| *name).collect()
    }

    #[test]
    fn each_query_keeps_its_own_best_above_the_cut() {
        // Query 1's own best ranks below the cut: it takes the place of the lowest kept
        // item that is no query's own best, which moves just below the cut.
        let mut items = ranked(&[("a", 0b01), ("b", 0), ("c", 0b01), ("d", 0), ("e", 0b10)]);
        cover_queries(&mut items, 3, |item| item.1);
        assert_eq!(names(&items, 5), ["a", "c", "e", "b", "d"]);

        // Several below the cut keep their fused order.
        let mut items = ranked(&[("a", 0), ("b", 0), ("c", 0), ("d", 0b01), ("e", 0b10)]);
        cover_queries(&mut items, 3, |item| item.1);
        assert_eq!(names(&items, 5), ["a", "d", "e", "b", "c"]);

        // Every own best above the cut already: nothing moves.
        let order = ranked(&[("a", 0b01), ("b", 0b10), ("c", 0), ("d", 0)]);
        let mut items = order.clone();
        cover_queries(&mut items, 2, |item| item.1);
        assert_eq!(items, order);

        // More own bests than the cut holds: the cut is never exceeded.
        let mut items = ranked(&[("a", 0b001), ("b", 0b010), ("c", 0b100)]);
        cover_queries(&mut items, 1, |item| item.1);
        assert_eq!(names(&items, 1), ["a"]);
    }

    #[test]
    fn split_parts_continue_their_section() {
        assert!(is_part("pricing", "pricing~2"));
        assert!(is_part("", "~3"));
        assert!(!is_part("pricing", "pricing"));
        assert!(!is_part("pricing", "pricing~"));
        assert!(!is_part("pricing", "pricing~2a"));
        assert!(!is_part("pricing", "pricing-tiers"));
    }

    #[test]
    fn search_budgets_and_limits_grow_with_the_queries() {
        let config = EngineConfig::default();
        let search = |queries: &[&str], budget: Option<u32>| SearchRequest {
            queries: queries.iter().map(|q| (*q).to_owned()).collect(),
            budget,
            ..SearchRequest::default()
        };
        let one = search(&["a"], None);
        let five = search(&["a", "b", "c", "d", " "], None);
        assert_eq!(config.search_budget(&one), DEFAULT_SEARCH_BUDGET);
        assert_eq!(config.search_budget(&five), 4 * DEFAULT_SEARCH_BUDGET);
        assert_eq!(config.search_limit(&five), 4 * DEFAULT_SEARCH_LIMIT as usize);
        assert_eq!(config.search_budget(&search(&[], None)), DEFAULT_SEARCH_BUDGET);
        let many = search(&["a", "b", "c", "d"], Some(MAX_BUDGET));
        assert_eq!(config.search_budget(&many), MAX_BUDGET);
        assert_eq!(config.read_budget(None), DEFAULT_READ_BUDGET);
        assert_eq!(config.read_budget(Some(1)), MIN_BUDGET);
        assert_eq!(config.map_budget(Some(u32::MAX)), MAX_BUDGET);
    }
}
