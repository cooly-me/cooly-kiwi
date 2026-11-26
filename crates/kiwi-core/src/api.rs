//! Requests and responses of the four read tools (`map`, `search`, `read`, `trace`).
//!
//! Field order is fixed and every list is sorted deterministically, so one snapshot and
//! one request always serialise to the same bytes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A zoom level of an atom.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum Level {
    /// Title with breadcrumb (about 10 tokens).
    L0,
    /// Short abstract (about 40–60 tokens).
    L1,
    /// Full text.
    #[default]
    L2,
}

/// Structural filters, applied with bitmap intersections before any ranking.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default, deny_unknown_fields)]
pub struct Filters {
    /// Document types to keep (`vision`, `prd`, `rfc`, `adr`, `runbook`, `standard`,
    /// `glossary`, or a corpus's own type); any of them.
    #[serde(rename = "type")]
    pub doc_type: Vec<String>,
    /// Statuses to keep (`approved`, `draft`…); any of them.
    pub status: Vec<String>,
    /// Owners to keep (`@team-payments`); any of them.
    pub owner: Vec<String>,
    /// Tags to keep; any of them.
    pub tags: Vec<String>,
    /// Keep only atoms whose document path starts with this prefix.
    pub path: Option<String>,
    /// Corpora to search, by namespace, on a server that federates several; any of
    /// them (default: every corpus).
    pub namespace: Vec<String>,
}

impl Filters {
    /// True when no filter is set.
    pub fn is_empty(&self) -> bool {
        self.doc_type.is_empty()
            && self.status.is_empty()
            && self.owner.is_empty()
            && self.tags.is_empty()
            && self.path.is_none()
            && self.namespace.is_empty()
    }
}

/// `kiwi_map` input.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default, deny_unknown_fields)]
pub struct MapRequest {
    /// An atom id or a path prefix (`adr/`, `product`); empty for the whole corpus.
    pub scope: Option<String>,
    /// Tree depth below the scope: 1 lists documents, 2 (the default) adds their top
    /// sections.
    pub depth: Option<u8>,
    /// Token budget of the answer (its default and maximum are in the tool's
    /// description). Raise it to see more of a large scope; `omitted` counts what did
    /// not fit.
    pub budget: Option<u32>,
}

/// `kiwi_search` input.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default, deny_unknown_fields)]
pub struct SearchRequest {
    /// Queries written by the agent, several in one call: keywords, synonyms, ids or a
    /// one-line hypothetical answer. Results of all queries are fused, and every query
    /// that matches anything has at least one result.
    pub queries: Vec<String>,
    /// Structural filters.
    pub filters: Filters,
    /// Token budget for each query; the answer gets this times the number of queries
    /// (the default and the maximum are in the tool's description). Raise it for more
    /// abstracts and results.
    pub budget: Option<u32>,
    /// Results for each query before the budget cut (the default and the maximum are
    /// in the tool's description).
    pub limit: Option<u32>,
}

/// What to add around each atom in `kiwi_read`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum ReadContext {
    /// Only the atom.
    #[default]
    None,
    /// The parent section's L0.
    Parent,
    /// The L0 of the sibling sections.
    Siblings,
    /// The L0 of the child sections.
    Children,
}

/// `kiwi_read` input.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default, deny_unknown_fields)]
pub struct ReadRequest {
    /// Atom ids (`path#anchor`), document paths or document ids (`ADR-005`). At L2 each
    /// is read whole: a document with all its sections, a section with all its
    /// subsections.
    pub ids: Vec<String>,
    /// Zoom level (default L2, the full text).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<Level>,
    /// Neighbouring sections to list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<ReadContext>,
    /// Token budget of the text (the default and the maximum are in the tool's
    /// description). What does not fit is left out section by section, and `more` says
    /// how to read it; raise the budget to get more in one answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<u32>,
    /// Continue the first id's text after this section: copy it from `more.next`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

/// Direction of a graph walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Follow edges from the atom.
    Out,
    /// Follow edges into the atom.
    In,
    /// Both.
    #[default]
    Both,
}

/// `kiwi_trace` input.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default, deny_unknown_fields)]
pub struct TraceRequest {
    /// Start atom id, document path or document id.
    pub id: String,
    /// Edge kind to follow (`implements`, `supersedes`, `decided_by`, `depends_on`,
    /// `amends`, `amended_by`, `mentions`, `code`); `typed` (default) follows every kind except `mentions`;
    /// `any` follows all.
    pub relation: Option<String>,
    /// Direction (default both).
    pub direction: Option<Direction>,
    /// Maximum number of hops (default 3).
    pub depth: Option<u8>,
}

/// A notice attached to an answer.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Warning {
    /// The atom's document is past its `review_by` date.
    ReviewOverdue {
        /// Atom id.
        id: String,
        /// The missed deadline.
        review_by: String,
    },
    /// A deprecated atom was replaced by the atom that supersedes it.
    Redirected {
        /// The deprecated atom.
        id: String,
        /// Its replacement.
        to: String,
    },
    /// The atom is deprecated and no visible replacement exists.
    Deprecated {
        /// Atom id.
        id: String,
    },
    /// The atom's document is still a draft or in review.
    Unapproved {
        /// Atom id.
        id: String,
        /// `draft` or `review`.
        status: String,
    },
    /// A retrieval tier failed (such as an embedding service that is down), so the
    /// results come from the other tiers. Asking again later may rank differently.
    Degraded {
        /// The tier: `bm25`, `vec` or `rerank`.
        tier: String,
    },
}

/// Token use of an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetUse {
    /// The budget asked for.
    pub limit: u32,
    /// Estimated tokens of the returned items.
    pub used: u32,
}

/// One node of a `kiwi_map` tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MapNode {
    /// Atom id.
    pub id: String,
    /// Heading text.
    pub title: String,
    /// Document type and status, on document roots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    /// L1 abstract, when the budget allowed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Child sections.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<MapNode>,
}

/// `kiwi_map` output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MapResponse {
    /// Commit the answer is pinned to (on a federated server, an id of the corpora's
    /// snapshots together).
    pub snapshot: String,
    /// On a federated server: the snapshot of each corpus the caller can read, by
    /// namespace. Cite an atom of namespace `ns` as `kiwi://atom/{id}@{snapshots[ns]}`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub snapshots: BTreeMap<String, String>,
    /// The resolved scope.
    pub scope: String,
    /// Top-level nodes.
    pub nodes: Vec<MapNode>,
    /// Visible nodes left out for lack of budget or depth.
    pub omitted: u32,
    /// Token use.
    pub budget: BudgetUse,
    /// Notices.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Warning>,
    /// Hidden sections in scope (only with `hidden = "stub"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redacted: Option<u32>,
}

/// One `kiwi_search` result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchItem {
    /// Atom id.
    pub id: String,
    /// `L1` for high-ranked items, `L0` for the rest.
    pub level: Level,
    /// L0: title with breadcrumb.
    pub title: String,
    /// L1 abstract (only at level L1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Document status.
    pub status: String,
    /// Fused score (rounded to 4 decimals).
    pub score: f64,
    /// Why it matched: `exact`, `term`, `graph`, `filter`, or a retriever name.
    pub why: Vec<String>,
}

/// `kiwi_search` output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchResponse {
    /// Commit the answer is pinned to (on a federated server, an id of the corpora's
    /// snapshots together).
    pub snapshot: String,
    /// On a federated server: the snapshot of each corpus the caller can read, by
    /// namespace. Cite an atom of namespace `ns` as `kiwi://atom/{id}@{snapshots[ns]}`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub snapshots: BTreeMap<String, String>,
    /// Ranked results.
    pub results: Vec<SearchItem>,
    /// Ranked results left out by the budget.
    pub omitted: u32,
    /// Token use.
    pub budget: BudgetUse,
    /// Notices.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Warning>,
    /// Hidden exact matches (only with `hidden = "stub"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redacted: Option<u32>,
}

/// A typed link from or to an atom.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Link {
    /// Edge kind; incoming edges are prefixed with `~` (`~implements`).
    pub rel: String,
    /// The other end: an atom id or `code:<glob>`.
    pub id: String,
}

/// A neighbouring section listed by `kiwi_read`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextItem {
    /// `parent`, `sibling` or `child`.
    pub relation: String,
    /// Atom id.
    pub id: String,
    /// L0.
    pub title: String,
}

/// One atom returned by `kiwi_read`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadItem {
    /// Atom id.
    pub id: String,
    /// L0: title with breadcrumb.
    pub title: String,
    /// The level returned.
    pub level: Level,
    /// The text at that level.
    pub text: String,
    /// Tokens of `text`.
    pub tokens: u32,
    /// Document type.
    #[serde(rename = "type")]
    pub doc_type: String,
    /// Document status.
    pub status: String,
    /// Document owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Review deadline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_by: Option<String>,
    /// Stable citation `kiwi://atom/{id}@{snapshot}`.
    pub cite: String,
    /// Typed links to visible atoms.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
    /// Neighbouring sections.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context: Vec<ContextItem>,
    /// When `text` holds more than the atom's own section (its subsections, or the rest
    /// of a text continued with `after`): the sections it holds, in order. Each is cited
    /// as `kiwi://atom/{section}@{snapshot}`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<String>,
}

/// What a `kiwi_read` left out for its budget, and how to read it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadMore {
    /// Sections left out.
    pub sections: u32,
    /// Their tokens.
    pub tokens: u32,
    /// The highest sections left out, each read whole by its id: the rest of the text
    /// cut short, then the ids not reached.
    pub left_out: Vec<String>,
    /// The `kiwi_read` arguments that read on from where this answer stops.
    pub next: ReadRequest,
}

/// A hidden atom shown as a stub (only with `hidden = "stub"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HiddenItem {
    /// The id that was asked for.
    pub id: String,
    /// Who to ask for access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Explanation.
    pub note: String,
}

/// `kiwi_read` output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadResponse {
    /// Commit the answer is pinned to (on a federated server, an id of the corpora's
    /// snapshots together).
    pub snapshot: String,
    /// On a federated server: the snapshot of each corpus the caller can read, by
    /// namespace. Cite an atom of namespace `ns` as `kiwi://atom/{id}@{snapshots[ns]}`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub snapshots: BTreeMap<String, String>,
    /// The atoms, in request order.
    pub atoms: Vec<ReadItem>,
    /// Ids that do not exist (or that the caller may not see).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_found: Vec<String>,
    /// Stubs for hidden atoms (only with `hidden = "stub"`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden: Vec<HiddenItem>,
    /// Notices.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Warning>,
    /// Number of hidden atoms (only with `hidden = "stub"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redacted: Option<u32>,
    /// Token use of the text.
    pub budget: BudgetUse,
    /// Present when the budget cut the answer short: what was left out and how to read
    /// it. Absent, the answer is whole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub more: Option<ReadMore>,
}

/// A node of a trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceNode {
    /// Atom id, or `code:<glob>` for a code path.
    pub id: String,
    /// L0, or the glob.
    pub title: String,
    /// Document type (`code` for code paths).
    #[serde(rename = "type")]
    pub doc_type: String,
    /// Document status (empty for code paths).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    /// Hops from the start.
    pub hops: u8,
}

/// An edge of a trace.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TraceEdge {
    /// Source id.
    pub from: String,
    /// Edge kind.
    pub rel: String,
    /// Target id.
    pub to: String,
}

/// `kiwi_trace` output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceResponse {
    /// Commit the answer is pinned to (on a federated server, an id of the corpora's
    /// snapshots together).
    pub snapshot: String,
    /// On a federated server: the snapshot of each corpus the caller can read, by
    /// namespace. Cite an atom of namespace `ns` as `kiwi://atom/{id}@{snapshots[ns]}`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub snapshots: BTreeMap<String, String>,
    /// The resolved start atom.
    pub root: String,
    /// Nodes in walk order.
    pub nodes: Vec<TraceNode>,
    /// Edges between visible nodes.
    pub edges: Vec<TraceEdge>,
    /// Notices.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Warning>,
}

/// What the audit log needs to know about an answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditFacts {
    /// Uids returned, in answer order.
    pub atoms: Vec<u32>,
    /// Matches withheld by the policy gate.
    pub blocked: u32,
    /// A retrieval tier failed; the answer is not cached.
    pub degraded: bool,
}
