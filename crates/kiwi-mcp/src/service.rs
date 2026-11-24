//! The tool service behind both transports: one corpus, or several federated by
//! namespace.
//!
//! With one corpus, ids are the corpus's own (`adr/adr-005#decision`). With several,
//! every id in an answer is qualified with its namespace (`acme:adr/adr-005#decision`),
//! inputs may be qualified or bare (a bare id must name an atom the caller can see in
//! exactly one corpus), and:
//!
//! - each corpus applies its own policy, principals and hidden mode, and computes its
//!   ranking (BM25 statistics included) inside the caller's view of that corpus only;
//! - a search merges the corpora's rankings by fused score, reranks the merged top when
//!   a reranker is configured (only atoms whose corpus lets the reranker read them), and
//!   cuts to one budget;
//! - a corpus the caller can see nothing of contributes nothing, is not listed, and
//!   answers exactly like a namespace that does not exist;
//! - every corpus consulted appends its own audit record, with its own atoms.
//!
//! The search settings (budget, limit, reranker) of the first corpus the caller can see
//! apply to federated answers, so a corpus the caller cannot see never shapes one; a
//! caller who sees nothing gets the defaults.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use kiwi_core::api::{
    AuditFacts, BudgetUse, ContextItem, HiddenItem, Link, MapNode, MapRequest, MapResponse, ReadRequest,
    ReadResponse, SearchRequest, SearchResponse, TraceRequest, TraceResponse, Warning,
};
use kiwi_core::{Engine, EngineConfig, RankedAtom, View, fit_budget, text};
use kiwi_policy::Identity;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use crate::corpus::{Corpus, CorpusPlugins, Served, ServiceOptions, engine_config, open_engine};
use crate::feedback::{FeedbackRecord, Verdict};

/// Errors a tool call can end with.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// The caller sent something invalid; the message is shown to the agent.
    #[error("{0}")]
    Invalid(String),
    /// The server failed. The message (paths, a tool's output) is for the operator's
    /// log only: MCP clients receive a generic message with a reference to it.
    #[error("internal error: {0}")]
    Internal(String),
}

impl From<kiwi_core::KiwiError> for ToolError {
    fn from(error: kiwi_core::KiwiError) -> Self {
        match error {
            kiwi_core::KiwiError::InvalidInput(message) => ToolError::Invalid(message),
            other => ToolError::Internal(other.to_string()),
        }
    }
}

impl From<kiwi_policy::PolicyError> for ToolError {
    fn from(error: kiwi_policy::PolicyError) -> Self {
        ToolError::Internal(error.to_string())
    }
}

/// `kiwi_feedback` input.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FeedbackRequest {
    /// The query or question the feedback is about.
    pub query: String,
    /// Atom ids concerned (may be empty for `missing`).
    #[serde(default)]
    pub ids: Vec<String>,
    /// `useful`, `wrong` or `missing`.
    pub verdict: Verdict,
    /// Optional note for the writers.
    #[serde(default)]
    pub note: Option<String>,
    /// On a federated server: the corpus a verdict without ids is about (default: the
    /// first one you can read).
    #[serde(default)]
    pub namespace: Option<String>,
}

/// One change in `kiwi_propose`. Exactly one form:
/// - `section` + `content`: replace the body of one section (its heading line, anchor
///   and access attributes are kept);
/// - `path` + `set`: set frontmatter keys of a document (e.g. `{"status": "deprecated"}`);
/// - `path` + `content`: create a file or replace a whole file;
/// - `path` alone: delete a file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProposeChange {
    /// Corpus-relative Markdown path, e.g. `adr/adr-009-retry-budgets.md`.
    #[serde(default)]
    pub path: Option<String>,
    /// An atom id (`path#anchor`) whose body to replace.
    #[serde(default)]
    pub section: Option<String>,
    /// The whole file (with `path`) or the new section body (with `section`).
    #[serde(default)]
    pub content: Option<String>,
    /// Frontmatter keys to set (with `path`).
    #[serde(default)]
    pub set: Option<BTreeMap<String, serde_json::Value>>,
}

/// `kiwi_propose` input.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProposeRequest {
    /// The changes (1 to 20).
    pub changes: Vec<ProposeChange>,
    /// Why the change is needed: shown to reviewers in the pull request.
    pub rationale: String,
    /// Pull request title (default: derived from the first change).
    #[serde(default)]
    pub title: Option<String>,
    /// On a federated server: the corpus to change (required there; one per proposal).
    #[serde(default)]
    pub namespace: Option<String>,
}

#[derive(Debug, Serialize)]
struct FeedbackResponse<'a> {
    snapshot: &'a str,
    recorded: bool,
    gap: bool,
}

#[derive(Debug, Serialize)]
struct ProposeResponse<'a> {
    snapshot: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    namespace: Option<&'a str>,
    status: &'a str,
    id: &'a str,
    branch: &'a str,
    link: &'a str,
    merge: &'a str,
    lint: &'a [kiwi_core::plugin::Finding],
}

type CacheKey = (Vec<(String, Vec<u32>)>, &'static str, String);
type CachedAnswer = (String, Vec<AuditFacts>);
type Answer = (String, Vec<AuditFacts>);

/// The response cache holds at most this many answers and this many bytes of them; when
/// either is reached it starts over (answers of replaced packs go with it).
const CACHE_CAPACITY: usize = 10_000;
const CACHE_BYTES: usize = 64 * 1024 * 1024;

/// Cached answers and the bytes their texts take.
#[derive(Default)]
struct Cache {
    answers: HashMap<CacheKey, CachedAnswer>,
    bytes: usize,
}
/// Tokens a namespace node costs in a federated map.
const NAMESPACE_NODE_TOKENS: u32 = 6;

/// One corpus as a caller sees it during one call: one served state (pack, policy and
/// configuration together), taken once.
struct Scoped {
    served: Arc<Served>,
    engine: Arc<Engine>,
    view: View,
    classes: Vec<u32>,
}

impl Scoped {
    fn visible(&self) -> bool {
        !self.view.visible().is_empty()
    }
}

/// The access chains of the permission classes the caller may read in one corpus.
fn readable_chains(scope: &Scoped) -> Vec<Vec<kiwi_core::model::AccessDecl>> {
    let classes = &scope.engine.pack().data().classes;
    scope
        .classes
        .iter()
        .filter_map(|&class| classes.get(class as usize))
        .map(|class| {
            class
                .chain
                .iter()
                .map(|decl| {
                    kiwi_core::model::AccessDecl::new(
                        decl.label.as_str(),
                        decl.groups.iter().map(|g| g.as_str().to_owned()),
                    )
                })
                .collect()
        })
        .collect()
}

/// The corpus whose settings shape a federated answer: the first one the caller can
/// see, never a hidden one.
fn lead(scoped: &[Scoped]) -> Option<usize> {
    scoped.iter().position(Scoped::visible)
}

/// An id located in one corpus.
enum Located<'a> {
    /// In corpus `index`, with the corpus's own id.
    Found(usize, &'a str),
    /// Nowhere the caller can see.
    Missing,
}

/// The tool service.
pub struct KiwiService {
    corpora: Vec<Corpus>,
    cache: Mutex<Cache>,
}

impl std::fmt::Debug for KiwiService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KiwiService")
            .field("corpora", &self.corpora)
            .finish()
    }
}

impl KiwiService {
    /// A service over one corpus.
    pub fn open(
        options: ServiceOptions,
        gate: kiwi_policy::PolicyGate,
        plugins: CorpusPlugins,
    ) -> Result<Self, ToolError> {
        Self::federate(vec![Corpus::open(options, gate, plugins)?])
    }

    /// A service over several corpora, by namespace (they must differ). The search
    /// settings of the first corpus the caller can see apply to federated answers.
    pub fn federate(mut corpora: Vec<Corpus>) -> Result<Self, ToolError> {
        if corpora.is_empty() {
            return Err(ToolError::Internal("no corpus to serve".into()));
        }
        let mut seen = BTreeSet::new();
        for corpus in &corpora {
            if !seen.insert(corpus.namespace().to_owned()) {
                return Err(ToolError::Internal(format!(
                    "two corpora use the namespace `{}`; give each its own `namespace` in .kiwi/kiwi.toml",
                    corpus.namespace()
                )));
            }
        }
        if corpora.len() > 1 {
            // Any corpus's reranker may lead a search and read the merged top of every
            // corpus, so every corpus decides with its own policy what each may read.
            let rerankers: Vec<_> = corpora.iter().map(Corpus::reranker).collect();
            for corpus in &mut corpora {
                corpus.join_federation(rerankers.clone())?;
            }
        }
        Ok(Self {
            corpora,
            cache: Mutex::new(Cache::default()),
        })
    }

    /// Whether ids carry namespaces (several corpora).
    pub fn federated(&self) -> bool {
        self.corpora.len() > 1
    }

    /// The corpora, in the order given.
    pub fn corpora(&self) -> &[Corpus] {
        &self.corpora
    }

    /// Ships what every corpus's audit sinks still hold and stops them; called when the
    /// server stops (the local audit logs are already complete).
    pub fn shutdown(&self) {
        for corpus in &self.corpora {
            corpus.close();
        }
    }

    /// What is served, for logs: `namespace@snapshot` per corpus.
    pub fn describe(&self) -> String {
        self.corpora
            .iter()
            .map(|c| {
                let engine = c.engine();
                let snapshot = engine.pack().snapshot();
                format!("{}@{}", c.namespace(), snapshot.get(..12).unwrap_or(snapshot))
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn scope(&self, identity: &Identity) -> Result<Vec<Scoped>, ToolError> {
        self.corpora
            .iter()
            .map(|corpus| {
                let served = corpus.served();
                let (view, classes) = served.view(identity)?;
                Ok(Scoped {
                    engine: Arc::clone(&served.engine),
                    served,
                    view,
                    classes,
                })
            })
            .collect()
    }

    fn qualify(&self, index: usize, id: &str) -> String {
        if self.federated() {
            format!("{}:{id}", self.corpora[index].namespace())
        } else {
            id.to_owned()
        }
    }

    /// The corpus a namespace names, if the caller can see anything of it. A hidden
    /// corpus is reported like one that does not exist.
    fn namespace_index(&self, scoped: &[Scoped], namespace: &str) -> Option<usize> {
        self.corpora
            .iter()
            .position(|c| c.namespace() == namespace)
            .filter(|&i| scoped[i].visible())
    }

    /// Finds the corpus of an input id: `ns:id` names its corpus; a bare id must be an
    /// atom the caller can see in exactly one corpus.
    fn locate<'a>(&self, scoped: &[Scoped], id: &'a str) -> Result<Located<'a>, ToolError> {
        if !self.federated() {
            return Ok(Located::Found(0, id));
        }
        // A hidden corpus's namespace is not matched, so `hidden:x` is looked up like any
        // other id, exactly as if no corpus had that name.
        if let Some((namespace, rest)) = id.split_once(':')
            && let Some(index) = self.namespace_index(scoped, namespace)
        {
            return Ok(Located::Found(index, rest));
        }
        let mut found = Vec::new();
        for (index, scope) in scoped.iter().enumerate() {
            if let Some(uid) = scope.engine.pack().resolve_id(id)?
                && scope.view.contains(uid)
            {
                found.push(index);
            }
        }
        match found.as_slice() {
            [] => Ok(Located::Missing),
            [index] => Ok(Located::Found(*index, id)),
            many => Err(ToolError::Invalid(format!(
                "`{id}` exists in several namespaces ({}); qualify it as namespace:id",
                many.iter()
                    .map(|&i| self.corpora[i].namespace())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    /// The snapshot fields of an answer: one corpus's snapshot, or on a federated server
    /// an id over the snapshots of the corpora the caller can read, and those snapshots.
    fn snapshots(&self, scoped: &[Scoped]) -> (String, BTreeMap<String, String>) {
        if !self.federated() {
            return (scoped[0].engine.pack().snapshot().to_owned(), BTreeMap::new());
        }
        let map: BTreeMap<String, String> = self
            .corpora
            .iter()
            .zip(scoped)
            .filter(|(_, scope)| scope.visible())
            .map(|(corpus, scope)| {
                (
                    corpus.namespace().to_owned(),
                    scope.engine.pack().snapshot().to_owned(),
                )
            })
            .collect();
        let mut hasher = blake3::Hasher::new();
        for (namespace, snapshot) in &map {
            hasher.update(namespace.as_bytes());
            hasher.update(b"@");
            hasher.update(snapshot.as_bytes());
            hasher.update(b"\n");
        }
        (format!("fed-{}", &hasher.finalize().to_hex()[..16]), map)
    }

    fn audit_all(
        &self,
        identity: &Identity,
        tool: &str,
        scoped: &[Scoped],
        facts: &[AuditFacts],
    ) -> Result<(), ToolError> {
        for (index, (scope, facts)) in scoped.iter().zip(facts).enumerate() {
            self.audit_one(
                index,
                scope,
                identity,
                tool,
                scope.engine.pack().snapshot(),
                facts,
            )?;
        }
        Ok(())
    }

    /// Appends one corpus's audit record. On a federated server, a corpus the caller
    /// cannot see never fails the call: its failure is logged for the operator, and the
    /// answer is the same as if the corpus did not exist.
    fn audit_one(
        &self,
        index: usize,
        scope: &Scoped,
        identity: &Identity,
        tool: &str,
        snapshot: &str,
        facts: &AuditFacts,
    ) -> Result<(), ToolError> {
        let corpus = &self.corpora[index];
        match corpus.audit(identity, tool, snapshot, facts) {
            Err(ToolError::Internal(error)) if self.federated() && !scope.visible() => {
                crate::server::log_internal(&format!(
                    "audit of `{}` (not visible to the caller) failed: {error}",
                    corpus.namespace()
                ));
                Ok(())
            }
            other => other,
        }
    }

    fn parse<T: for<'de> Deserialize<'de>>(args: serde_json::Value) -> Result<T, ToolError> {
        serde_json::from_value(args).map_err(|e| ToolError::Invalid(format!("invalid arguments: {e}")))
    }

    /// Answers a read tool from the cache or by running it, and audits every corpus.
    fn cached(
        &self,
        identity: &Identity,
        tool: &'static str,
        args: &serde_json::Value,
        run: impl FnOnce(&[Scoped]) -> Result<Answer, ToolError>,
    ) -> Result<Answer, ToolError> {
        let scoped = self.scope(identity)?;
        let key: CacheKey = (
            scoped
                .iter()
                .map(|s| (s.served.key(), s.classes.clone()))
                .collect(),
            tool,
            args.to_string(),
        );
        if let Ok(cache) = self.cache.lock()
            && let Some((text, facts)) = cache.answers.get(&key)
        {
            let (text, facts) = (text.clone(), facts.clone());
            drop(cache);
            self.audit_all(identity, tool, &scoped, &facts)?;
            return Ok((text, facts));
        }
        let (text, facts) = run(&scoped)?;
        self.audit_all(identity, tool, &scoped, &facts)?;
        // An answer missing a failed tier is not kept: the next call tries again.
        if !facts.iter().any(|f| f.degraded)
            && let Ok(mut cache) = self.cache.lock()
        {
            let size = key.2.len() + text.len();
            if cache.answers.len() >= CACHE_CAPACITY || cache.bytes + size > CACHE_BYTES {
                cache.answers.clear();
                cache.bytes = 0;
            }
            cache.bytes += size;
            cache.answers.insert(key, (text.clone(), facts.clone()));
        }
        Ok((text, facts))
    }

    /// Runs a tool for `identity` and returns its JSON answer.
    pub fn call(
        &self,
        identity: &Identity,
        tool: &str,
        args: serde_json::Value,
    ) -> Result<String, ToolError> {
        let args = if args.is_null() {
            serde_json::json!({})
        } else {
            args
        };
        match tool {
            "kiwi_map" => {
                let req: MapRequest = Self::parse(args.clone())?;
                Ok(self.cached(identity, "kiwi_map", &args, |s| self.map(s, &req))?.0)
            }
            "kiwi_search" => {
                let req: SearchRequest = Self::parse(args.clone())?;
                let (text, facts) = self.cached(identity, "kiwi_search", &args, |s| self.search(s, &req))?;
                // A gap: no exact, term or lexical match (vector neighbours always
                // exist, so they alone do not count as an answer).
                let answered = serde_json::from_str::<SearchResponse>(&text)
                    .map(|r| {
                        r.results.iter().any(|item| {
                            item.why.iter().any(|w| {
                                matches!(w.as_str(), "exact" | "term" | "bm25" | "scoped" | "filter")
                            })
                        })
                    })
                    .unwrap_or_else(|_| facts.iter().any(|f| !f.atoms.is_empty()));
                if !answered && !req.queries.is_empty() {
                    self.record_gap(identity, &req)?;
                }
                Ok(text)
            }
            "kiwi_read" => {
                let req: ReadRequest = Self::parse(args.clone())?;
                Ok(self
                    .cached(identity, "kiwi_read", &args, |s| self.read(s, &req))?
                    .0)
            }
            "kiwi_trace" => {
                let req: TraceRequest = Self::parse(args.clone())?;
                Ok(self
                    .cached(identity, "kiwi_trace", &args, |s| self.trace(s, &req))?
                    .0)
            }
            "kiwi_feedback" => self.feedback_tool(identity, Self::parse(args)?),
            "kiwi_propose" => self.propose_tool(identity, Self::parse(args)?),
            other => Err(ToolError::Invalid(format!("unknown tool `{other}`"))),
        }
    }

    fn to_json<R: Serialize>(response: &R) -> Result<String, ToolError> {
        serde_json::to_string(response).map_err(|e| ToolError::Internal(e.to_string()))
    }

    fn search(&self, scoped: &[Scoped], req: &SearchRequest) -> Result<Answer, ToolError> {
        if !self.federated() {
            let (response, facts) = scoped[0].engine.search(&scoped[0].view, req)?;
            return Ok((Self::to_json(&response)?, vec![facts]));
        }
        let lead = lead(scoped);
        let settings = lead.map_or_else(EngineConfig::default, |index| *scoped[index].engine.config());
        let budget = settings.budget(req.budget);
        let limit = settings.limit(req);
        // The lead corpus's settings apply to every corpus: each ranks as deep as the
        // lead would.
        let req = &SearchRequest {
            limit: Some(limit as u32),
            ..req.clone()
        };
        let mut facts = vec![AuditFacts::default(); scoped.len()];
        let mut merged: Vec<(usize, RankedAtom)> = Vec::new();
        let mut redacted: Option<u32> = None;
        let mut warnings: BTreeSet<Warning> = BTreeSet::new();
        for (index, scope) in scoped.iter().enumerate() {
            // A corpus the caller can see nothing of is not consulted at all: no count,
            // no plugin call, no warning, no time spent that depends on it.
            if !scope.visible() {
                continue;
            }
            let ranking = scope.engine.rank(&scope.view, req)?;
            facts[index].blocked = ranking.blocked;
            facts[index].degraded = !ranking.degraded.is_empty();
            warnings.extend(
                ranking
                    .degraded
                    .into_iter()
                    .map(|tier| Warning::Degraded { tier }),
            );
            if scope.engine.config().hidden == kiwi_core::HiddenMode::Stub {
                *redacted.get_or_insert(0) += ranking.blocked;
            }
            merged.extend(ranking.items.into_iter().map(|item| (index, item)));
        }
        // By fused score; ties by corpus order, then uid.
        merged.sort_by(|a, b| {
            b.1.score
                .total_cmp(&a.1.score)
                .then(a.0.cmp(&b.0))
                .then(a.1.uid.cmp(&b.1.uid))
        });
        let total = merged.len();
        let reranker = lead.and_then(|index| scoped[index].served.federation_reranker(index));
        let head = reranker.map_or(limit, |r| limit.max(r.top_k()));
        merged.truncate(head);
        if let (Some(attached), Some(lead)) = (reranker, lead) {
            // The lead corpus's reranker reads the merged top; each atom only when its
            // own corpus's policy lets that reranker read it. A failure keeps the order.
            let outcome = kiwi_core::rerank::rerank(
                attached.reranker().as_ref(),
                &kiwi_core::rerank::query_text(&req.queries),
                &mut merged,
                attached.top_k(),
                |(index, item)| {
                    let scope = &scoped[*index];
                    let class = scope.engine.class_of(item.uid).unwrap_or(u32::MAX);
                    let allowed = scope
                        .served
                        .federation_reranker(lead)
                        .is_some_and(|here| here.allows(class));
                    allowed.then(|| scope.engine.passage(item.uid)).transpose()
                },
                |(_, item)| kiwi_core::rerank::mark_reranked(item),
            );
            if let Err(error) = outcome {
                tracing::warn!(%error, "rerank failed; the fused order stands");
                warnings.insert(Warning::Degraded {
                    tier: kiwi_core::rerank::RERANK_TIER.to_owned(),
                });
                // Every consulted corpus's record says the answer was degraded, so it
                // is not cached.
                for (fact, scope) in facts.iter_mut().zip(scoped) {
                    fact.degraded |= scope.visible();
                }
            }
        }
        merged.truncate(limit);
        let mut costs = Vec::with_capacity(merged.len());
        for (index, item) in &merged {
            let extra = namespace_tokens(self.corpora[*index].namespace());
            costs.push(scoped[*index].engine.costs(item.uid, extra)?);
        }
        let fit = fit_budget(&costs, budget);
        let mut results = Vec::with_capacity(fit.included);
        for ((index, item), &summary) in merged.iter().zip(&fit.summaries) {
            let mut local = BTreeSet::new();
            let mut found = scoped[*index].engine.item(item, summary, &mut local)?;
            found.id = self.qualify(*index, &found.id);
            warnings.extend(local.into_iter().map(|w| self.qualify_warning(*index, w)));
            facts[*index].atoms.push(item.uid);
            results.push(found);
        }
        let (snapshot, snapshots) = self.snapshots(scoped);
        let response = SearchResponse {
            snapshot,
            snapshots,
            results,
            omitted: (total - fit.included) as u32,
            budget: BudgetUse {
                limit: budget,
                used: fit.used,
            },
            warnings: warnings.into_iter().collect(),
            redacted,
        };
        Ok((Self::to_json(&response)?, facts))
    }

    fn qualify_warning(&self, index: usize, warning: Warning) -> Warning {
        match warning {
            Warning::ReviewOverdue { id, review_by } => Warning::ReviewOverdue {
                id: self.qualify(index, &id),
                review_by,
            },
            Warning::Redirected { id, to } => Warning::Redirected {
                id: self.qualify(index, &id),
                to: self.qualify(index, &to),
            },
            Warning::Deprecated { id } => Warning::Deprecated {
                id: self.qualify(index, &id),
            },
            Warning::Unapproved { id, status } => Warning::Unapproved {
                id: self.qualify(index, &id),
                status,
            },
            degraded @ Warning::Degraded { .. } => degraded,
        }
    }

    fn qualify_node(&self, index: usize, mut node: MapNode) -> MapNode {
        node.id = self.qualify(index, &node.id);
        node.children = node
            .children
            .into_iter()
            .map(|child| self.qualify_node(index, child))
            .collect();
        node
    }

    fn map(&self, scoped: &[Scoped], req: &MapRequest) -> Result<Answer, ToolError> {
        if !self.federated() {
            let (response, facts) = scoped[0].engine.map(&scoped[0].view, req)?;
            return Ok((Self::to_json(&response)?, vec![facts]));
        }
        let budget = lead(scoped)
            .map_or_else(EngineConfig::default, |index| *scoped[index].engine.config())
            .budget(req.budget);
        let scope = req.scope.as_deref().map(str::trim).unwrap_or("");
        // `ns:rest` (or `ns:` / `ns`) maps one corpus; anything else maps every corpus
        // the caller can read, each with an equal share of the budget.
        // Only corpora the caller can see are matched by name: a hidden one's name is
        // read like any other scope, exactly as if no corpus had it.
        let named = match scope.split_once(':') {
            Some((namespace, rest)) => self.namespace_index(scoped, namespace).map(|index| (index, rest)),
            None => self.namespace_index(scoped, scope).map(|index| (index, "")),
        };
        let targets: Vec<(usize, &str)> = match named {
            Some(target) => vec![target],
            None => (0..scoped.len())
                .filter(|&i| scoped[i].visible())
                .map(|i| (i, scope))
                .collect(),
        };
        let mut facts = vec![AuditFacts::default(); scoped.len()];
        let mut nodes = Vec::new();
        let mut warnings = BTreeSet::new();
        let (mut omitted, mut used) = (0u32, 0u32);
        let mut redacted: Option<u32> = None;
        let share = if named.is_some() {
            budget
        } else {
            (budget / targets.len().max(1) as u32).saturating_sub(NAMESPACE_NODE_TOKENS)
        };
        for (index, local_scope) in &targets {
            let request = MapRequest {
                scope: (!local_scope.is_empty()).then(|| (*local_scope).to_owned()),
                depth: req.depth,
                budget: Some(share),
            };
            let (response, corpus_facts) =
                scoped[*index]
                    .engine
                    .map_within(&scoped[*index].view, &request, share)?;
            facts[*index] = corpus_facts;
            omitted += response.omitted;
            used += response.budget.used;
            if let Some(count) = response.redacted {
                *redacted.get_or_insert(0) += count;
            }
            warnings.extend(
                response
                    .warnings
                    .into_iter()
                    .map(|w| self.qualify_warning(*index, w)),
            );
            let children: Vec<MapNode> = response
                .nodes
                .into_iter()
                .map(|node| self.qualify_node(*index, node))
                .collect();
            if named.is_some() {
                nodes = children;
            } else if !children.is_empty() {
                used += NAMESPACE_NODE_TOKENS;
                nodes.push(MapNode {
                    id: format!("{}:", self.corpora[*index].namespace()),
                    title: self.corpora[*index].namespace().to_owned(),
                    doc: Some("namespace".into()),
                    summary: None,
                    children,
                });
            }
        }
        let (snapshot, snapshots) = self.snapshots(scoped);
        let response = MapResponse {
            snapshot,
            snapshots,
            scope: scope.to_owned(),
            nodes,
            omitted,
            budget: BudgetUse { limit: budget, used },
            warnings: warnings.into_iter().collect(),
            redacted,
        };
        Ok((Self::to_json(&response)?, facts))
    }

    fn read(&self, scoped: &[Scoped], req: &ReadRequest) -> Result<Answer, ToolError> {
        if !self.federated() {
            let (response, facts) = scoped[0].engine.read(&scoped[0].view, req)?;
            return Ok((Self::to_json(&response)?, vec![facts]));
        }
        if req.ids.is_empty() {
            return Err(ToolError::Invalid("ids must not be empty".into()));
        }
        if req.ids.len() > 20 {
            return Err(ToolError::Invalid("at most 20 ids per read".into()));
        }
        let mut facts = vec![AuditFacts::default(); scoped.len()];
        let mut atoms = Vec::new();
        let mut seen = BTreeSet::new();
        let mut not_found = Vec::new();
        let mut hidden = Vec::new();
        let mut warnings = BTreeSet::new();
        let mut redacted: Option<u32> = None;
        for asked in &req.ids {
            let (index, local) = match self.locate(scoped, asked)? {
                Located::Found(index, local) => (index, local),
                Located::Missing => {
                    not_found.push(asked.clone());
                    continue;
                }
            };
            let scope = &scoped[index];
            let request = ReadRequest {
                ids: vec![local.to_owned()],
                level: req.level,
                context: req.context,
            };
            let (response, corpus_facts) = scope.engine.read(&scope.view, &request)?;
            facts[index].blocked += corpus_facts.blocked;
            if let Some(count) = response.redacted {
                *redacted.get_or_insert(0) += count;
            }
            not_found.extend(response.not_found.iter().map(|_| asked.clone()));
            hidden.extend(response.hidden.into_iter().map(|item| HiddenItem {
                id: asked.clone(),
                ..item
            }));
            warnings.extend(
                response
                    .warnings
                    .into_iter()
                    .map(|w| self.qualify_warning(index, w)),
            );
            for (mut item, uid) in response.atoms.into_iter().zip(corpus_facts.atoms) {
                item.id = self.qualify(index, &item.id);
                if !seen.insert(item.id.clone()) {
                    continue;
                }
                item.cite = format!("kiwi://atom/{}@{}", item.id, scope.engine.pack().snapshot());
                item.links = item
                    .links
                    .into_iter()
                    .map(|link| Link {
                        rel: link.rel,
                        id: self.qualify(index, &link.id),
                    })
                    .collect();
                item.context = item
                    .context
                    .into_iter()
                    .map(|c| ContextItem {
                        id: self.qualify(index, &c.id),
                        ..c
                    })
                    .collect();
                facts[index].atoms.push(uid);
                atoms.push(item);
            }
        }
        let (snapshot, snapshots) = self.snapshots(scoped);
        let response = ReadResponse {
            snapshot,
            snapshots,
            atoms,
            not_found,
            hidden,
            warnings: warnings.into_iter().collect(),
            redacted,
        };
        Ok((Self::to_json(&response)?, facts))
    }

    fn trace(&self, scoped: &[Scoped], req: &TraceRequest) -> Result<Answer, ToolError> {
        if !self.federated() {
            let (response, facts) = scoped[0].engine.trace(&scoped[0].view, req)?;
            return Ok((Self::to_json(&response)?, vec![facts]));
        }
        let missing = || ToolError::Invalid(format!("no atom `{}`", req.id));
        let Located::Found(index, local) = self.locate(scoped, &req.id)? else {
            return Err(missing());
        };
        let request = TraceRequest {
            id: local.to_owned(),
            ..req.clone()
        };
        let scope = &scoped[index];
        let (response, corpus_facts) = scope.engine.trace(&scope.view, &request).map_err(|e| match e {
            kiwi_core::KiwiError::InvalidInput(message) if message.starts_with("no atom") => missing(),
            other => other.into(),
        })?;
        let mut facts = vec![AuditFacts::default(); scoped.len()];
        facts[index] = corpus_facts;
        let (snapshot, snapshots) = self.snapshots(scoped);
        let response = TraceResponse {
            snapshot,
            snapshots,
            root: self.qualify(index, &response.root),
            nodes: response
                .nodes
                .into_iter()
                .map(|mut node| {
                    node.id = self.qualify(index, &node.id);
                    node
                })
                .collect(),
            edges: response
                .edges
                .into_iter()
                .map(|mut edge| {
                    edge.from = self.qualify(index, &edge.from);
                    edge.to = self.qualify(index, &edge.to);
                    edge
                })
                .collect(),
            warnings: response
                .warnings
                .into_iter()
                .map(|w| self.qualify_warning(index, w))
                .collect(),
        };
        Ok((Self::to_json(&response)?, facts))
    }

    fn record_gap(&self, identity: &Identity, req: &SearchRequest) -> Result<(), ToolError> {
        let scoped = self.scope(identity)?;
        let query = text::truncate_chars(&req.queries.join(" | "), MAX_GAP_QUERY);
        for (corpus, scope) in self.corpora.iter().zip(&scoped) {
            let searched = req.filters.namespace.is_empty()
                || req.filters.namespace.iter().any(|ns| ns == corpus.namespace());
            // A single corpus records every gap, as before federation; a federated
            // server records one in each corpus the caller searched and can read.
            if self.federated() && !(scope.visible() && searched) {
                continue;
            }
            corpus.record_feedback(FeedbackRecord {
                ts: String::new(),
                principal: identity.principal.clone(),
                agent: identity.agent.clone(),
                channel: identity.channel.clone(),
                snapshot: scope.engine.pack().snapshot().to_owned(),
                query: query.clone(),
                ids: Vec::new(),
                verdict: Verdict::NoResults,
                note: None,
                view: Some(readable_chains(scope)),
            })?;
        }
        Ok(())
    }

    fn feedback_tool(&self, identity: &Identity, req: FeedbackRequest) -> Result<String, ToolError> {
        if req.verdict == Verdict::NoResults {
            return Err(ToolError::Invalid(
                "verdict must be useful, wrong or missing".into(),
            ));
        }
        if req.query.trim().is_empty() || req.query.len() > 2_000 {
            return Err(ToolError::Invalid("query must be 1 to 2000 characters".into()));
        }
        let scoped = self.scope(identity)?;
        // Ids the caller cannot see are dropped silently: the answer is the same whether
        // they exist or not.
        let mut by_corpus: BTreeMap<usize, (Vec<String>, AuditFacts)> = BTreeMap::new();
        for asked in req.ids.iter().take(50) {
            let (index, local) = match self.locate(&scoped, asked) {
                Ok(Located::Found(index, local)) => (index, local),
                Ok(Located::Missing) | Err(ToolError::Invalid(_)) => continue,
                Err(other) => return Err(other),
            };
            let scope = &scoped[index];
            let entry = by_corpus.entry(index).or_default();
            match scope.engine.pack().resolve_id(local)? {
                Some(uid) if scope.view.contains(uid) => {
                    let id = scope
                        .engine
                        .pack()
                        .atom(uid)
                        .map(|a| a.id.as_str().to_owned())
                        .unwrap_or_default();
                    entry.0.push(id);
                    entry.1.atoms.push(uid);
                }
                Some(_) => entry.1.blocked += 1,
                None => {}
            }
        }
        if by_corpus.is_empty() {
            let index = match req.namespace.as_deref() {
                Some(namespace) if self.federated() => self
                    .namespace_index(&scoped, namespace)
                    .ok_or_else(|| ToolError::Invalid(format!("no namespace `{namespace}`")))?,
                _ => (0..scoped.len()).find(|&i| scoped[i].visible()).unwrap_or(0),
            };
            by_corpus.insert(index, (Vec::new(), AuditFacts::default()));
        }
        for (index, (ids, facts)) in &by_corpus {
            let corpus = &self.corpora[*index];
            let snapshot = scoped[*index].engine.pack().snapshot().to_owned();
            corpus.record_feedback(FeedbackRecord {
                ts: String::new(),
                principal: identity.principal.clone(),
                agent: identity.agent.clone(),
                channel: identity.channel.clone(),
                snapshot: snapshot.clone(),
                query: req.query.clone(),
                ids: ids.clone(),
                verdict: req.verdict,
                note: req.note.as_ref().map(|n| text::truncate_chars(n, 2_000)),
                view: Some(readable_chains(&scoped[*index])),
            })?;
            self.audit_one(
                *index,
                &scoped[*index],
                identity,
                "kiwi_feedback",
                &snapshot,
                facts,
            )?;
        }
        let (snapshot, _) = self.snapshots(&scoped);
        Self::to_json(&FeedbackResponse {
            snapshot: &snapshot,
            recorded: true,
            gap: req.verdict == Verdict::Missing,
        })
    }

    fn propose_tool(&self, identity: &Identity, req: ProposeRequest) -> Result<String, ToolError> {
        if req.changes.is_empty() || req.changes.len() > 20 {
            return Err(ToolError::Invalid("a proposal has 1 to 20 changes".into()));
        }
        if req.rationale.trim().is_empty() {
            return Err(ToolError::Invalid("a rationale is required".into()));
        }
        if req
            .changes
            .iter()
            .any(|change| change.content.as_ref().is_some_and(|c| c.len() > 512 * 1024))
        {
            return Err(ToolError::Invalid("a change is limited to 512 KiB".into()));
        }
        let (index, changes) = if self.federated() {
            let namespace = req.namespace.as_deref().ok_or_else(|| {
                ToolError::Invalid("this server serves several corpora: set namespace".into())
            })?;
            let scoped = self.scope(identity)?;
            let index = self
                .namespace_index(&scoped, namespace)
                .ok_or_else(|| ToolError::Invalid(format!("no namespace `{namespace}`")))?;
            let prefix = format!("{namespace}:");
            let changes: Vec<ProposeChange> = req
                .changes
                .iter()
                .map(|change| ProposeChange {
                    section: change
                        .section
                        .as_ref()
                        .map(|s| s.strip_prefix(&prefix).unwrap_or(s).to_owned()),
                    ..change.clone()
                })
                .collect();
            (index, changes)
        } else {
            (0, req.changes.clone())
        };
        let corpus = &self.corpora[index];
        let (proposal, snapshot) =
            corpus.propose(identity, &changes, req.title.as_deref(), &req.rationale)?;
        Self::to_json(&ProposeResponse {
            snapshot: &snapshot,
            namespace: self.federated().then_some(corpus.namespace()),
            status: "opened",
            id: &proposal.id,
            branch: &proposal.branch,
            link: &proposal.link,
            merge: &proposal.merge,
            lint: &proposal.lint,
        })
    }

    /// Reads `kiwi://atom/{id}@{rev}`: the L2 of one atom at a pinned snapshot.
    ///
    /// The atom must be visible to the caller now: in the current pack, under the
    /// current policy, with its current access chain. Its text at the pinned snapshot is
    /// returned only if the chain it had then is visible to the caller too, under the
    /// current policy, and if none of its paragraphs is now held only by atoms hidden
    /// from the caller; so a section narrowed or removed since is not readable through an
    /// old snapshot, the stricter of the two chains always applies, and text moved under
    /// a narrower declaration is not reopened. An id hidden either way answers like one
    /// that does not exist. The snapshot must be of the served source's lineage (see
    /// [`Corpus::engine_at`]).
    pub fn read_resource(&self, identity: &Identity, uri: &str) -> Result<String, ToolError> {
        let rest = uri
            .strip_prefix("kiwi://atom/")
            .ok_or_else(|| ToolError::Invalid(format!("unknown resource `{uri}`")))?;
        let (id, rev) = rest
            .rsplit_once('@')
            .ok_or_else(|| ToolError::Invalid("expected kiwi://atom/{id}@{rev}".into()))?;
        let missing = || ToolError::Invalid(format!("no atom `{id}` at {rev}"));
        let scoped = self.scope(identity)?;
        let (index, local) = match self.locate(&scoped, id)? {
            Located::Found(index, local) => (index, local),
            Located::Missing => return Err(missing()),
        };
        let scope = &scoped[index];
        // The atom as the caller may read it now: in the current pack, in their view.
        let current = scope.engine.pack();
        let now = current
            .resolve_id(local)?
            .filter(|uid| scope.view.contains(*uid))
            .ok_or_else(missing)?;
        let canonical = current
            .atom(now)
            .map(|atom| atom.id.as_str().to_owned())
            .ok_or_else(missing)?;
        let corpus = &self.corpora[index];
        let engine = corpus
            .engine_at(rev)?
            .ok_or_else(|| ToolError::Invalid(format!("snapshot `{rev}` is not available")))?;
        let view = if Arc::ptr_eq(&engine, &scope.engine) {
            scope.view.clone()
        } else {
            scope.served.view_of(&engine, identity)?.0
        };
        // The same atom at the pinned snapshot, under the access it had then (judged by
        // the current policy), whose text holds nothing hidden from the caller now.
        let pinned = engine.pack();
        let then = pinned
            .resolve_id(&canonical)?
            .filter(|uid| view.contains(*uid))
            .ok_or_else(missing)?;
        if scope
            .served
            .hides_any_paragraph(&pinned.body(then)?, &scope.classes)?
        {
            return Err(missing());
        }
        let request = ReadRequest {
            ids: vec![canonical],
            level: Some(kiwi_core::api::Level::L2),
            context: None,
        };
        let (response, facts) = engine.read(&view, &request)?;
        self.audit_one(
            index,
            scope,
            identity,
            "resource",
            engine.pack().snapshot(),
            &facts,
        )?;
        let atom = response.atoms.into_iter().next().ok_or_else(missing)?;
        Ok(atom.text)
    }
}

/// The longest query a gap record keeps.
const MAX_GAP_QUERY: usize = 2_000;

/// Tokens a `namespace:` prefix adds to an id.
fn namespace_tokens(namespace: &str) -> u32 {
    kiwi_compiler::compile::count_tokens(&format!("{namespace}:"))
}
