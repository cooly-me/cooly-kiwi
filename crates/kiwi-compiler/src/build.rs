//! The build: read a snapshot, compile changed documents (reusing cached ones), link
//! them, run the indexers and store the pack.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use kiwi_core::pack::{StoreLock, replace_file};
use kiwi_core::plugin::{Enricher, IndexDoc, Indexer};
use kiwi_core::{Pack, PackStore};
use kiwi_policy::{KiwiOwners, PathDefaults};
use rayon::prelude::*;
use serde::Serialize;

use crate::compile::{self, CompiledDoc, cache_key, count_tokens};
use crate::config::{CONFIG_PATH, KiwiConfig};
use crate::error::{CompileError, Result};
use crate::link::{LinkInput, link};
use crate::source::{self, Snapshot, SourceMode};

/// The default pack directory inside a corpus.
pub const PACK_DIR: &str = ".kiwi/packs";

pub use crate::link::PluginGate;

/// What to build.
#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// The corpus root.
    pub root: PathBuf,
    /// How to read it.
    pub mode: SourceMode,
    /// The pack directory (default `<root>/.kiwi/packs`).
    pub out: Option<PathBuf>,
    /// Overrides the snapshot date.
    pub as_of: Option<String>,
}

impl BuildOptions {
    /// Options for a corpus with defaults.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            mode: SourceMode::Auto,
            out: None,
            as_of: None,
        }
    }

    /// The pack directory.
    pub fn out_dir(&self) -> PathBuf {
        self.out.clone().unwrap_or_else(|| self.root.join(PACK_DIR))
    }
}

/// The plugins taking part in a build.
#[derive(Default)]
pub struct BuildPlugins<'a> {
    /// Indexers, one segment each.
    pub indexers: Vec<&'a dyn Indexer>,
    /// Enrichers for L1 abstracts.
    pub enrichers: Vec<&'a dyn Enricher>,
    /// Governance over plugins; `None` lets every plugin see every atom.
    pub gate: Option<&'a PluginGate<'a>>,
}

/// What a build did.
#[derive(Debug, Clone, Serialize)]
pub struct BuildReport {
    /// Snapshot id.
    pub snapshot: String,
    /// `git` or `dir`.
    pub source: String,
    /// Documents.
    pub docs: usize,
    /// Atoms.
    pub atoms: usize,
    /// Atoms compiled in this build.
    pub atoms_rebuilt: usize,
    /// Atoms reused from the cache.
    pub atoms_reused: usize,
    /// Permission classes.
    pub classes: usize,
    /// Plugin segments.
    pub segments: Vec<String>,
    /// The pack file.
    pub pack: PathBuf,
    /// The pack's content hash.
    pub hash: String,
    /// False when an identical pack already existed.
    pub written: bool,
    /// Wall time in milliseconds.
    pub millis: u128,
}

/// The configuration of a snapshot (defaults when it has no `.kiwi/kiwi.toml`).
pub fn load_config(snapshot: &Snapshot) -> Result<KiwiConfig> {
    match snapshot.file(CONFIG_PATH) {
        Some(bytes) => {
            let text = std::str::from_utf8(bytes)
                .map_err(|e| CompileError::Config(format!("{CONFIG_PATH}: {e}")))?;
            KiwiConfig::parse(text)
        }
        None => Ok(KiwiConfig::default()),
    }
}

/// The `KIWIOWNERS` rules of a snapshot.
pub fn load_owners(snapshot: &Snapshot) -> Result<KiwiOwners> {
    match snapshot.file("KIWIOWNERS") {
        Some(bytes) => {
            let text =
                std::str::from_utf8(bytes).map_err(|e| CompileError::Config(format!("KIWIOWNERS: {e}")))?;
            Ok(KiwiOwners::parse(text)?)
        }
        None => Ok(KiwiOwners::default()),
    }
}

/// Builds a pack for the corpus and stores it.
pub fn build(options: &BuildOptions, plugins: &BuildPlugins<'_>) -> Result<BuildReport> {
    BuildSession::begin(options)?.finish(plugins)
}

/// One build of a store, holding its lock from before the snapshot is read until the
/// pack is current. Builds of one store run one after the other (the compile cache, the
/// snapshot index and `CURRENT` are shared), and the snapshot a build reads is the one
/// it stores: a build that waited never makes an older commit current. Read the
/// configuration and policy from [`BuildSession::snapshot`] so the plugins match it.
pub struct BuildSession {
    started: Instant,
    as_of: Option<String>,
    store: PackStore,
    snapshot: Snapshot,
    _lock: StoreLock,
}

impl BuildSession {
    /// Takes the store's lock, then reads the snapshot.
    pub fn begin(options: &BuildOptions) -> Result<Self> {
        let started = Instant::now();
        let store = PackStore::new(options.out_dir());
        let lock = store.lock()?;
        let snapshot = source::read(&options.root, &options.mode)?;
        Ok(Self {
            started,
            as_of: options.as_of.clone(),
            store,
            snapshot,
            _lock: lock,
        })
    }

    /// The snapshot being built.
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// Compiles the snapshot with `plugins`, stores the pack and makes it current.
    pub fn finish(self, plugins: &BuildPlugins<'_>) -> Result<BuildReport> {
        let previous = match self.store.current()? {
            Some(path) => Pack::open(&path).ok(),
            None => None,
        };
        let (data, rebuilt, reused) = compile_snapshot(
            &self.snapshot,
            self.as_of.as_deref(),
            &self.store.dir().join("cache"),
            previous.as_ref(),
            plugins,
        )?;
        let stored = self.store.write(&data)?;
        Ok(BuildReport {
            snapshot: data.snapshot.clone(),
            source: data.source.clone(),
            docs: data.docs.len(),
            atoms: data.atoms.len(),
            atoms_rebuilt: rebuilt,
            atoms_reused: reused,
            classes: data.classes.len(),
            segments: data.segments.iter().map(|s| s.name.clone()).collect(),
            pack: stored.path,
            hash: stored.hash,
            written: stored.written,
            millis: self.started.elapsed().as_millis(),
        })
    }
}

/// Compiles one document through the compile cache in `cache_dir`: a cached result is
/// reused, a new one is stored. Returns the document, its cache key and whether it was
/// compiled now.
pub fn compile_cached(
    file: &str,
    bytes: &[u8],
    defaults: &PathDefaults,
    config: &KiwiConfig,
    cache_dir: &Path,
) -> Result<(CompiledDoc, String, bool)> {
    let key = cache_key(file, bytes, defaults, config);
    let cache_file = cache_dir.join(format!("{key}.doc"));
    let cached: Option<CompiledDoc> = std::fs::read(&cache_file)
        .ok()
        .and_then(|b| rkyv::from_bytes::<CompiledDoc, rkyv::rancor::Error>(&b).ok());
    if let Some(doc) = cached {
        return Ok((doc, key, false));
    }
    let text = std::str::from_utf8(bytes).map_err(|e| CompileError::Document {
        path: file.to_owned(),
        line: 1,
        message: format!("not UTF-8: {e}"),
    })?;
    let doc = compile::compile_doc(file, text, defaults, config)?;
    let encoded = rkyv::to_bytes::<rkyv::rancor::Error>(&doc)
        .map_err(|e| CompileError::Config(format!("cache: {e}")))?;
    replace_file(&cache_file, &encoded)?;
    Ok((doc, key, true))
}

/// Compiles every document of a snapshot in parallel through the compile cache in
/// `cache_dir`, in the snapshot's (path) order. The cache is not pruned, so a caller
/// that is not a build (a proposal's lint) can share a store's cache.
pub fn compile_docs(
    snapshot: &Snapshot,
    config: &KiwiConfig,
    owners: &KiwiOwners,
    cache_dir: &Path,
) -> Result<Vec<(CompiledDoc, String, bool)>> {
    std::fs::create_dir_all(cache_dir).map_err(|e| CompileError::source(cache_dir, e))?;
    let files: Vec<(&String, &Vec<u8>)> = snapshot.markdown().collect();
    files
        .par_iter()
        .map(|(file, bytes)| compile_cached(file, bytes, &owners.defaults(file), config, cache_dir))
        .collect()
}

/// Compiles a snapshot into pack data, using and refreshing the compile cache in
/// `cache_dir`. Returns the data and the counts of rebuilt and reused atoms.
pub fn compile_snapshot(
    snapshot: &Snapshot,
    as_of: Option<&str>,
    cache_dir: &Path,
    previous: Option<&Pack>,
    plugins: &BuildPlugins<'_>,
) -> Result<(kiwi_core::model::PackData, usize, usize)> {
    let config = load_config(snapshot)?;
    let owners = load_owners(snapshot)?;
    // Documents compile independently: in parallel, each cached by its key.
    let compiled = compile_docs(snapshot, &config, &owners, cache_dir)?;
    let mut docs: Vec<CompiledDoc> = Vec::with_capacity(compiled.len());
    let mut used_keys: BTreeSet<String> = BTreeSet::new();
    let (mut rebuilt, mut reused) = (0usize, 0usize);
    for (doc, key, fresh) in compiled {
        if fresh {
            rebuilt += doc.atoms.len();
        } else {
            reused += doc.atoms.len();
        }
        used_keys.insert(key);
        docs.push(doc);
    }
    check_unique_ids(&docs)?;
    enrich(&mut docs, cache_dir, plugins, &mut used_keys)?;
    prune_cache(cache_dir, &used_keys);

    let input = LinkInput {
        namespace: &config.namespace,
        snapshot: &snapshot.id,
        source: snapshot.kind,
        as_of: as_of.unwrap_or(&snapshot.as_of),
        config_hash: config.build_hash(),
    };
    let data = link(&docs, &input, &plugins.indexers, previous, plugins.gate)?;
    Ok((data, rebuilt, reused))
}

/// Refuses two documents with the same frontmatter id (ignoring case).
pub fn check_unique_ids(docs: &[CompiledDoc]) -> Result<()> {
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    for doc in docs {
        if let Some(id) = &doc.doc_id {
            let key = kiwi_core::text::key(id);
            if let Some(other) = seen.insert(key, doc.path.clone()) {
                return Err(CompileError::Document {
                    path: format!("{}.md", doc.path),
                    line: 1,
                    message: format!("document id `{id}` is also used by {other}.md"),
                });
            }
        }
    }
    Ok(())
}

/// Runs the enrichers on atoms without an authored summary, caching each answer by
/// enricher and atom hash; the names of the cache entries used are added to `used`.
fn enrich(
    docs: &mut [CompiledDoc],
    cache_dir: &Path,
    plugins: &BuildPlugins<'_>,
    used: &mut BTreeSet<String>,
) -> Result<()> {
    for enricher in &plugins.enrichers {
        let caps = enricher.capabilities();
        for doc in docs.iter_mut() {
            for atom in doc.atoms.iter_mut() {
                if atom.l1_authored {
                    continue;
                }
                if let Some(gate) = plugins.gate
                    && !gate(enricher.name(), &caps, &atom.chain)
                {
                    continue;
                }
                let key = blake3::hash(
                    [enricher.name().as_bytes(), b"\0", &atom.hash[..]]
                        .concat()
                        .as_slice(),
                )
                .to_hex();
                let name = format!("enrich-{key}");
                let file = cache_dir.join(format!("{name}.txt"));
                used.insert(name);
                let summary = match std::fs::read_to_string(&file) {
                    Ok(text) => Some(text),
                    Err(_) => {
                        let doc_view = IndexDoc {
                            uid: 0,
                            id: &atom.id,
                            title: &atom.title,
                            breadcrumb: &atom.l0,
                            summary: &atom.l1,
                            body: &atom.plain,
                            hash: &atom.hash,
                            class: 0,
                            withheld: false,
                        };
                        let produced = enricher.summarize(&doc_view)?;
                        if let Some(text) = &produced {
                            replace_file(&file, text.as_bytes())?;
                        }
                        produced
                    }
                };
                if let Some(summary) = summary.filter(|s| !s.trim().is_empty()) {
                    atom.l1 = kiwi_core::text::truncate_chars(summary.trim(), 320);
                    atom.tokens.l1 = count_tokens(&atom.l1);
                }
            }
        }
    }
    Ok(())
}

fn prune_cache(cache_dir: &Path, used: &BTreeSet<String>) {
    let Ok(entries) = std::fs::read_dir(cache_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let stale = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| {
                n.strip_suffix(".doc")
                    .or_else(|| n.strip_suffix(".json"))
                    .or_else(|| n.strip_suffix(".txt"))
            })
            .is_some_and(|key| !used.contains(key));
        if stale {
            // Best effort: a stale entry left behind only costs disk space.
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::sync::Arc;

    use kiwi_core::api::{Filters, ReadRequest, SearchRequest};
    use kiwi_core::model::PackData;
    use kiwi_core::plugin::Lint;
    use kiwi_core::{Engine, EngineConfig, HiddenMode, View};

    use super::*;
    use crate::lint::BuiltinLint;

    const LONG: &str = "This sentence is long enough to keep the section as its own atom. ";

    fn corpus(kiwiowners: &str) -> Snapshot {
        let body = LONG.repeat(6);
        let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        let mut add = |path: &str, text: String| {
            files.insert(path.to_owned(), text.into_bytes());
        };
        add("KIWIOWNERS", kiwiowners.to_owned());
        add(
            "product/prd.md",
            format!(
                "---\nid: PRD-1\nsummary: S.\n---\n# Checkout\n\n{body}\n\n## Pricing {{#pricing access=finance}}\n\n{body}\n"
            ),
        );
        add(
            "product/own.md",
            format!("---\nid: PRD-2\nowner: \"@frontmatter\"\nsummary: S.\n---\n# Own\n\n{body}\n"),
        );
        add(
            "finance/budget.md",
            format!("---\nid: FIN-1\nsummary: S.\n---\n# Budget\n\n{body}\n"),
        );
        add(
            "notes/loose.md",
            format!("---\nid: NOTE-1\nsummary: S.\n---\n# Loose\n\n{body}\n"),
        );
        Snapshot {
            kind: "dir",
            id: "dir:test".into(),
            as_of: "2026-11-24".into(),
            files,
            repo: None,
        }
    }

    fn data(kiwiowners: &str) -> PackData {
        let cache = tempfile::tempdir().unwrap();
        let plugins = BuildPlugins {
            indexers: Vec::new(),
            enrichers: Vec::new(),
            gate: None,
        };
        compile_snapshot(&corpus(kiwiowners), None, cache.path(), None, &plugins)
            .unwrap()
            .0
    }

    fn pack(data: &PackData) -> Arc<Pack> {
        Arc::new(Pack::from_bytes(&kiwi_core::pack::encode(data).unwrap()).unwrap())
    }

    fn owner_of<'a>(data: &'a PackData, path: &str) -> Option<&'a str> {
        data.docs
            .iter()
            .find(|d| d.path == path)
            .and_then(|d| d.meta.owner.as_deref())
    }

    const ACCESS: &str = "*  label=internal\nfinance/  access=finance\n";
    const OWNED: &str = "*  label=internal  owner=@team-docs\n\
                         product/  owner=cot-buom@cooly.me\n\
                         finance/  access=finance  owner=cfo@example.com\n\
                         notes/  owner=@notes\n\
                         notes/loose.md  owner=@last\n";

    #[test]
    fn kiwiowners_supplies_the_owner_and_frontmatter_overrides_it() {
        let data = data(OWNED);
        assert_eq!(owner_of(&data, "product/prd"), Some("cot-buom@cooly.me"));
        assert_eq!(
            owner_of(&data, "product/own"),
            Some("@frontmatter"),
            "frontmatter wins"
        );
        assert_eq!(owner_of(&data, "finance/budget"), Some("cfo@example.com"));
        assert_eq!(
            owner_of(&data, "notes/loose"),
            Some("@last"),
            "the last match wins"
        );
        let without = self::data(ACCESS);
        assert_eq!(owner_of(&without, "product/prd"), None);
        assert_eq!(owner_of(&without, "product/own"), Some("@frontmatter"));
    }

    #[test]
    fn missing_owner_lint_falls_back_to_kiwiowners() {
        let missing = |kiwiowners: &str| -> Vec<String> {
            let pack = pack(&data(kiwiowners));
            let mut ids: Vec<String> = BuiltinLint
                .check(&pack)
                .unwrap()
                .into_iter()
                .filter(|f| f.rule == "missing-owner")
                .map(|f| f.id)
                .collect();
            ids.sort();
            ids
        };
        assert_eq!(
            missing(ACCESS),
            vec!["finance/budget", "notes/loose", "product/prd"]
        );
        assert!(missing(OWNED).is_empty());
    }

    #[test]
    fn the_owner_filter_stubs_and_reads_use_the_effective_owner() {
        let pack = pack(&data(OWNED));
        let view = View::everything(&pack);
        let engine = Engine::new(Arc::clone(&pack), Vec::new(), EngineConfig::default());
        let owned_by = |owner: &str| -> Vec<String> {
            let request = SearchRequest {
                filters: Filters {
                    owner: vec![owner.to_owned()],
                    ..Filters::default()
                },
                budget: Some(4000),
                ..SearchRequest::default()
            };
            let (response, _) = engine.search(&view, &request).unwrap();
            let mut ids: Vec<String> = response.results.into_iter().map(|r| r.id).collect();
            ids.sort();
            ids
        };
        assert_eq!(
            owned_by("cot-buom@cooly.me"),
            vec!["product/prd", "product/prd#pricing"]
        );
        assert_eq!(owned_by("@frontmatter"), vec!["product/own"]);
        assert!(
            owned_by("@team-docs").is_empty(),
            "every document has a closer owner"
        );

        let read = ReadRequest {
            ids: vec!["PRD-1".into()],
            ..ReadRequest::default()
        };
        let (response, _) = engine.read(&view, &read).unwrap();
        assert_eq!(response.atoms[0].owner.as_deref(), Some("cot-buom@cooly.me"));

        let config = EngineConfig {
            hidden: HiddenMode::Stub,
            ..EngineConfig::default()
        };
        let stubs = Engine::new(Arc::clone(&pack), Vec::new(), config);
        let nothing = View::from_classes(&pack, []);
        let (response, _) = stubs.read(&nothing, &read).unwrap();
        assert_eq!(response.hidden[0].owner.as_deref(), Some("cot-buom@cooly.me"));
    }

    #[test]
    fn owners_never_change_a_chain_or_a_permission_class() {
        let access = data(ACCESS);
        let owned = data(OWNED);
        assert_eq!(access.classes, owned.classes);
        let classes = |data: &PackData| -> Vec<(String, u32)> {
            data.atoms.iter().map(|a| (a.id.clone(), a.class)).collect()
        };
        assert_eq!(classes(&access), classes(&owned));
        // A line with only `owner=` leaves an uncovered document unlabeled.
        let bare = data("notes/  owner=@notes\n");
        let unlabeled = data("");
        assert_eq!(bare.classes, unlabeled.classes);
        assert_eq!(classes(&bare), classes(&unlabeled));
    }
}
