//! The plugin traits. First-party plugins are compiled in behind cargo features;
//! third-party plugins run as WebAssembly components behind the same traits.

use std::io::Write;

use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::pack::Pack;

/// What a plugin declares it may see and reach. The host enforces it: content and
/// labels are only passed when declared, and governance can keep labelled atoms away
/// from plugins that declare network access.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// The plugin reads atom text.
    pub content: bool,
    /// The plugin reads permission labels.
    pub labels: bool,
    /// Hosts the plugin may contact; empty means no network.
    pub network: Vec<String>,
}

/// One atom as an indexer or enricher sees it.
#[derive(Debug, Clone, Copy)]
pub struct IndexDoc<'a> {
    /// The atom uid (the key every segment is indexed by).
    pub uid: u32,
    /// The atom id.
    pub id: &'a str,
    /// The heading text.
    pub title: &'a str,
    /// The L0 breadcrumb line.
    pub breadcrumb: &'a str,
    /// The L1 abstract.
    pub summary: &'a str,
    /// The L2 full text.
    pub body: &'a str,
    /// BLAKE3 of the atom content.
    pub hash: &'a [u8; 32],
    /// The permission class.
    pub class: u32,
    /// The policy keeps this atom from the plugin (it declares network access and the
    /// policy does not let it read this atom over the network): index nothing from it.
    /// The text fields are empty when this is set.
    pub withheld: bool,
}

/// A build-time plugin that writes one segment into the pack.
pub trait Indexer: Send + Sync {
    /// The segment name, unique per pack.
    fn name(&self) -> &str;
    /// The segment format version.
    fn version(&self) -> u32;
    /// What the indexer reads.
    fn capabilities(&self) -> Capabilities;
    /// Builds the segment. `previous` is the same segment of the previous pack (with
    /// its version), so an indexer can reuse work keyed by atom hash.
    fn build(&self, docs: &[IndexDoc<'_>], previous: Option<(u32, &[u8])>) -> Result<Vec<u8>>;
}

/// The atoms a query may touch, decided by the policy gate before any retrieval.
#[derive(Debug, Clone, Copy)]
pub struct Scope<'a> {
    /// Atoms the principal may see. Statistics (such as BM25 document frequencies)
    /// must only ever be computed over this set.
    pub visible: &'a RoaringBitmap,
    /// The subset that passes the structural filters; hits must come from it.
    pub candidates: &'a RoaringBitmap,
}

/// A scored candidate from a retriever.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    /// Atom uid.
    pub uid: u32,
    /// Retriever-specific score; only the rank order is used for fusion.
    pub score: f32,
}

/// A query-time plugin over its own segment.
pub trait Retriever: Send + Sync {
    /// The retriever name, shown in `why`.
    fn name(&self) -> &str;
    /// Ranks candidates for one query. Must never return an atom outside
    /// `scope.candidates`, and must not let atoms outside `scope.visible` influence
    /// scores.
    fn search(&self, query: &str, scope: Scope<'_>, limit: usize) -> Result<Vec<Hit>>;

    /// Ranks candidates for several queries; one list per query, in order. Override
    /// when batching is cheaper (one embedding call for all queries).
    fn search_batch(&self, queries: &[String], scope: Scope<'_>, limit: usize) -> Result<Vec<Vec<Hit>>> {
        queries.iter().map(|q| self.search(q, scope, limit)).collect()
    }

    /// Whether the planner should also pass glossary expansions and document-scoped
    /// sub-queries (useful for lexical retrievers, wasted work for semantic ones).
    fn wants_expansions(&self) -> bool {
        true
    }
}

/// Opens a retriever from a pack's segment.
pub trait RetrieverFactory: Send + Sync {
    /// The segment and retriever name.
    fn name(&self) -> &str;
    /// Opens the retriever, or returns `None` when the pack has no such segment.
    fn open(&self, pack: &Pack) -> Result<Option<Box<dyn Retriever>>>;
}

/// A build-time plugin that derives text for an atom, such as an L1 abstract.
pub trait Enricher: Send + Sync {
    /// Plugin name.
    fn name(&self) -> &str;
    /// What the enricher reads and reaches.
    fn capabilities(&self) -> Capabilities;
    /// An abstract for the atom, or `None` to keep the default.
    fn summarize(&self, doc: &IndexDoc<'_>) -> Result<Option<String>>;
}

/// Severity of a lint finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Worth a look.
    Warning,
    /// Breaks the corpus contract; fails `kiwi lint`.
    Error,
}

/// One lint finding.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Finding {
    /// Severity.
    pub severity: Severity,
    /// Rule name, e.g. `broken-link`.
    pub rule: String,
    /// The atom or document concerned.
    pub id: String,
    /// What is wrong.
    pub message: String,
}

/// A build or CI check over a compiled pack.
pub trait Lint: Send + Sync {
    /// Rule set name.
    fn name(&self) -> &str;
    /// Runs the checks.
    fn check(&self, pack: &Pack) -> Result<Vec<Finding>>;
}

/// Writes the visible part of a pack in another format (`llms.txt`, JSONL…).
pub trait Exporter: Send + Sync {
    /// Format name.
    fn name(&self) -> &str;
    /// Writes the export of the `visible` atoms.
    fn export(&self, pack: &Pack, visible: &RoaringBitmap, out: &mut dyn Write) -> Result<()>;
}

/// Receives every audit record as one JSON line (S3, SIEM, OpenTelemetry…).
pub trait AuditSink: Send + Sync {
    /// Sink name.
    fn name(&self) -> &str;
    /// Delivers one record. Called in `seq` order while the log is locked, so it must
    /// not block: queue the line and ship it from another thread.
    fn deliver(&self, line: &str) -> Result<()>;
    /// Ships what is still queued and stops, within a bounded wait; called when the
    /// server stops. Records delivered after it are dropped.
    fn close(&self) {}
}

/// A query-time plugin that scores how well passages answer a query (a cross-encoder),
/// to reorder the top of a fused ranking. It only ever receives passages of atoms in the
/// caller's view, and of those only the ones the policy lets it read.
pub trait Reranker: Send + Sync {
    /// Plugin name, also the Cedar principal `plugin:<name>` it is governed as.
    fn name(&self) -> &str;
    /// What it reads and which hosts it reaches.
    fn capabilities(&self) -> Capabilities;
    /// One relevance score per passage, higher is better. Implementations bound their
    /// own time and return an error rather than block past it.
    fn score(&self, query: &str, passages: &[String]) -> Result<Vec<f32>>;
}
