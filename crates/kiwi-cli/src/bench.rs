//! Kiwi-Bench: recall@5, tokens per question and latency for each retrieval mode, leak
//! counts per principal, cold start and incremental build time, and an optional
//! synthetic scale run.
//!
//! Modes:
//! - `grep+read`: rank files by query-term hits and read the top three whole (the
//!   baseline an agent without Kiwi uses; it ignores permissions).
//! - `navigation`: no index; `kiwi_map` then `kiwi_read` of the three best-matching
//!   entries, picked by word overlap (a stand-in for the agent's own choice).
//! - `bm25`, `bm25+vec`: `kiwi_search` with the question's queries, then `kiwi_read` of
//!   the top three.
//!
//! Tokens are counted with cl100k over the exact JSON the tools return.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use clap::Args;
use kiwi_compiler::build::{BuildPlugins, PACK_DIR};
use kiwi_compiler::compile::count_tokens;
use kiwi_compiler::{BuildOptions, SourceMode, source};
use kiwi_core::api::{Filters, Level, MapNode, MapRequest, ReadRequest, SearchRequest, TraceRequest};
use kiwi_core::plugin::RetrieverFactory;
use kiwi_core::{Engine, Pack, PackStore, View, text};
use kiwi_policy::{Identity, PolicyGate};
use serde::{Deserialize, Serialize};

use crate::plugins::{self, PluginOverrides};

/// `kiwi bench` arguments.
#[derive(Args, Debug)]
pub struct BenchArgs {
    /// The corpus.
    #[arg(default_value = "fixtures/acme-kb")]
    corpus: PathBuf,
    /// Questions file.
    #[arg(long, default_value = "bench/questions.yaml")]
    questions: PathBuf,
    /// Principals file.
    #[arg(long, default_value = "bench/principals.yaml")]
    principals: PathBuf,
    /// Override the configured embedder (`hash` needs no model).
    #[arg(long)]
    embedder: Option<String>,
    /// Timed repetitions per question on the warm process (the reranked mode is timed
    /// once per question, on the call that is scored).
    #[arg(long, default_value_t = 5)]
    repeat: usize,
    /// Also measure a synthetic corpus of this many atoms.
    #[arg(long)]
    synthetic: Option<usize>,
    /// Keep the synthetic corpus (with its pack) in this directory.
    #[arg(long, requires = "synthetic")]
    keep_synthetic: Option<PathBuf>,
    /// Also measure `bm25+vec+rerank` with this local cross-encoder (a kiwi built with
    /// `fastembed`), e.g. `jina-reranker-v2-base-multilingual`.
    #[arg(long)]
    rerank: Option<String>,
    /// Results reranked from the top with --rerank (the local backend's default).
    #[arg(long, default_value_t = kiwi_compiler::config::DEFAULT_LOCAL_TOP_K as usize)]
    rerank_top_k: usize,
    /// Print the results as JSON.
    #[arg(long)]
    json: bool,
    /// Compare the results with this baseline (TOML, see `bench/baselines/`) and exit
    /// with code 1 on a regression beyond its tolerances.
    #[arg(long)]
    baseline: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct Question {
    id: String,
    question: String,
    queries: Vec<String>,
    principal: String,
    answers: Vec<String>,
    #[serde(default)]
    hidden: bool,
}

#[derive(Debug, Deserialize)]
struct Questions {
    questions: Vec<Question>,
}

#[derive(Debug, Deserialize)]
struct PrincipalCase {
    id: String,
    agent: String,
    #[serde(default = "stdio")]
    channel: String,
    #[serde(default)]
    must_see: Vec<String>,
    #[serde(default)]
    must_not_see: Vec<String>,
}

fn stdio() -> String {
    "stdio".into()
}

#[derive(Debug, Deserialize)]
struct Canary {
    word: String,
    section: String,
}

#[derive(Debug, Deserialize)]
struct Principals {
    principals: Vec<PrincipalCase>,
    #[serde(default)]
    canaries: Vec<Canary>,
}

/// One row of the result table.
#[derive(Debug, Clone, Serialize)]
pub struct ModeResult {
    /// Mode name.
    pub mode: String,
    /// Mean recall@5 over answerable questions (atom level).
    pub recall_at_5: f64,
    /// Mean recall@10 over answerable questions (atom level).
    pub recall_at_10: f64,
    /// Mean recall@5 at document level (an answer counts when its document is among
    /// the documents of the top five results), comparable with `grep+read`.
    pub doc_recall_at_5: f64,
    /// Mean reciprocal rank of the first answer atom among the first ten results
    /// (document level for `grep+read`).
    pub mrr: f64,
    /// Mean tokens of the discovery call alone (search or map response).
    pub discovery_tokens: f64,
    /// Mean tokens per question (discovery plus reading the top three).
    pub mean_tokens: f64,
    /// Median latency of the discovery call, milliseconds.
    pub p50_ms: f64,
    /// 95th percentile latency of the discovery call, milliseconds.
    pub p95_ms: f64,
    /// 99th percentile latency of the discovery call, milliseconds.
    pub p99_ms: f64,
    /// Hidden answers returned to principals who may not see them.
    pub leaks: usize,
    /// Questions whose answers were not all in the top five.
    pub misses: Vec<String>,
}

/// Everything `kiwi bench` measures.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// Snapshot benchmarked.
    pub snapshot: String,
    /// Atoms in the pack.
    pub atoms: u32,
    /// Answerable questions.
    pub questions: usize,
    /// Rows per mode.
    pub modes: Vec<ModeResult>,
    /// Leak checks run (responses inspected) and violations found.
    pub leak_checks: usize,
    /// Leak violations (must be 0).
    pub leak_violations: Vec<String>,
    /// Must-see expectations that failed (a policy too strict).
    pub missing_visibility: Vec<String>,
    /// Cold start: open the pack and retrievers and answer one search, median ms.
    pub cold_start_ms: f64,
    /// Full build of the corpus, ms.
    pub full_build_ms: f64,
    /// Rebuild after changing one file, ms, and atoms compiled.
    pub incremental_build_ms: f64,
    /// Atoms compiled by that rebuild.
    pub incremental_atoms: usize,
    /// Size of the pack file, bytes.
    pub pack_bytes: u64,
    /// Reloading the served corpus: parsing the policy and principals, opening the
    /// pack with its retrievers and swapping it in, median ms.
    pub reload_ms: f64,
    /// Rebuild with no change, ms, and atoms compiled (must be 0).
    pub noop_build_ms: f64,
    /// Atoms compiled by the no-change rebuild.
    pub noop_atoms: usize,
    /// Search latency on a raw question with the vector tier, median ms.
    pub query_embed_ms: Option<f64>,
    /// Opening the pack and answering a first query that needs the vector tier
    /// (includes loading the embedding model), ms.
    pub first_vector_query_ms: Option<f64>,
    /// The synthetic scale run, when asked for.
    pub scale: Option<ScaleReport>,
}

/// Latencies on a synthetic corpus.
#[derive(Debug, Clone, Serialize)]
pub struct ScaleReport {
    /// Atoms.
    pub atoms: u32,
    /// Full build, ms.
    pub build_ms: f64,
    /// Rebuild after one changed file, ms.
    pub incremental_build_ms: f64,
    /// Pack size, bytes.
    pub pack_bytes: u64,
    /// Cold start (open, retrievers, first search), median ms.
    pub cold_start_ms: f64,
    /// Exact id lookup through `kiwi_search`: p50, p95, p99 ms.
    pub exact_ms: (f64, f64, f64),
    /// Structural filter query: p50, p95, p99 ms.
    pub structural_ms: (f64, f64, f64),
    /// BM25 query through the planner: p50, p95, p99 ms.
    pub bm25_ms: (f64, f64, f64),
    /// BM25 retriever alone: p50, p95, p99 ms.
    pub bm25_retriever_ms: (f64, f64, f64),
    /// Hamming scan over all atoms (top 64): p50, p95, p99 ms.
    pub vec_scan_ms: (f64, f64, f64),
    /// Vector query through the planner (hash embedder): p50, p95, p99 ms.
    pub vec_ms: (f64, f64, f64),
}

fn percentile(samples: &mut [f64], p: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.sort_by(|a, b| a.total_cmp(b));
    let rank = ((p / 100.0) * (samples.len() as f64 - 1.0)).round() as usize;
    samples[rank.min(samples.len() - 1)]
}

fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

const STOPWORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "of", "to", "in", "on", "for", "is", "are", "do", "does", "we", "our",
    "what", "how", "why", "which", "who", "when", "it", "be", "by", "with", "at", "as", "that", "this",
    "from", "can", "i", "you", "there", "after", "before", "about", "into", "not", "no", "if", "than",
    "rather",
];

fn content_words(text_in: &str) -> BTreeSet<String> {
    let folded = text::fold(text_in);
    text::words(&folded)
        .into_iter()
        .flat_map(|w| {
            w.split(['-', '/', '#', '.', '_'])
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .filter(|w| w.len() > 2 && !STOPWORDS.contains(&w.as_str()))
        .map(|w| w.trim_end_matches('s').to_owned())
        .collect()
}

fn flatten(nodes: &[MapNode], out: &mut Vec<(String, String)>) {
    for node in nodes {
        out.push((
            node.id.clone(),
            format!("{} {}", node.title, node.summary.clone().unwrap_or_default()),
        ));
        flatten(&node.children, out);
    }
}

/// The agent questions are asked through: the command line, which the example
/// corpus's directory lists.
const BENCH_AGENT: &str = "cli";

struct Ctx<'a> {
    gate: &'a PolicyGate,
}

impl Ctx<'_> {
    fn view(&self, pack: &Pack, principal: &str, agent: &str) -> Result<View> {
        self.view_on(pack, principal, agent, "stdio")
    }

    fn view_on(&self, pack: &Pack, principal: &str, agent: &str, channel: &str) -> Result<View> {
        Ok(self.gate.view(pack, &Identity::new(principal, agent, channel))?)
    }
}

fn resolve_all(pack: &Pack, ids: &[String]) -> Result<BTreeSet<u32>> {
    let mut out = BTreeSet::new();
    for id in ids {
        let uid = pack
            .resolve_id(id)?
            .ok_or_else(|| anyhow!("bench answer `{id}` names no atom"))?;
        out.insert(uid);
    }
    Ok(out)
}

fn read_top(engine: &Engine, view: &View, ids: &[String]) -> Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let (response, _) = engine.read(
        view,
        &ReadRequest {
            ids: ids.iter().take(3).cloned().collect(),
            level: Some(Level::L2),
            context: None,
            ..Default::default()
        },
    )?;
    Ok(count_tokens(&serde_json::to_string(&response)?) as usize)
}

fn search_request(question: &Question) -> SearchRequest {
    SearchRequest {
        queries: question.queries.clone(),
        filters: Filters::default(),
        budget: None,
        limit: None,
    }
}

fn run_search_mode(
    name: &str,
    engine: &Engine,
    ctx: &Ctx<'_>,
    questions: &[Question],
    repeat: usize,
) -> Result<ModeResult> {
    let pack = engine.pack();
    let mut s = Samples::new();
    for question in questions {
        let view = ctx.view(pack, &question.principal, BENCH_AGENT)?;
        let request = search_request(question);
        // With `repeat` 0 the scored call is the one timed (a reranked search, too
        // costly to run again); otherwise it warms the process for `repeat` timed runs.
        let start = Instant::now();
        let (response, _) = engine.search(&view, &request)?;
        if repeat == 0 {
            s.latencies.push(ms(start));
        }
        for _ in 0..repeat {
            let start = Instant::now();
            let _ = engine.search(&view, &request)?;
            s.latencies.push(ms(start));
        }
        let answers = resolve_all(pack, &question.answers)?;
        let returned: Vec<u32> = response
            .results
            .iter()
            .filter_map(|r| pack.resolve_id(&r.id).ok().flatten())
            .collect();
        if question.hidden {
            s.leaks += returned.iter().filter(|u| answers.contains(u)).count();
            continue;
        }
        let top5: Vec<u32> = returned.iter().take(5).copied().collect();
        s.recalls.push((question.id.clone(), recall(&answers, &top5)));
        s.recalls10
            .push(recall(&answers, &returned[..returned.len().min(10)]));
        s.doc_recalls.push(doc_recall(pack, &answers, &top5));
        s.reciprocal.push(reciprocal_rank(&answers, &returned));
        let ids: Vec<String> = response.results.iter().map(|r| r.id.clone()).collect();
        let discovery = count_tokens(&serde_json::to_string(&response)?) as usize;
        s.discovery.push(discovery as f64);
        s.tokens.push((discovery + read_top(engine, &view, &ids)?) as f64);
    }
    Ok(finish(name, s))
}

struct Samples {
    recalls: Vec<(String, f64)>,
    recalls10: Vec<f64>,
    doc_recalls: Vec<f64>,
    reciprocal: Vec<f64>,
    tokens: Vec<f64>,
    discovery: Vec<f64>,
    latencies: Vec<f64>,
    leaks: usize,
}

impl Samples {
    fn new() -> Self {
        Self {
            recalls: Vec::new(),
            recalls10: Vec::new(),
            doc_recalls: Vec::new(),
            reciprocal: Vec::new(),
            tokens: Vec::new(),
            discovery: Vec::new(),
            latencies: Vec::new(),
            leaks: 0,
        }
    }
}

/// The share of `answers` among `top`.
fn recall(answers: &BTreeSet<u32>, top: &[u32]) -> f64 {
    let top: BTreeSet<u32> = top.iter().copied().collect();
    answers.intersection(&top).count() as f64 / answers.len().max(1) as f64
}

/// 1 / rank of the first answer among the first ten results, 0 when none is there.
fn reciprocal_rank(answers: &BTreeSet<u32>, ranked: &[u32]) -> f64 {
    ranked
        .iter()
        .take(10)
        .position(|uid| answers.contains(uid))
        .map_or(0.0, |rank| 1.0 / (rank as f64 + 1.0))
}

fn doc_recall(pack: &Pack, answers: &BTreeSet<u32>, top: &[u32]) -> f64 {
    let docs_of = |uids: Vec<u32>| -> BTreeSet<u32> {
        uids.into_iter()
            .filter_map(|u| pack.atom(u).map(|a| a.doc.to_native()))
            .collect()
    };
    let wanted = docs_of(answers.iter().copied().collect());
    let got = docs_of(top.to_vec());
    wanted.intersection(&got).count() as f64 / wanted.len().max(1) as f64
}

fn finish(name: &str, samples: Samples) -> ModeResult {
    let mean = |v: &[f64]| {
        if v.is_empty() {
            0.0
        } else {
            v.iter().sum::<f64>() / v.len() as f64
        }
    };
    let mut result = summarize(
        name,
        samples.recalls,
        samples.tokens,
        samples.latencies,
        samples.leaks,
    );
    result.doc_recall_at_5 = mean(&samples.doc_recalls);
    result.recall_at_10 = mean(&samples.recalls10);
    result.mrr = mean(&samples.reciprocal);
    result.discovery_tokens = mean(&samples.discovery);
    result
}

fn summarize(
    name: &str,
    recalls: Vec<(String, f64)>,
    tokens: Vec<f64>,
    mut latencies: Vec<f64>,
    leaks: usize,
) -> ModeResult {
    let misses = recalls
        .iter()
        .filter(|(_, r)| *r < 1.0)
        .map(|(id, _)| id.clone())
        .collect();
    let recalls: Vec<f64> = recalls.into_iter().map(|(_, r)| r).collect();
    let mean = |v: &[f64]| {
        if v.is_empty() {
            0.0
        } else {
            v.iter().sum::<f64>() / v.len() as f64
        }
    };
    ModeResult {
        mode: name.to_owned(),
        recall_at_5: mean(&recalls),
        mean_tokens: mean(&tokens),
        p50_ms: percentile(&mut latencies, 50.0),
        p95_ms: percentile(&mut latencies, 95.0),
        p99_ms: percentile(&mut latencies, 99.0),
        leaks,
        misses,
        doc_recall_at_5: 0.0,
        recall_at_10: 0.0,
        mrr: 0.0,
        discovery_tokens: 0.0,
    }
}

fn run_navigation(
    engine: &Engine,
    ctx: &Ctx<'_>,
    questions: &[Question],
    repeat: usize,
) -> Result<ModeResult> {
    let pack = engine.pack();
    let mut s = Samples::new();
    let request = MapRequest {
        scope: None,
        depth: Some(3),
        budget: Some(6_000),
    };
    for question in questions {
        let view = ctx.view(pack, &question.principal, BENCH_AGENT)?;
        let (map, _) = engine.map(&view, &request)?;
        for _ in 0..repeat {
            let start = Instant::now();
            let _ = engine.map(&view, &request)?;
            s.latencies.push(ms(start));
        }
        let mut entries = Vec::new();
        flatten(&map.nodes, &mut entries);
        let wanted = content_words(&format!("{} {}", question.question, question.queries.join(" ")));
        let mut scored: Vec<(usize, usize, String)> = entries
            .iter()
            .enumerate()
            .map(|(i, (id, label))| (content_words(label).intersection(&wanted).count(), i, id.clone()))
            .filter(|(score, _, _)| *score > 0)
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let picks: Vec<String> = scored.into_iter().take(5).map(|(_, _, id)| id).collect();
        let answers = resolve_all(pack, &question.answers)?;
        let picked: Vec<u32> = picks
            .iter()
            .filter_map(|id| pack.resolve_id(id).ok().flatten())
            .collect();
        if question.hidden {
            s.leaks += entries
                .iter()
                .filter_map(|(id, _)| pack.resolve_id(id).ok().flatten())
                .filter(|u| answers.contains(u))
                .count();
            continue;
        }
        s.recalls.push((question.id.clone(), recall(&answers, &picked)));
        s.recalls10.push(recall(&answers, &picked));
        s.doc_recalls.push(doc_recall(pack, &answers, &picked));
        s.reciprocal.push(reciprocal_rank(&answers, &picked));
        let discovery = count_tokens(&serde_json::to_string(&map)?) as usize;
        s.discovery.push(discovery as f64);
        s.tokens
            .push((discovery + read_top(engine, &view, &picks)?) as f64);
    }
    Ok(finish("navigation", s))
}

fn run_grep(
    corpus_files: &BTreeMap<String, String>,
    pack: &Pack,
    questions: &[Question],
    repeat: usize,
) -> Result<ModeResult> {
    let mut recalls = Vec::new();
    let mut tokens = Vec::new();
    let mut latencies = Vec::new();
    let mut leaks = 0;
    let mut reciprocal = Vec::new();
    let mut recalls10 = Vec::new();
    let folded: BTreeMap<&String, String> = corpus_files.iter().map(|(p, t)| (p, text::fold(t))).collect();
    for question in questions {
        let terms = content_words(&question.queries.join(" "));
        let mut ranked: Vec<(usize, &String)> = Vec::new();
        for _ in 0..repeat.max(1) {
            let start = Instant::now();
            ranked = folded
                .iter()
                .map(|(path, text)| {
                    (
                        terms
                            .iter()
                            .map(|t| text.matches(t.as_str()).count())
                            .sum::<usize>(),
                        *path,
                    )
                })
                .filter(|(hits, _)| *hits > 0)
                .collect();
            ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
            latencies.push(ms(start));
        }
        let top_docs: Vec<String> = ranked
            .iter()
            .take(5)
            .map(|(_, p)| p.trim_end_matches(".md").to_owned())
            .collect();
        let answers = resolve_all(pack, &question.answers)?;
        let answer_docs: Vec<String> = answers
            .iter()
            .filter_map(|u| pack.doc_of(*u).map(|d| d.path.as_str().to_owned()))
            .collect();
        if question.hidden {
            leaks += answer_docs
                .iter()
                .filter(|d| top_docs.iter().take(3).any(|t| t == *d))
                .count();
            continue;
        }
        let found = answer_docs.iter().filter(|d| top_docs.contains(d)).count();
        let found10 = answer_docs
            .iter()
            .filter(|d| {
                ranked
                    .iter()
                    .take(10)
                    .any(|(_, p)| p.trim_end_matches(".md") == d.as_str())
            })
            .count();
        recalls10.push(found10 as f64 / answer_docs.len().max(1) as f64);
        reciprocal.push(
            ranked
                .iter()
                .take(10)
                .position(|(_, p)| answer_docs.iter().any(|d| *d == p.trim_end_matches(".md")))
                .map_or(0.0, |rank| 1.0 / (rank as f64 + 1.0)),
        );
        recalls.push((
            question.id.clone(),
            found as f64 / answer_docs.len().max(1) as f64,
        ));
        let used: usize = ranked
            .iter()
            .take(3)
            .filter_map(|(_, p)| corpus_files.get(*p))
            .map(|t| count_tokens(t) as usize)
            .sum();
        tokens.push(used as f64);
    }
    let mut result = summarize("grep+read", recalls, tokens, latencies, leaks);
    result.doc_recall_at_5 = result.recall_at_5;
    result.recall_at_10 = recalls10.iter().sum::<f64>() / recalls10.len().max(1) as f64;
    result.mrr = reciprocal.iter().sum::<f64>() / reciprocal.len().max(1) as f64;
    Ok(result)
}

fn open_engine(pack_path: &Path, factories: &[Arc<dyn RetrieverFactory>]) -> Result<Engine> {
    let pack = Pack::open(pack_path)?;
    let mut retrievers = Vec::new();
    for factory in factories {
        if let Some(r) = factory.open(&pack)? {
            retrievers.push(r);
        }
    }
    Ok(Engine::new(
        Arc::new(pack),
        retrievers,
        kiwi_core::EngineConfig::default(),
    ))
}

/// Attaches a local cross-encoder, allowed per class by the policy, and loads it.
#[cfg(feature = "fastembed")]
fn with_reranker(engine: Engine, model: &str, top_k: usize, gate: &PolicyGate) -> Result<Engine> {
    use kiwi_core::plugin::Reranker;
    let reranker = Arc::new(kiwi_rerank::LocalReranker::new(
        model,
        plugins::cache_dir().join("models"),
        std::time::Duration::from_secs(60),
    )?);
    reranker.warm()?;
    let caps = reranker.capabilities();
    let allowed = engine
        .pack()
        .data()
        .classes
        .iter()
        .map(|class| {
            let chain: Vec<kiwi_core::model::AccessDecl> = class
                .chain
                .iter()
                .map(|d| {
                    kiwi_core::model::AccessDecl::new(
                        d.label.as_str(),
                        d.groups.iter().map(|g| g.as_str().to_owned()),
                    )
                })
                .collect();
            gate.plugin_allows(reranker.name(), &caps, &chain)
        })
        .collect();
    Ok(engine.with_reranker(kiwi_core::rerank::AttachedReranker::new(reranker, top_k, allowed)))
}

#[cfg(not(feature = "fastembed"))]
fn with_reranker(_engine: Engine, _model: &str, _top_k: usize, _gate: &PolicyGate) -> Result<Engine> {
    Err(anyhow!(
        "--rerank needs a kiwi built with the `fastembed` feature"
    ))
}

fn check_leaks(
    engine: &Engine,
    ctx: &Ctx<'_>,
    principals: &Principals,
    questions: &[Question],
) -> Result<(usize, Vec<String>, Vec<String>)> {
    let pack = engine.pack();
    let mut checks = 0usize;
    let mut violations = Vec::new();
    let mut missing = Vec::new();
    let all_answers: Vec<String> = questions.iter().flat_map(|q| q.answers.clone()).collect();
    for case in &principals.principals {
        let view = ctx.view_on(pack, &case.id, &case.agent, &case.channel)?;
        let who = format!("{}/{}", case.id, case.agent);
        let forbidden: BTreeSet<u32> = resolve_all(pack, &case.must_not_see)?;
        for id in &case.must_see {
            let uid = pack
                .resolve_id(id)?
                .ok_or_else(|| anyhow!("`{id}` names no atom"))?;
            if !view.contains(uid) {
                missing.push(format!("{who} cannot see {id}"));
            }
        }
        let hidden_words: Vec<&Canary> = principals
            .canaries
            .iter()
            .filter(|c| {
                pack.resolve_id(&c.section)
                    .ok()
                    .flatten()
                    .is_some_and(|u| !view.contains(u))
            })
            .collect();
        let mut responses: Vec<(String, String, Vec<u32>)> = Vec::new();
        let mut queries: Vec<Vec<String>> = questions.iter().map(|q| q.queries.clone()).collect();
        queries.extend(principals.canaries.iter().map(|c| vec![c.word.clone()]));
        for qs in &queries {
            let (r, f) = engine.search(
                &view,
                &SearchRequest {
                    queries: qs.clone(),
                    ..Default::default()
                },
            )?;
            responses.push(("search".into(), serde_json::to_string(&r)?, f.atoms));
        }
        let (r, f) = engine.map(
            &view,
            &MapRequest {
                scope: None,
                depth: Some(8),
                budget: Some(32_000),
            },
        )?;
        responses.push(("map".into(), serde_json::to_string(&r)?, f.atoms));
        for chunk in all_answers.chunks(20) {
            let (mut r, f) = engine.read(
                &view,
                &ReadRequest {
                    ids: chunk.to_vec(),
                    level: Some(Level::L2),
                    context: Some(kiwi_core::api::ReadContext::Children),
                    ..Default::default()
                },
            )?;
            // `not_found` echoes the caller's own request (hidden and absent ids are
            // reported alike), so it is not a disclosure.
            r.not_found.clear();
            responses.push(("read".into(), serde_json::to_string(&r)?, f.atoms));
        }
        for id in all_answers.iter().take(30) {
            if let Ok((r, f)) = engine.trace(
                &view,
                &TraceRequest {
                    id: id.clone(),
                    ..Default::default()
                },
            ) {
                responses.push(("trace".into(), serde_json::to_string(&r)?, f.atoms));
            }
        }
        for (tool, text, atoms) in &responses {
            checks += 1;
            for uid in atoms {
                if !view.contains(*uid) || forbidden.contains(uid) {
                    violations.push(format!(
                        "{who} {tool} returned {}",
                        pack.atom(*uid).map(|a| a.id.as_str()).unwrap_or("?")
                    ));
                }
            }
            for id in &case.must_not_see {
                let quoted = format!("\"{id}\"");
                if text.contains(&quoted) {
                    violations.push(format!("{who} {tool} mentions {id}"));
                }
            }
            for canary in &hidden_words {
                if text.contains(&canary.word) {
                    violations.push(format!("{who} {tool} contains the word {}", canary.word));
                }
            }
        }
    }
    Ok((checks, violations, missing))
}

/// Copies the corpus's working files into the scratch directory, leaving out Git's
/// own directory (its object files are read-only, and the bench reads the working
/// files) and Kiwi's packs and state. Files are written fresh rather than copied with
/// their permissions, so restoring the corpus later can overwrite them.
fn copy_markdown_tree(from: &Path, to: &Path) -> Result<()> {
    for entry in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let target = to.join(&name);
        let kind = entry.file_type()?;
        if kind.is_dir() {
            if name == "packs" || name == "state" {
                continue;
            }
            std::fs::create_dir_all(&target)?;
            copy_markdown_tree(&path, &target)?;
        } else if kind.is_file() {
            let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            std::fs::write(&target, bytes).with_context(|| format!("writing {}", target.display()))?;
        }
    }
    Ok(())
}

/// Runs the benchmark. Exits with code 1 on a leak, or on a regression beyond the
/// tolerances of `--baseline`.
pub fn run(args: &BenchArgs) -> Result<()> {
    let baseline = match &args.baseline {
        Some(path) => Some(Baseline::read(path)?),
        None => None,
    };
    let report = measure(args)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_report(&report);
    }
    let mut failed = false;
    if !report.leak_violations.is_empty() || report.modes.iter().any(|m| m.leaks > 0 && m.mode != "grep+read")
    {
        eprintln!("kiwi bench: hidden content was returned");
        failed = true;
    }
    if let Some(baseline) = &baseline {
        let regressions = baseline.regressions(&report);
        for regression in &regressions {
            eprintln!("kiwi bench: regression: {regression}");
        }
        failed |= !regressions.is_empty();
    }
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

/// Expected results and the regressions tolerated (`kiwi bench --baseline`). Every
/// figure is optional; a mode named here must be measured.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Baseline {
    tolerance: Tolerance,
    #[serde(default)]
    modes: BTreeMap<String, ModeBaseline>,
    #[serde(default)]
    timings: TimingBaseline,
    #[serde(default)]
    scale: ScaleBaseline,
}

/// How far a result may fall behind its baseline.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Tolerance {
    /// The largest drop of recall@5, recall@10 or MRR (absolute, 0-1).
    quality: f64,
    /// A latency or time may grow to this many times its baseline...
    latency: f64,
    /// ...or by this many milliseconds, whichever is larger (timer noise on tiny values).
    latency_floor_ms: f64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModeBaseline {
    recall_at_5: Option<f64>,
    recall_at_10: Option<f64>,
    mrr: Option<f64>,
    p95_ms: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TimingBaseline {
    full_build_ms: Option<f64>,
    reload_ms: Option<f64>,
    cold_start_ms: Option<f64>,
}

/// The synthetic scale run's figures (`--synthetic`), p95 where a latency.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScaleBaseline {
    build_ms: Option<f64>,
    exact_p95_ms: Option<f64>,
    bm25_p95_ms: Option<f64>,
    vec_p95_ms: Option<f64>,
}

impl Baseline {
    fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Every result that fell behind its baseline by more than the tolerance.
    fn regressions(&self, report: &Report) -> Vec<String> {
        let mut out = Vec::new();
        let quality = |out: &mut Vec<String>, what: String, got: f64, want: Option<f64>| {
            if let Some(want) = want
                && got < want - self.tolerance.quality
            {
                out.push(format!(
                    "{what} {got:.3} < baseline {want:.3} - {}",
                    self.tolerance.quality
                ));
            }
        };
        let time = |out: &mut Vec<String>, what: String, got: f64, want: Option<f64>| {
            if let Some(want) = want {
                let limit = (want * self.tolerance.latency).max(want + self.tolerance.latency_floor_ms);
                if got > limit {
                    out.push(format!(
                        "{what} {got:.3} ms > {limit:.3} ms (baseline {want:.3} ms)"
                    ));
                }
            }
        };
        for (name, want) in &self.modes {
            let Some(got) = report.modes.iter().find(|m| &m.mode == name) else {
                out.push(format!("mode {name} was not measured"));
                continue;
            };
            quality(
                &mut out,
                format!("{name} recall@5"),
                got.recall_at_5,
                want.recall_at_5,
            );
            quality(
                &mut out,
                format!("{name} recall@10"),
                got.recall_at_10,
                want.recall_at_10,
            );
            quality(&mut out, format!("{name} MRR"), got.mrr, want.mrr);
            time(&mut out, format!("{name} p95"), got.p95_ms, want.p95_ms);
        }
        let timings = &self.timings;
        time(
            &mut out,
            "full build".into(),
            report.full_build_ms,
            timings.full_build_ms,
        );
        time(&mut out, "reload".into(), report.reload_ms, timings.reload_ms);
        time(
            &mut out,
            "cold start".into(),
            report.cold_start_ms,
            timings.cold_start_ms,
        );
        let scale = &self.scale;
        let wants_scale = scale.build_ms.is_some()
            || scale.exact_p95_ms.is_some()
            || scale.bm25_p95_ms.is_some()
            || scale.vec_p95_ms.is_some();
        match &report.scale {
            Some(got) => {
                time(&mut out, "scale build".into(), got.build_ms, scale.build_ms);
                time(
                    &mut out,
                    "scale exact p95".into(),
                    got.exact_ms.1,
                    scale.exact_p95_ms,
                );
                time(
                    &mut out,
                    "scale bm25 p95".into(),
                    got.bm25_ms.1,
                    scale.bm25_p95_ms,
                );
                time(&mut out, "scale vec p95".into(), got.vec_ms.1, scale.vec_p95_ms);
            }
            None if wants_scale => out.push("the scale run was not measured (--synthetic)".into()),
            None => {}
        }
        out
    }
}

fn measure(args: &BenchArgs) -> Result<Report> {
    let questions: Questions = serde_yaml_ng::from_slice(
        &std::fs::read(&args.questions).with_context(|| format!("reading {}", args.questions.display()))?,
    )?;
    let principals: Principals = serde_yaml_ng::from_slice(
        &std::fs::read(&args.principals).with_context(|| format!("reading {}", args.principals.display()))?,
    )?;
    // Work on a copy, so the bench measures full, incremental and no-op builds without
    // touching the corpus or its packs.
    let scratch = tempfile::tempdir()?;
    let corpus = scratch.path().join("corpus");
    std::fs::create_dir_all(&corpus)?;
    copy_markdown_tree(&args.corpus, &corpus)?;
    let snapshot = source::read(&corpus, &SourceMode::Worktree)?;
    let config = kiwi_compiler::build::load_config(&snapshot)?;
    let overrides = PluginOverrides {
        embedder: args.embedder.clone(),
        ..PluginOverrides::default()
    };
    let loaded = plugins::load(&config, &overrides)?;
    let options = BuildOptions {
        root: corpus.clone(),
        mode: SourceMode::Worktree,
        out: Some(corpus.join(PACK_DIR)),
        as_of: None,
    };
    let snapshot_for_gate = source::read(&corpus, &SourceMode::Worktree)?;
    let policy = std::str::from_utf8(
        snapshot_for_gate
            .file(&config.governance.policy)
            .ok_or_else(|| anyhow!("no policy"))?,
    )?
    .to_owned();
    let principals_text = std::str::from_utf8(
        snapshot_for_gate
            .file(&config.governance.principals)
            .ok_or_else(|| anyhow!("no principal directory"))?,
    )?
    .to_owned();
    let new_gate = || -> Result<PolicyGate> {
        Ok(PolicyGate::new(
            &policy,
            kiwi_policy::Directory::parse(&principals_text)?,
        )?)
    };
    let gate = new_gate()?;
    let indexers: Vec<&dyn kiwi_core::plugin::Indexer> = loaded.indexers.iter().map(|i| i.as_ref()).collect();
    let plugin_gate =
        |name: &str, caps: &kiwi_core::plugin::Capabilities, chain: &[kiwi_core::model::AccessDecl]| {
            gate.plugin_allows(name, caps, chain)
        };
    let build_plugins = BuildPlugins {
        indexers: indexers.clone(),
        gate: Some(&plugin_gate),
        ..BuildPlugins::default()
    };
    let start = Instant::now();
    kiwi_compiler::build(&options, &build_plugins)?;
    let full_build_ms = ms(start);
    let start = Instant::now();
    let noop = kiwi_compiler::build(&options, &build_plugins)?;
    let noop_build_ms = ms(start);
    // Change one file: append a paragraph to one section (of the fixture's ADR-006, or
    // of the corpus's first document).
    let changed = match snapshot.file("adr/adr-006-integer-money.md") {
        Some(_) => corpus.join("adr/adr-006-integer-money.md"),
        None => corpus.join(
            snapshot
                .markdown()
                .next()
                .map(|(path, _)| path.clone())
                .ok_or_else(|| anyhow!("the corpus has no Markdown document"))?,
        ),
    };
    let mut text_changed = std::fs::read_to_string(&changed)?;
    text_changed.push_str("\nAmounts in reports are formatted from minor units only at the edge.\n");
    std::fs::write(&changed, text_changed)?;
    let start = Instant::now();
    let incremental = kiwi_compiler::build(&options, &build_plugins)?;
    let incremental_build_ms = ms(start);
    // Restore and rebuild, so the benchmark runs on the corpus as written.
    copy_markdown_tree(&args.corpus, &corpus)?;
    let report = kiwi_compiler::build(&options, &build_plugins)?;
    let pack_path = PackStore::new(corpus.join(PACK_DIR))
        .current()?
        .ok_or_else(|| anyhow!("no pack"))?;

    let ctx = Ctx { gate: &gate };

    let bm25: Vec<Arc<dyn RetrieverFactory>> = loaded
        .factories
        .iter()
        .filter(|f| f.name() == "bm25")
        .cloned()
        .collect();
    let all: Vec<Arc<dyn RetrieverFactory>> = loaded.factories.clone();
    let nav_engine = open_engine(&pack_path, &[])?;
    let mut modes = Vec::new();
    let files: BTreeMap<String, String> = snapshot
        .markdown()
        .map(|(p, b)| (p.clone(), String::from_utf8_lossy(b).into_owned()))
        .collect();
    modes.push(run_grep(
        &files,
        nav_engine.pack(),
        &questions.questions,
        args.repeat,
    )?);
    modes.push(run_navigation(
        &nav_engine,
        &ctx,
        &questions.questions,
        args.repeat,
    )?);
    let mut leak_engine = None;
    if !bm25.is_empty() && nav_engine.pack().segment("bm25").is_some() {
        let engine = open_engine(&pack_path, &bm25)?;
        modes.push(run_search_mode(
            "bm25",
            &engine,
            &ctx,
            &questions.questions,
            args.repeat,
        )?);
        leak_engine = Some(engine);
    }
    let mut query_embed_ms = None;
    if all.len() > bm25.len() && nav_engine.pack().segment("vec").is_some() {
        let engine = open_engine(&pack_path, &all)?;
        modes.push(run_search_mode(
            "bm25+vec",
            &engine,
            &ctx,
            &questions.questions,
            args.repeat,
        )?);
        query_embed_ms = Some(measure_query_embedding(&engine, &questions.questions)?);
        leak_engine = Some(engine);
    }
    if let Some(model) = &args.rerank {
        // A cross-encoder costs a second or more per search on a CPU, so this mode runs
        // each question once and times that call. The leak suite keeps the pipeline
        // above: a reranker only reorders results already in the view (and reads only
        // what the policy allows it, which the federation and remote-plugin tests
        // check), so it cannot change what a caller sees.
        let engine = with_reranker(open_engine(&pack_path, &all)?, model, args.rerank_top_k, &gate)?;
        modes.push(run_search_mode(
            "bm25+vec+rerank",
            &engine,
            &ctx,
            &questions.questions,
            0,
        )?);
    }
    let leak_engine = match leak_engine {
        Some(engine) => engine,
        None => open_engine(&pack_path, &[])?,
    };
    let (leak_checks, leak_violations, missing_visibility) =
        check_leaks(&leak_engine, &ctx, &principals, &questions.questions)?;

    let mut cold = Vec::new();
    for _ in 0..args.repeat.clamp(3, 20) {
        let start = Instant::now();
        let engine = open_engine(&pack_path, &all)?;
        let view = View::everything(engine.pack());
        let _ = engine.search(
            &view,
            &SearchRequest {
                queries: vec!["ADR-005".into()],
                ..Default::default()
            },
        )?;
        cold.push(ms(start));
    }
    let first_vector_query_ms = if nav_engine.pack().segment("vec").is_some() && all.len() > bm25.len() {
        let start = Instant::now();
        let engine = open_engine(&pack_path, &all)?;
        let view = View::everything(engine.pack());
        let _ = engine.search(
            &view,
            &SearchRequest {
                queries: vec!["how are retries made safe".into()],
                ..Default::default()
            },
        )?;
        Some(ms(start))
    } else {
        None
    };
    let reload_ms = measure_reload(
        &corpus,
        &pack_path,
        &config,
        &all,
        &new_gate,
        args.repeat.clamp(3, 20),
    )?;
    let scale = match args.synthetic {
        Some(n) => Some(scale_run(n, args.repeat, args.keep_synthetic.as_deref())?),
        None => None,
    };
    Ok(Report {
        snapshot: report.snapshot,
        atoms: report.atoms as u32,
        questions: questions.questions.iter().filter(|q| !q.hidden).count(),
        modes,
        leak_checks,
        leak_violations,
        missing_visibility,
        cold_start_ms: percentile(&mut cold, 50.0),
        full_build_ms,
        incremental_build_ms,
        incremental_atoms: incremental.atoms_rebuilt,
        pack_bytes: std::fs::metadata(&pack_path)?.len(),
        reload_ms,
        noop_build_ms,
        noop_atoms: noop.atoms_rebuilt,
        query_embed_ms,
        first_vector_query_ms,
        scale,
    })
}

/// What a server does when a new pack becomes current (`kiwi serve --follow`, or a
/// `--watch` rebuild): parse the snapshot's policy and principals, open the pack with
/// its retrievers and swap it in. Median ms of `runs` reloads of a served corpus.
fn measure_reload(
    corpus: &Path,
    pack_path: &Path,
    config: &kiwi_compiler::KiwiConfig,
    factories: &[Arc<dyn RetrieverFactory>],
    new_gate: &dyn Fn() -> Result<PolicyGate>,
    runs: usize,
) -> Result<f64> {
    let state = tempfile::tempdir()?;
    let options = kiwi_mcp::ServiceOptions {
        corpus: corpus.to_owned(),
        packs: corpus.join(PACK_DIR),
        state_dir: state.path().to_owned(),
        config: config.clone(),
        audit: false,
        proposals: false,
        stdout_is_protocol: false,
        snapshot: None,
    };
    let served = kiwi_mcp::Corpus::open(
        options,
        new_gate()?,
        kiwi_mcp::CorpusPlugins {
            factories: factories.to_vec(),
            reranker: None,
        },
    )
    .map_err(|e| anyhow!(e))?;
    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        let start = Instant::now();
        served
            .replace(pack_path, new_gate()?, config.clone(), factories.to_vec())
            .map_err(|e| anyhow!(e))?;
        samples.push(ms(start));
    }
    Ok(percentile(&mut samples, 50.0))
}

fn measure_query_embedding(engine: &Engine, questions: &[Question]) -> Result<f64> {
    let view = View::everything(engine.pack());
    let mut samples = Vec::new();
    // The vector tier dominates a search that skips the exact tier; time a whole
    // bm25+vec search minus nothing: report the end-to-end planner latency instead of
    // guessing the split.
    for question in questions.iter().take(10) {
        let start = Instant::now();
        let _ = engine.search(
            &view,
            &SearchRequest {
                queries: vec![question.question.clone()],
                ..Default::default()
            },
        )?;
        samples.push(ms(start));
    }
    Ok(percentile(&mut samples, 50.0))
}

fn print_report(report: &Report) {
    println!(
        "Kiwi-Bench on snapshot {} ({} atoms, {} answerable questions)",
        report.snapshot, report.atoms, report.questions
    );
    println!();
    println!(
        "{:<16} {:>8} {:>8} {:>8} {:>6} {:>9} {:>9} {:>8} {:>8} {:>8} {:>5}",
        "mode",
        "recall@5",
        "r@10",
        "doc r@5",
        "mrr",
        "tokens/q",
        "discovery",
        "p50 ms",
        "p95 ms",
        "p99 ms",
        "leaks"
    );
    for m in &report.modes {
        println!(
            "{:<16} {:>8.3} {:>8.3} {:>8.3} {:>6.3} {:>9.0} {:>9.0} {:>8.3} {:>8.3} {:>8.3} {:>5}",
            m.mode,
            m.recall_at_5,
            m.recall_at_10,
            m.doc_recall_at_5,
            m.mrr,
            m.mean_tokens,
            m.discovery_tokens,
            m.p50_ms,
            m.p95_ms,
            m.p99_ms,
            m.leaks
        );
    }
    println!();
    println!(
        "leak tests: {} responses checked, {} violations, {} must-see failures",
        report.leak_checks,
        report.leak_violations.len(),
        report.missing_visibility.len()
    );
    for v in report
        .leak_violations
        .iter()
        .chain(&report.missing_visibility)
        .take(20)
    {
        println!("  {v}");
    }
    println!(
        "cold start (open pack + retrievers + first search by id): {:.2} ms",
        report.cold_start_ms
    );
    println!(
        "reload (policy + pack + retrievers, swapped in): {:.2} ms; pack {:.2} MB",
        report.reload_ms,
        report.pack_bytes as f64 / 1e6
    );
    if let Some(first) = report.first_vector_query_ms {
        println!("first query that needs vectors (includes loading the model): {first:.0} ms");
    }
    println!(
        "build: full {:.0} ms; no change {:.0} ms ({} atoms compiled); one file changed {:.0} ms ({} atoms compiled)",
        report.full_build_ms,
        report.noop_build_ms,
        report.noop_atoms,
        report.incremental_build_ms,
        report.incremental_atoms
    );
    if let Some(e) = report.query_embed_ms {
        println!("bm25+vec search on a raw question (includes query embedding): p50 {e:.2} ms");
    }
    if let Some(s) = &report.scale {
        println!();
        println!(
            "synthetic scale: {} atoms, pack {:.1} MB, build {:.0} ms, one-file rebuild {:.0} ms",
            s.atoms,
            s.pack_bytes as f64 / 1e6,
            s.build_ms,
            s.incremental_build_ms
        );
        println!("  cold start          {:.2} ms", s.cold_start_ms);
        println!(
            "  exact (id)          p50 {:.3}  p95 {:.3}  p99 {:.3} ms",
            s.exact_ms.0, s.exact_ms.1, s.exact_ms.2
        );
        println!(
            "  structural filter   p50 {:.3}  p95 {:.3}  p99 {:.3} ms",
            s.structural_ms.0, s.structural_ms.1, s.structural_ms.2
        );
        println!(
            "  bm25 retriever      p50 {:.3}  p95 {:.3}  p99 {:.3} ms",
            s.bm25_retriever_ms.0, s.bm25_retriever_ms.1, s.bm25_retriever_ms.2
        );
        println!(
            "  bm25 search         p50 {:.3}  p95 {:.3}  p99 {:.3} ms",
            s.bm25_ms.0, s.bm25_ms.1, s.bm25_ms.2
        );
        println!(
            "  hamming scan        p50 {:.3}  p95 {:.3}  p99 {:.3} ms",
            s.vec_scan_ms.0, s.vec_scan_ms.1, s.vec_scan_ms.2
        );
        println!(
            "  vec search (hash)   p50 {:.3}  p95 {:.3}  p99 {:.3} ms",
            s.vec_ms.0, s.vec_ms.1, s.vec_ms.2
        );
    }
}

#[cfg(all(feature = "bm25", feature = "vec"))]
fn scale_run(atoms: usize, repeat: usize, keep: Option<&Path>) -> Result<ScaleReport> {
    scale::run(atoms, repeat, keep)
}

#[cfg(not(all(feature = "bm25", feature = "vec")))]
fn scale_run(_atoms: usize, _repeat: usize, _keep: Option<&Path>) -> Result<ScaleReport> {
    Err(anyhow!("the synthetic scale run needs the bm25 and vec features"))
}

#[cfg(all(feature = "bm25", feature = "vec"))]
mod scale {
    //! A synthetic corpus for latency at scale: documents of twenty sections drawn from
    //! a fixed vocabulary with a Zipf-like distribution, built with BM25 and the hash
    //! embedder.

    use super::*;

    const VOCAB: &[&str] = &[
        "payment",
        "checkout",
        "ledger",
        "payout",
        "seller",
        "buyer",
        "order",
        "refund",
        "dispute",
        "capture",
        "saga",
        "kafka",
        "event",
        "consumer",
        "partition",
        "retry",
        "timeout",
        "idempotency",
        "key",
        "cache",
        "index",
        "search",
        "ranking",
        "latency",
        "budget",
        "token",
        "agent",
        "policy",
        "access",
        "audit",
        "review",
        "approval",
        "deprecated",
        "service",
        "database",
        "postgres",
        "schema",
        "migration",
        "backfill",
        "replica",
        "region",
        "failover",
        "alert",
        "dashboard",
        "metric",
        "release",
        "rollout",
        "flag",
        "experiment",
        "cohort",
        "notification",
        "email",
        "push",
        "quiet",
        "hours",
        "preference",
        "kyc",
        "identity",
        "document",
        "verification",
        "fraud",
        "risk",
        "score",
        "threshold",
        "limit",
        "quota",
        "rate",
        "throttle",
        "queue",
        "worker",
        "batch",
        "stream",
        "window",
        "aggregate",
        "report",
        "finance",
        "tax",
        "invoice",
        "currency",
        "exchange",
        "minor",
        "unit",
        "rounding",
        "precision",
        "contract",
        "vendor",
        "partner",
        "bank",
        "settlement",
        "reconciliation",
        "statement",
        "export",
        "import",
        "listing",
        "photo",
        "category",
        "tag",
        "glossary",
        "term",
    ];

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn word(&mut self) -> &'static str {
            // Zipf-like: squaring a uniform draw favours the head of the vocabulary.
            let u = (self.next() % 10_000) as f64 / 10_000.0;
            VOCAB[((u * u) * VOCAB.len() as f64) as usize % VOCAB.len()]
        }
        fn sentence(&mut self, words: usize) -> String {
            let mut s: Vec<&str> = (0..words).map(|_| self.word()).collect();
            s.push("system");
            let mut text = s.join(" ");
            text.push('.');
            text
        }
    }

    fn write_corpus(dir: &Path, atoms: usize) -> Result<()> {
        let docs = atoms.div_ceil(20);
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        std::fs::create_dir_all(dir.join(".kiwi"))?;
        std::fs::write(
            dir.join("KIWIOWNERS"),
            "* label=internal\nsecret/** label=confidential groups=finance\n",
        )?;
        std::fs::write(
            dir.join(".kiwi/kiwi.toml"),
            "namespace = \"synthetic\"\n[plugins.vec]\nenabled = true\nembedder = \"hash\"\n",
        )?;
        for d in 0..docs {
            let folder = if d % 10 == 0 { "secret" } else { "docs" };
            let kind = ["prd", "rfc", "adr", "runbook"][d % 4];
            std::fs::create_dir_all(dir.join(folder))?;
            let mut text = format!(
                "---\nid: SYN-{d}\ntype: {kind}\nstatus: approved\nowner: \"@team-{}\"\n---\n\n# Document {d} about {}\n\n{}\n",
                d % 7,
                rng.word(),
                rng.sentence(40)
            );
            for s in 1..20 {
                text.push_str(&format!(
                    "\n## Section {s} on {} {{#s{s}}}\n\n{} {}\n",
                    rng.word(),
                    rng.sentence(45),
                    rng.sentence(45)
                ));
            }
            std::fs::write(dir.join(folder).join(format!("doc-{d:06}.md")), text)?;
        }
        Ok(())
    }

    /// A document id the benchmark's caller may see (every tenth document is secret).
    fn visible_doc(seed: usize, atoms: usize) -> usize {
        let docs = atoms.div_ceil(20).max(2);
        let d = seed % docs;
        if d.is_multiple_of(10) { (d + 1) % docs } else { d }
    }

    fn timed(repeat: usize, mut f: impl FnMut() -> Result<()>) -> Result<(f64, f64, f64)> {
        let mut samples = Vec::with_capacity(repeat);
        for _ in 0..repeat {
            let start = Instant::now();
            f()?;
            samples.push(ms(start));
        }
        Ok((
            percentile(&mut samples, 50.0),
            percentile(&mut samples, 95.0),
            percentile(&mut samples, 99.0),
        ))
    }

    pub(super) fn run(atoms: usize, repeat: usize, keep: Option<&Path>) -> Result<ScaleReport> {
        let scratch = tempfile::tempdir()?;
        let dir = match keep {
            Some(dir) => dir.to_owned(),
            None => scratch.path().join("corpus"),
        };
        write_corpus(&dir, atoms)?;
        let snapshot = source::read(&dir, &SourceMode::Worktree)?;
        let config = kiwi_compiler::build::load_config(&snapshot)?;
        let loaded = plugins::load(
            &config,
            &PluginOverrides {
                embedder: Some("hash".into()),
                ..Default::default()
            },
        )?;
        let options = BuildOptions {
            root: dir.clone(),
            mode: SourceMode::Worktree,
            out: Some(dir.join(PACK_DIR)),
            as_of: None,
        };
        let plugins_ref = BuildPlugins {
            indexers: loaded.indexers.iter().map(|i| i.as_ref()).collect(),
            ..BuildPlugins::default()
        };
        let start = Instant::now();
        let report = kiwi_compiler::build(&options, &plugins_ref)?;
        let build_ms = ms(start);
        let touched = dir.join("docs/doc-000001.md");
        let mut text = std::fs::read_to_string(&touched)?;
        text.push_str("\nOne more sentence about payout reconciliation.\n");
        std::fs::write(&touched, text)?;
        let start = Instant::now();
        kiwi_compiler::build(&options, &plugins_ref)?;
        let incremental_build_ms = ms(start);
        let pack_path = report.pack.clone();
        let pack_path = PackStore::new(dir.join(PACK_DIR)).current()?.unwrap_or(pack_path);
        let pack_bytes = std::fs::metadata(&pack_path)?.len();

        let mut cold = Vec::new();
        for _ in 0..repeat.clamp(3, 10) {
            let start = Instant::now();
            let engine = open_engine(&pack_path, &loaded.factories)?;
            let view = View::everything(engine.pack());
            let _ = engine.search(
                &view,
                &SearchRequest {
                    queries: vec!["SYN-5".into()],
                    ..Default::default()
                },
            )?;
            cold.push(ms(start));
        }
        let engine = open_engine(&pack_path, &loaded.factories)?;
        let pack = Arc::clone(engine.pack());
        // A staff member: every internal document, no finance secrets.
        let view = View::from_classes(
            &pack,
            (0..pack.class_count()).filter(|&c| {
                pack.data()
                    .classes
                    .get(c as usize)
                    .is_some_and(|cl| cl.chain.iter().all(|d| d.label.as_str() == "internal"))
            }),
        );
        let queries = [
            "payout reconciliation bank",
            "retry timeout queue worker",
            "kyc identity verification fraud",
            "ledger currency rounding",
            "notification quiet hours",
        ];
        let mut qi = 0usize;
        let exact_ms = timed(repeat * 10, || {
            qi += 1;
            engine.search(
                &view,
                &SearchRequest {
                    queries: vec![format!("SYN-{}", visible_doc(qi * 7919, atoms))],
                    ..Default::default()
                },
            )?;
            Ok(())
        })?;
        let structural_ms = timed(repeat * 10, || {
            engine.search(
                &view,
                &SearchRequest {
                    filters: Filters {
                        doc_type: vec!["adr".into()],
                        owner: vec!["@team-3".into()],
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )?;
            Ok(())
        })?;
        let bm25_only = open_engine(
            &pack_path,
            &loaded
                .factories
                .iter()
                .filter(|f| f.name() == "bm25")
                .cloned()
                .collect::<Vec<_>>(),
        )?;
        let bm25_ms = timed(repeat * 10, || {
            qi += 1;
            bm25_only.search(
                &view,
                &SearchRequest {
                    queries: vec![queries[qi % queries.len()].into()],
                    ..Default::default()
                },
            )?;
            Ok(())
        })?;
        let bm25_factory = loaded
            .factories
            .iter()
            .find(|f| f.name() == "bm25")
            .ok_or_else(|| anyhow!("bm25 not built in"))?;
        let bm25_retriever = bm25_factory
            .open(&pack)?
            .ok_or_else(|| anyhow!("no bm25 segment"))?;
        let bm25_retriever_ms = timed(repeat * 10, || {
            qi += 1;
            bm25_retriever.search(
                queries[qi % queries.len()],
                kiwi_core::plugin::Scope {
                    visible: view.visible(),
                    candidates: view.visible(),
                },
                30,
            )?;
            Ok(())
        })?;
        let vec_retriever = {
            let (_, bytes) = pack.segment("vec").ok_or_else(|| anyhow!("no vec segment"))?;
            kiwi_vec::VecRetriever::open(bytes, Arc::new(kiwi_vec::HashEmbedder))?
        };
        let code = vec_retriever.query_code("payout reconciliation bank")?;
        let vec_scan_ms = timed(repeat * 10, || {
            let _ = vec_retriever.scan(&code, view.visible(), 64);
            Ok(())
        })?;
        let vec_only = open_engine(
            &pack_path,
            &loaded
                .factories
                .iter()
                .filter(|f| f.name() == "vec")
                .cloned()
                .collect::<Vec<_>>(),
        )?;
        let vec_ms = timed(repeat * 10, || {
            qi += 1;
            vec_only.search(
                &view,
                &SearchRequest {
                    queries: vec![queries[qi % queries.len()].into()],
                    ..Default::default()
                },
            )?;
            Ok(())
        })?;
        Ok(ScaleReport {
            atoms: pack.atom_count(),
            build_ms,
            incremental_build_ms,
            pack_bytes,
            cold_start_ms: percentile(&mut cold, 50.0),
            exact_ms,
            structural_ms,
            bm25_ms,
            bm25_retriever_ms,
            vec_scan_ms,
            vec_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn mode(name: &str, recall: f64, mrr: f64, p95_ms: f64) -> ModeResult {
        ModeResult {
            mode: name.into(),
            recall_at_5: recall,
            recall_at_10: recall,
            doc_recall_at_5: recall,
            mrr,
            discovery_tokens: 0.0,
            mean_tokens: 0.0,
            p50_ms: p95_ms,
            p95_ms,
            p99_ms: p95_ms,
            leaks: 0,
            misses: Vec::new(),
        }
    }

    fn report(modes: Vec<ModeResult>, reload_ms: f64) -> Report {
        Report {
            snapshot: "dir:test".into(),
            atoms: 1,
            questions: 1,
            modes,
            leak_checks: 0,
            leak_violations: Vec::new(),
            missing_visibility: Vec::new(),
            cold_start_ms: 1.0,
            full_build_ms: 100.0,
            incremental_build_ms: 1.0,
            incremental_atoms: 1,
            pack_bytes: 1,
            reload_ms,
            noop_build_ms: 1.0,
            noop_atoms: 0,
            query_embed_ms: None,
            first_vector_query_ms: None,
            scale: None,
        }
    }

    const BASELINE: &str = r#"
[tolerance]
quality = 0.02
latency = 3.0
latency_floor_ms = 1.0

[modes.bm25]
recall_at_5 = 0.80
mrr = 0.70
p95_ms = 0.1

[timings]
reload_ms = 10.0
"#;

    #[test]
    fn results_within_tolerance_pass() {
        let baseline: Baseline = toml::from_str(BASELINE).unwrap();
        // Quality 0.015 below, latency under the 1 ms floor, reload under 3x.
        let ok = report(vec![mode("bm25", 0.785, 0.70, 1.05)], 29.0);
        assert!(
            baseline.regressions(&ok).is_empty(),
            "{:?}",
            baseline.regressions(&ok)
        );
    }

    #[test]
    fn regressions_beyond_tolerance_are_named() {
        let baseline: Baseline = toml::from_str(BASELINE).unwrap();
        let worse = report(vec![mode("bm25", 0.77, 0.60, 1.2)], 31.0);
        let found = baseline.regressions(&worse);
        assert_eq!(found.len(), 4, "{found:?}");
        assert!(found[0].starts_with("bm25 recall@5"), "{found:?}");
        assert!(found.iter().any(|r| r.starts_with("reload")), "{found:?}");
        let missing = report(Vec::new(), 1.0);
        assert_eq!(baseline.regressions(&missing), ["mode bm25 was not measured"]);
    }

    #[test]
    fn unknown_baseline_keys_are_errors() {
        assert!(toml::from_str::<Baseline>(&BASELINE.replace("mrr = 0.70", "mmr = 0.70")).is_err());
    }
}
