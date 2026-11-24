//! One corpus behind the server: its current pack and engine, its policy gate, its
//! audit log, feedback and proposals. A federated server holds several, each governed
//! and audited on its own.
//!
//! The pack and the governance read from the same snapshot (policy, principals,
//! configuration) are served together as one [`Served`], swapped as a whole when a
//! watched corpus is rebuilt, so no call ever sees a new pack under an old policy or
//! the other way round.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use kiwi_compiler::KiwiConfig;
use kiwi_compiler::propose::{self, Author, Edit, Permissions, ProposalTarget, ProposeError};
use kiwi_core::api::AuditFacts;
use kiwi_core::model::AccessDecl;
use kiwi_core::plugin::{Reranker, RetrieverFactory};
use kiwi_core::rerank::AttachedReranker;
use kiwi_core::{Engine, EngineConfig, Pack, PackStore, View};
use kiwi_policy::{Action, AuditEvent, AuditLog, AuditStatus, Identity, PolicyGate};

use crate::feedback::{FeedbackLog, FeedbackRecord};
use crate::service::{ProposeChange, ToolError};

/// Settings of one served corpus.
#[derive(Debug, Clone)]
pub struct ServiceOptions {
    /// The corpus root.
    pub corpus: PathBuf,
    /// The pack store.
    pub packs: PathBuf,
    /// Where the audit log, feedback and proposal records live.
    pub state_dir: PathBuf,
    /// The corpus configuration.
    pub config: KiwiConfig,
    /// Write the audit log.
    pub audit: bool,
    /// Allow `kiwi_propose` (needs the corpus in a Git repository).
    pub proposals: bool,
    /// Standard output carries the MCP protocol (stdio), so no audit sink may use it.
    pub stdout_is_protocol: bool,
    /// The snapshot the policy, principals and configuration were read from. Its pack
    /// is served, and the corpus does not open without it, so a pack is never served
    /// under another revision's governance. `None` serves the store's current pack,
    /// for a caller that read the governance from that pack's own build.
    pub snapshot: Option<String>,
}

/// The plugins of one corpus.
#[derive(Clone, Default)]
pub struct CorpusPlugins {
    /// Retrievers, opened on every pack.
    pub factories: Vec<Arc<dyn RetrieverFactory>>,
    /// The reranker and how many results from the top it reorders.
    pub reranker: Option<(Arc<dyn Reranker>, usize)>,
}

impl std::fmt::Debug for CorpusPlugins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CorpusPlugins")
            .field("factories", &self.factories.len())
            .field(
                "reranker",
                &self.reranker.as_ref().map(|(r, k)| (r.name().to_owned(), *k)),
            )
            .finish()
    }
}

/// The caller's rights, as the corpus policy decides them.
struct CallerPermissions<'a> {
    gate: &'a PolicyGate,
    identity: &'a Identity,
}

impl Permissions for CallerPermissions<'_> {
    fn read(&self, chain: &[AccessDecl]) -> Result<bool, String> {
        self.gate
            .allows_chain(self.identity, chain)
            .map_err(|e| e.to_string())
    }

    fn propose(&self, chain: &[AccessDecl]) -> Result<bool, String> {
        self.gate
            .allows_write(self.identity, Action::Propose, chain)
            .map_err(|e| e.to_string())
    }

    fn merge(&self, chain: &[AccessDecl]) -> Result<bool, String> {
        self.gate
            .allows_write(self.identity, Action::Merge, chain)
            .map_err(|e| e.to_string())
    }
}

/// The edit one requested change asks for.
fn edit_of(change: &ProposeChange) -> Result<Edit, ToolError> {
    Ok(
        match (&change.path, &change.section, &change.content, &change.set) {
            (None, Some(section), Some(body), None) => Edit::Section {
                id: section.clone(),
                body: body.clone(),
            },
            (Some(path), None, None, Some(keys)) => Edit::Set {
                path: path.clone(),
                keys: keys.clone(),
            },
            (Some(path), None, Some(content), None) => Edit::Write {
                path: path.clone(),
                content: content.clone(),
            },
            (Some(path), None, None, None) => Edit::Delete { path: path.clone() },
            _ => {
                return Err(ToolError::Invalid(
                    "each change is {section, content}, {path, set}, {path, content} or {path}".into(),
                ));
            }
        },
    )
}

/// What a call did as a whole: the same on the record of every corpus it consulted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSummary {
    /// How it ended.
    pub status: AuditStatus,
    /// Items the answer returned, for the tools that return items.
    pub results: Option<u32>,
    /// Tokens of the body the caller received.
    pub tokens: u32,
    /// The token budget applied.
    pub budget: Option<u32>,
    /// Server time, in milliseconds.
    pub latency_ms: u64,
    /// What the caller asked for; recorded only where `audit.record_requests` is set.
    pub requests: Vec<String>,
    /// The reference of a failure.
    pub reference: Option<String>,
}

/// How a proposal ended, for the call's audit record in its corpus.
pub struct Proposed {
    /// The proposal and the snapshot it is based on, or what the caller is told.
    pub answer: Result<(propose::Proposal, String), ToolError>,
    /// The engine served when the proposal was decided.
    pub engine: Arc<Engine>,
    /// The atoms the proposal changes that the caller can see: the sections it edits and
    /// the roots of the documents it writes, sets or deletes.
    pub facts: AuditFacts,
    /// For whoever reads the local log: why it was refused or failed, or its conflicts
    /// with content hidden from the caller.
    pub note: Option<String>,
    /// The reference of a failure, which the caller is given in a message.
    pub failure: Option<String>,
}

/// A proposal that did not open: what the caller is told and what the record keeps.
struct Unopened {
    error: ToolError,
    note: Option<String>,
    failure: Option<String>,
}

impl From<ToolError> for Unopened {
    fn from(error: ToolError) -> Self {
        Self {
            error,
            note: None,
            failure: None,
        }
    }
}

impl From<kiwi_core::KiwiError> for Unopened {
    fn from(error: kiwi_core::KiwiError) -> Self {
        ToolError::from(error).into()
    }
}

/// The stable ids of the atoms `uids` of `pack`, distinct in their order, and the
/// distinct paths of their documents in first-seen order.
fn stable_ids(pack: &Pack, uids: &[u32]) -> (Vec<String>, Vec<String>) {
    let mut ids: Vec<String> = Vec::with_capacity(uids.len());
    let mut docs: Vec<String> = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();
    let mut seen_docs = std::collections::HashSet::new();
    for &uid in uids {
        let Some(atom) = pack.atom(uid) else {
            continue;
        };
        if seen_ids.insert(uid) {
            ids.push(atom.id.as_str().to_owned());
        }
        if let Some(doc) = pack.doc(atom.doc.to_native())
            && seen_docs.insert(atom.doc.to_native())
        {
            docs.push(doc.path.as_str().to_owned());
        }
    }
    (ids, docs)
}

/// The root atom of the document at `path` (a corpus-relative Markdown path) in `pack`,
/// if there is one.
fn document_root(pack: &Pack, path: &str) -> Result<Option<u32>, ToolError> {
    let Some(stem) = path.strip_suffix(".md") else {
        return Ok(None);
    };
    Ok(pack.resolve_id(stem)?.filter(|&uid| {
        pack.doc_of(uid)
            .is_some_and(|doc| doc.path.as_str() == stem && doc.root.to_native() == uid)
    }))
}

/// Engine settings from the corpus configuration.
pub fn engine_config(config: &KiwiConfig) -> EngineConfig {
    EngineConfig {
        default_budget: config.search.budget,
        default_limit: config.search.limit,
        hidden: config.governance.hidden,
        ..EngineConfig::default()
    }
}

/// Opens the engine over a pack with every factory that finds its segment, and the
/// reranker allowed, class by class, what the policy lets it read.
pub fn open_engine(
    pack: Arc<Pack>,
    plugins: &CorpusPlugins,
    config: EngineConfig,
    gate: Option<&PolicyGate>,
) -> Result<Engine, ToolError> {
    let mut retrievers = Vec::new();
    for factory in &plugins.factories {
        if let Some(retriever) = factory.open(&pack)? {
            retrievers.push(retriever);
        }
    }
    let attached = match (&plugins.reranker, gate) {
        (Some((reranker, top_k)), Some(gate)) => Some(attach(&pack, reranker, *top_k, gate)),
        _ => None,
    };
    let engine = Engine::new(pack, retrievers, config);
    Ok(match attached {
        Some(attached) => engine.with_reranker(attached),
        None => engine,
    })
}

/// `reranker` attached to `pack`, allowed class by class what `gate` lets it read with
/// its capabilities.
fn attach(pack: &Pack, reranker: &Arc<dyn Reranker>, top_k: usize, gate: &PolicyGate) -> AttachedReranker {
    let caps = reranker.capabilities();
    let allowed = pack
        .data()
        .classes
        .iter()
        .map(|class| {
            let chain: Vec<AccessDecl> = class
                .chain
                .iter()
                .map(|d| AccessDecl::new(d.label.as_str(), d.groups.iter().map(|g| g.as_str().to_owned())))
                .collect();
            gate.plugin_allows(reranker.name(), &caps, &chain)
        })
        .collect();
    AttachedReranker::new(Arc::clone(reranker), top_k, allowed)
}

/// Paragraphs by BLAKE3 hash, with the classes of the atoms that hold each.
type ParagraphClasses = std::collections::HashMap<[u8; 32], Vec<u32>>;

/// The BLAKE3 hashes of the paragraphs of `text`: its blocks between blank lines, each
/// with its lines trimmed.
fn paragraphs(text: &str) -> impl Iterator<Item = [u8; 32]> {
    let text = text.replace("\r\n", "\n");
    let blocks: Vec<String> = text.split("\n\n").map(str::to_owned).collect();
    blocks.into_iter().filter_map(|block| {
        let lines: Vec<&str> = block.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        (!lines.is_empty()).then(|| *blake3::hash(lines.join("\n").as_bytes()).as_bytes())
    })
}

fn paragraph_classes(pack: &Pack) -> Result<ParagraphClasses, ToolError> {
    let mut held = ParagraphClasses::new();
    for uid in 0..pack.atom_count() {
        let Some(atom) = pack.atom(uid) else {
            continue;
        };
        let class = atom.class.to_native();
        for hash in paragraphs(&pack.body(uid)?) {
            let holders = held.entry(hash).or_default();
            if !holders.contains(&class) {
                holders.push(class);
            }
        }
    }
    Ok(held)
}

/// The rerankers of a federation, by corpus: each corpus's own, if it has one.
pub type FederationRerankers = Vec<Option<(Arc<dyn Reranker>, usize)>>;

/// A pack's engine with the governance of the same snapshot.
pub struct Served {
    /// The engine over the pack.
    pub engine: Arc<Engine>,
    /// The policy gate (policy and principals).
    pub gate: Arc<PolicyGate>,
    /// The corpus configuration.
    pub config: KiwiConfig,
    /// The plugins the engine was opened with.
    plugins: CorpusPlugins,
    /// On a federated server, every corpus's reranker (by corpus index) attached to
    /// this pack under this policy: what each may read here when it leads a search.
    federation: Vec<Option<AttachedReranker>>,
    /// The BLAKE3 hash of every paragraph of every atom's text (L2), with the
    /// permission classes that hold it; computed on first use.
    paragraphs: std::sync::OnceLock<Arc<ParagraphClasses>>,
    /// Distinguishes this from every other `Served` of the process (response caches are
    /// keyed by it: the same pack under another policy or configuration answers anew).
    generation: u64,
}

impl std::fmt::Debug for Served {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Served")
            .field("pack", &self.engine.pack().id())
            .field("generation", &self.generation)
            .finish()
    }
}

static GENERATION: AtomicU64 = AtomicU64::new(1);

impl Served {
    fn new(
        engine: Engine,
        gate: Arc<PolicyGate>,
        config: KiwiConfig,
        plugins: CorpusPlugins,
        federation: &[Option<(Arc<dyn Reranker>, usize)>],
    ) -> Self {
        let federation = federation
            .iter()
            .map(|reranker| {
                reranker
                    .as_ref()
                    .map(|(reranker, top_k)| attach(engine.pack(), reranker, *top_k, &gate))
            })
            .collect();
        Self {
            engine: Arc::new(engine),
            gate,
            config,
            plugins,
            federation,
            paragraphs: std::sync::OnceLock::new(),
            generation: GENERATION.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// Whether `text` holds a paragraph that, in the served pack, only atoms outside the
    /// permitted `classes` hold: text the caller may not read now.
    pub fn hides_any_paragraph(&self, text: &str, classes: &[u32]) -> Result<bool, ToolError> {
        let held = match self.paragraphs.get() {
            Some(held) => Arc::clone(held),
            None => {
                let computed = Arc::new(paragraph_classes(self.engine.pack())?);
                Arc::clone(self.paragraphs.get_or_init(|| computed))
            }
        };
        Ok(paragraphs(text).any(|hash| {
            held.get(&hash)
                .is_some_and(|holders| holders.iter().all(|class| !classes.contains(class)))
        }))
    }

    /// The reranker of federation corpus `lead`, attached to this pack under this
    /// policy.
    pub fn federation_reranker(&self, lead: usize) -> Option<&AttachedReranker> {
        self.federation.get(lead).and_then(Option::as_ref)
    }

    /// What response caches key this state by.
    pub fn key(&self) -> String {
        format!("{}#{}", self.engine.pack().id(), self.generation)
    }

    /// The caller's view of `engine`'s pack under this policy, and the classes behind it.
    pub fn view_of(&self, engine: &Engine, identity: &Identity) -> Result<(View, Vec<u32>), ToolError> {
        let classes = self.gate.classes(engine.pack(), identity)?;
        Ok((
            View::from_classes(engine.pack(), classes.iter().copied()),
            classes,
        ))
    }

    /// The caller's view of the served pack.
    pub fn view(&self, identity: &Identity) -> Result<(View, Vec<u32>), ToolError> {
        self.view_of(&self.engine, identity)
    }
}

/// Whether `rev` names a snapshot: a commit hash or `dir:` and a hash, given in full or
/// by at least seven hex digits.
fn snapshot_ref(rev: &str) -> bool {
    let hex = rev.strip_prefix("dir:").unwrap_or(rev);
    (7..=64).contains(&hex.len()) && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Engines of pinned snapshots kept open, most recent first.
const PINNED_ENGINES: usize = 4;

/// One served corpus.
pub struct Corpus {
    namespace: String,
    served: RwLock<Arc<Served>>,
    pinned: Mutex<Vec<Arc<Engine>>>,
    audit: Option<AuditLog>,
    feedback: FeedbackLog,
    store: PackStore,
    options: ServiceOptions,
    /// The rerankers of the federation this corpus is part of (empty when it is served
    /// alone), kept for every new [`Served`].
    federation: FederationRerankers,
    /// Whether a pinned commit is an ancestor of a served one, by (pinned, served):
    /// commits never change, so a verdict holds for good.
    lineage: Mutex<std::collections::HashMap<(String, String), bool>>,
}

impl std::fmt::Debug for Corpus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Corpus")
            .field("namespace", &self.namespace)
            .field("corpus", &self.options.corpus)
            .finish()
    }
}

impl Corpus {
    /// Opens a corpus on the current pack of its store.
    pub fn open(
        options: ServiceOptions,
        gate: PolicyGate,
        plugins: CorpusPlugins,
    ) -> Result<Self, ToolError> {
        let store = PackStore::new(&options.packs);
        let internal = |e: kiwi_core::KiwiError| ToolError::Internal(e.to_string());
        let path = match &options.snapshot {
            Some(snapshot) => store.find_snapshot(snapshot).map_err(internal)?.ok_or_else(|| {
                ToolError::Internal(format!(
                    "no pack of snapshot {snapshot} (the one the policy was read from) in {}; build that snapshot with `kiwi build` and the same source options",
                    options.packs.display()
                ))
            })?,
            None => store.current().map_err(internal)?.ok_or_else(|| {
                ToolError::Internal(format!(
                    "no pack in {}; run `kiwi build` first",
                    options.packs.display()
                ))
            })?,
        };
        let pack = Pack::open(&path).map_err(|e| {
            ToolError::Internal(format!(
                "{e}; rebuild it with `kiwi build {}`",
                options.corpus.display()
            ))
        })?;
        if let Some(snapshot) = &options.snapshot
            && pack.snapshot() != snapshot
        {
            return Err(ToolError::Internal(format!(
                "the pack {} is of snapshot {}, not {snapshot} the policy was read from; refusing to serve it",
                path.display(),
                pack.snapshot()
            )));
        }
        let engine = open_engine(
            Arc::new(pack),
            &plugins,
            engine_config(&options.config),
            Some(&gate),
        )?;
        kiwi_core::pack::ensure_ignored(&options.state_dir)?;
        let audit = if options.audit {
            let sinks = kiwi_policy::sinks::build(&options.config.audit.sinks, options.stdout_is_protocol)
                .map_err(ToolError::Internal)?;
            Some(
                AuditLog::open(options.state_dir.join("audit.log"), sinks)
                    .map_err(|e| ToolError::Internal(e.to_string()))?,
            )
        } else {
            None
        };
        let served = Served::new(engine, Arc::new(gate), options.config.clone(), plugins, &[]);
        Ok(Self {
            namespace: options.config.namespace.clone(),
            served: RwLock::new(Arc::new(served)),
            pinned: Mutex::new(Vec::new()),
            audit,
            feedback: FeedbackLog::new(options.state_dir.join("feedback.jsonl")),
            store,
            options,
            federation: Vec::new(),
            lineage: Mutex::new(std::collections::HashMap::new()),
        })
    }

    /// Joins a federation whose corpora have `rerankers` (by corpus index). Whichever
    /// of them leads a search (the first corpus the caller can see), this corpus's
    /// policy decides, class by class, what that reranker may read here.
    pub fn join_federation(&mut self, rerankers: FederationRerankers) -> Result<(), ToolError> {
        let current = self.served();
        let engine = open_engine(
            Arc::clone(current.engine.pack()),
            &current.plugins,
            engine_config(&current.config),
            Some(&current.gate),
        )?;
        let served = Served::new(
            engine,
            Arc::clone(&current.gate),
            current.config.clone(),
            current.plugins.clone(),
            &rerankers,
        );
        self.served = RwLock::new(Arc::new(served));
        self.federation = rerankers;
        Ok(())
    }

    /// The reranker this corpus ranks with.
    pub fn reranker(&self) -> Option<(Arc<dyn Reranker>, usize)> {
        self.served().plugins.reranker.clone()
    }

    /// The namespace.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// What the corpus serves now: one pack with the governance of its snapshot. A call
    /// takes it once and answers from it throughout.
    pub fn served(&self) -> Arc<Served> {
        match self.served.read() {
            Ok(guard) => Arc::clone(&guard),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }

    /// The current engine.
    pub fn engine(&self) -> Arc<Engine> {
        Arc::clone(&self.served().engine)
    }

    /// The current policy gate.
    pub fn gate(&self) -> Arc<PolicyGate> {
        Arc::clone(&self.served().gate)
    }

    /// Serves the pack at `pack` with the policy, principals and configuration read
    /// from the same snapshot, all at once; the audit sinks follow the new
    /// configuration. The retrievers come from `factories` (the configuration's); the
    /// reranker stays the one the server started with. Anything that fails to open
    /// leaves the current state serving.
    pub fn replace(
        &self,
        pack: &Path,
        gate: PolicyGate,
        config: KiwiConfig,
        factories: Vec<Arc<dyn RetrieverFactory>>,
    ) -> Result<(), ToolError> {
        if config.namespace != self.namespace {
            return Err(ToolError::Internal(format!(
                "the namespace changed from `{}` to `{}`; restart the server to serve it",
                self.namespace, config.namespace
            )));
        }
        let current = self.served();
        let plugins = CorpusPlugins {
            factories,
            reranker: current.plugins.reranker.clone(),
        };
        let engine = open_engine(
            Arc::new(Pack::open(pack)?),
            &plugins,
            engine_config(&config),
            Some(&gate),
        )?;
        let sinks = match &self.audit {
            Some(_) => Some(
                kiwi_policy::sinks::build(&config.audit.sinks, self.options.stdout_is_protocol)
                    .map_err(ToolError::Internal)?,
            ),
            None => None,
        };
        let served = Arc::new(Served::new(
            engine,
            Arc::new(gate),
            config,
            plugins,
            &self.federation,
        ));
        match self.served.write() {
            Ok(mut guard) => *guard = served,
            Err(poisoned) => *poisoned.into_inner() = served,
        }
        if let (Some(log), Some(sinks)) = (&self.audit, sinks) {
            log.replace_sinks(sinks);
        }
        Ok(())
    }

    /// The caller's view of `engine`'s pack under the current policy, and the classes
    /// behind it.
    pub fn view(&self, engine: &Engine, identity: &Identity) -> Result<(View, Vec<u32>), ToolError> {
        self.served().view_of(engine, identity)
    }

    /// Stops the audit log's sinks after shipping what they hold.
    pub fn close(&self) {
        if let Some(log) = &self.audit {
            log.close();
        }
    }

    /// Appends one audit record of a call: `facts` name atoms of `pack`, recorded by
    /// their stable ids and documents. What the caller asked for is kept only when the
    /// served configuration sets `audit.record_requests`.
    pub fn record(
        &self,
        identity: &Identity,
        tool: &str,
        pack: &Pack,
        facts: &AuditFacts,
        call: &CallSummary,
        note: Option<String>,
    ) -> Result<(), ToolError> {
        let Some(log) = &self.audit else {
            return Ok(());
        };
        let (ids, docs) = stable_ids(pack, &facts.atoms);
        let queries = if self.served().config.audit.record_requests {
            call.requests.clone()
        } else {
            Vec::new()
        };
        log.record(AuditEvent {
            principal: identity.principal.clone(),
            agent: identity.agent.clone(),
            channel: identity.channel.clone(),
            tool: tool.to_owned(),
            snapshot: pack.snapshot().to_owned(),
            namespace: self.namespace.clone(),
            atoms: facts.atoms.clone(),
            blocked: facts.blocked,
            status: Some(call.status),
            ids,
            docs,
            results: call.results,
            tokens: Some(call.tokens),
            budget: call.budget,
            latency_ms: Some(call.latency_ms),
            queries,
            reference: call.reference.clone(),
            note,
        })
        .map_err(|e| ToolError::Internal(e.to_string()))?;
        Ok(())
    }

    /// Records a feedback line.
    pub fn record_feedback(&self, record: FeedbackRecord) -> Result<(), ToolError> {
        self.feedback
            .append(record)
            .map_err(|e| ToolError::Internal(e.to_string()))
    }

    /// Opens a proposal on this corpus's repository. `requested` uses this corpus's own
    /// (unqualified) ids. Returns the proposal and the snapshot it is based on, with what
    /// its audit record keeps.
    ///
    /// The proposal is planned on the base revision itself, not on the served pack:
    /// sections are found again there by id and access chain and edited by the
    /// parser's spans. A refusal says nothing about content the caller cannot see; a
    /// failure reaches the caller as a generic message and the audit log in full.
    pub fn propose(
        &self,
        identity: &Identity,
        requested: &[ProposeChange],
        title: Option<&str>,
        rationale: &str,
    ) -> Proposed {
        let served = self.served();
        match self.open_proposal(&served, identity, requested, title, rationale) {
            Ok((proposal, snapshot, facts, note)) => Proposed {
                answer: Ok((proposal, snapshot)),
                engine: Arc::clone(&served.engine),
                facts,
                note,
                failure: None,
            },
            Err(unopened) => Proposed {
                answer: Err(unopened.error),
                engine: Arc::clone(&served.engine),
                facts: AuditFacts::default(),
                note: unopened.note,
                failure: unopened.failure,
            },
        }
    }

    fn open_proposal(
        &self,
        served: &Served,
        identity: &Identity,
        requested: &[ProposeChange],
        title: Option<&str>,
        rationale: &str,
    ) -> Result<(propose::Proposal, String, AuditFacts, Option<String>), Unopened> {
        if !self.options.proposals {
            return Err(ToolError::Invalid("proposals are disabled on this server".into()).into());
        }
        let engine = &served.engine;
        let gate = &served.gate;
        let snapshot = engine.pack().snapshot().to_owned();
        // A proposal is decided on the very commit the server serves; a server of the
        // working files (--worktree, or a --watch rebuild of them) serves none.
        if engine.pack().data().source.as_str() != "git" {
            return Err(ToolError::Invalid(
                "proposals need a server of a commit; this one serves the working files".into(),
            )
            .into());
        }
        let target = ProposalTarget {
            corpus: self.options.corpus.clone(),
            base: snapshot.clone(),
            forge: self.options.config.forge.clone(),
            state_dir: self.options.state_dir.clone(),
            cache_dir: self.options.packs.join("cache"),
        };
        let edits = requested.iter().map(edit_of).collect::<Result<Vec<_>, _>>()?;
        let permissions = CallerPermissions { gate, identity };
        let plan = match propose::plan(&target, &permissions, &edits) {
            Ok(plan) => plan,
            Err(ProposeError::Refused(message)) => {
                return Err(Unopened {
                    note: Some(format!("refused: {message}")),
                    error: ToolError::Invalid(message),
                    failure: None,
                });
            }
            Err(ProposeError::Failed(cause)) => return Err(Self::failed(&cause)),
        };
        let title = title
            .filter(|t| !t.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                format!(
                    "docs: update {}",
                    plan.changes.first().map_or("knowledge", |c| c.path.as_str())
                )
            });
        let user = gate.directory().user(&identity.principal);
        let author = Author {
            principal: identity.principal.clone(),
            agent: identity.agent.clone(),
            name: user
                .and_then(|u| u.name.clone())
                .unwrap_or_else(|| identity.principal.clone()),
            email: user
                .and_then(|u| u.emails.first().cloned())
                .unwrap_or_else(|| format!("{}@kiwi.invalid", identity.principal)),
        };
        // Read before the proposal opens, so nothing fails once it is open.
        let facts = Self::changed_atoms(served, identity, &edits)?;
        let proposal = match propose::open(&target, &plan, &title, rationale, &author) {
            Ok(proposal) => proposal,
            Err(cause) => return Err(Self::failed(&cause)),
        };
        let mut notes = plan.notes.clone();
        notes.extend(proposal.owner_review_unapplied.iter().cloned());
        let note = (!notes.is_empty()).then(|| notes.join("; "));
        Ok((proposal, snapshot, facts, note))
    }

    /// The atoms `edits` change that the caller can see in the served pack: each edited
    /// section, and the root of each document written, set or deleted. A path that is
    /// no document of the pack (a new file) or one hidden from the caller names none.
    fn changed_atoms(served: &Served, identity: &Identity, edits: &[Edit]) -> Result<AuditFacts, ToolError> {
        let (view, _) = served.view(identity)?;
        let pack = served.engine.pack();
        let mut facts = AuditFacts::default();
        for edit in edits {
            let uid = match edit {
                Edit::Section { id, .. } => pack.resolve_id(id)?,
                Edit::Set { path, .. } | Edit::Write { path, .. } | Edit::Delete { path } => {
                    document_root(pack, path)?
                }
            };
            if let Some(uid) = uid.filter(|&uid| view.contains(uid))
                && !facts.atoms.contains(&uid)
            {
                facts.atoms.push(uid);
            }
        }
        Ok(facts)
    }

    /// A proposal that failed for a reason that may concern content the caller cannot
    /// see, or the server (a forge's output, a path): the cause goes to the operator's
    /// log and the audit note under a reference, the caller gets only the reference.
    fn failed(cause: &dyn std::fmt::Display) -> Unopened {
        let reference = crate::server::log_internal(&format!("kiwi_propose: {cause}"));
        Unopened {
            error: ToolError::Invalid(format!(
                "the proposal could not be prepared (reference {reference})"
            )),
            note: Some(format!("failed (reference {reference}): {cause}")),
            failure: Some(reference),
        }
    }

    /// The engine for a pinned snapshot of this corpus: the current one, or a kept pack
    /// (the last few stay open) of the served source's lineage. `rev` is a full snapshot
    /// id or at least seven hex digits of one (after `dir:` for a directory snapshot).
    /// A server serving a commit serves only that commit's ancestors, never another
    /// branch and never a directory snapshot; one serving a directory serves only
    /// directory snapshots. Visibility at that snapshot is for the caller to check
    /// against the current pack (see `KiwiService::read_resource`).
    pub fn engine_at(&self, rev: &str) -> Result<Option<Arc<Engine>>, ToolError> {
        if !snapshot_ref(rev) {
            return Ok(None);
        }
        let current = self.engine();
        if current.pack().snapshot().starts_with(rev) {
            return Ok(Some(current));
        }
        let Some(path) = self
            .store
            .find_snapshot(rev)
            .map_err(|e| ToolError::Internal(e.to_string()))?
        else {
            return Ok(None);
        };
        let cached = self.pinned.lock().ok().and_then(|pinned| {
            pinned
                .iter()
                .find(|e| e.pack().snapshot().starts_with(rev))
                .map(Arc::clone)
        });
        if let Some(engine) = cached {
            return self.in_lineage(&current, engine);
        }
        let pack = match Pack::open(&path) {
            Ok(pack) => pack,
            // Removed by `kiwi gc` since the index was read.
            Err(kiwi_core::KiwiError::Io { .. }) => return Ok(None),
            Err(other) => return Err(other.into()),
        };
        let engine = Arc::new(open_engine(
            Arc::new(pack),
            &CorpusPlugins::default(),
            engine_config(&self.served().config),
            None,
        )?);
        if let Ok(mut pinned) = self.pinned.lock() {
            pinned.insert(0, Arc::clone(&engine));
            pinned.truncate(PINNED_ENGINES);
        }
        self.in_lineage(&current, engine)
    }

    /// `pinned` when it is of the served source's lineage, else nothing.
    fn in_lineage(&self, current: &Engine, pinned: Arc<Engine>) -> Result<Option<Arc<Engine>>, ToolError> {
        let (now, then) = (current.pack(), pinned.pack());
        let ok = match (now.data().source.as_str(), then.data().source.as_str()) {
            ("git", "git") => {
                let key = (then.snapshot().to_owned(), now.snapshot().to_owned());
                let known = self
                    .lineage
                    .lock()
                    .ok()
                    .and_then(|cache| cache.get(&key).copied());
                match known {
                    Some(verdict) => verdict,
                    None => {
                        let verdict =
                            kiwi_compiler::source::is_ancestor(&self.options.corpus, &key.0, &key.1)
                                .map_err(|e| ToolError::Internal(e.to_string()))?;
                        if let Ok(mut cache) = self.lineage.lock() {
                            cache.insert(key, verdict);
                        }
                        verdict
                    }
                }
            }
            ("dir", "dir") => true,
            _ => false,
        };
        Ok(ok.then_some(pinned))
    }
}
