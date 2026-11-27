//! Federation: one server over two corpora, each with its own namespace, policy and
//! audit log.
//!
//! The second corpus (`beta`) is the example corpus again, all of it confidential to
//! the finance group, plus one document with a canary word. Every principal in
//! `bench/principals.yaml` is checked:
//!
//! 1. Non-interference across corpora: each corpus is rewritten without what the
//!    principal may not see (a corpus they see nothing of becomes empty), and every
//!    federated search, map and trace answer must stay the same, apart from snapshots.
//! 2. A corpus the caller sees nothing of answers exactly like a namespace that does
//!    not exist.
//! 3. Ids are qualified, citations name each corpus's snapshot, bare ids resolve only
//!    when unambiguous, and each corpus audits its own atoms.

#![allow(clippy::unwrap_used)]

mod common;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use common::Corpus;
use kiwi_compiler::build::{BuildPlugins, PACK_DIR};
use kiwi_compiler::{BuildOptions, KiwiConfig, SourceMode};
use kiwi_core::plugin::{Indexer, RetrieverFactory};
use kiwi_core::{Pack, PackStore, View};
use kiwi_mcp::{CorpusPlugins, KiwiService, ServiceOptions};
use kiwi_policy::{Directory, Identity, PolicyGate};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
struct Case {
    id: String,
    agent: String,
    #[serde(default = "stdio")]
    channel: String,
}

fn stdio() -> String {
    "stdio".into()
}

#[derive(Deserialize)]
struct Principals {
    principals: Vec<Case>,
}

#[derive(Deserialize)]
struct Question {
    queries: Vec<String>,
}

#[derive(Deserialize)]
struct Questions {
    questions: Vec<Question>,
}

const CANARY: &str = "Quillfeather";

/// The indexers a build runs and the retrievers a server opens.
type Plugins = (Vec<Box<dyn Indexer>>, Vec<Arc<dyn RetrieverFactory>>);

fn plugins() -> Plugins {
    (
        vec![
            Box::new(kiwi_bm25::Bm25Indexer),
            Box::new(kiwi_vec::VecIndexer::new(Arc::new(kiwi_vec::HashEmbedder))),
        ],
        vec![
            Arc::new(kiwi_bm25::Bm25Factory::new(true)),
            Arc::new(kiwi_vec::VecFactory::new(std::env::temp_dir())),
        ],
    )
}

/// Turns a copy of the example corpus into `beta`: finance-only, with a canary.
fn make_beta(root: &Path) {
    let config = std::fs::read_to_string(root.join(".kiwi/kiwi.toml"))
        .unwrap()
        .replace("namespace = \"acme\"", "namespace = \"beta\"");
    std::fs::write(root.join(".kiwi/kiwi.toml"), config).unwrap();
    std::fs::write(root.join("KIWIOWNERS"), "* label=confidential groups=finance\n").unwrap();
    // Frontmatter `access:` would override KIWIOWNERS; the example corpus sets it on a
    // few documents, so drop those lines to keep beta finance-only throughout.
    strip_access(root);
    std::fs::write(
        root.join("treasury.md"),
        format!(
            "---\nid: TRS-1\ntype: runbook\nstatus: approved\nowner: \"@finance\"\nsummary: How treasury sweeps cash.\n---\n\n# Treasury sweeps\n\nThe {CANARY} account receives the nightly sweep of seller balances.\n"
        ),
    )
    .unwrap();
}

fn strip_access(dir: &Path) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if !path.ends_with(".kiwi") {
                strip_access(&path);
            }
        } else if path.extension().is_some_and(|e| e == "md") {
            let text = std::fs::read_to_string(&path).unwrap();
            let kept: Vec<&str> = text.lines().filter(|l| !l.starts_with("access:")).collect();
            std::fs::write(&path, kept.join("\n") + "\n").unwrap();
        }
    }
}

fn build(root: &Path) {
    let (indexers, _) = plugins();
    let options = BuildOptions {
        root: root.to_owned(),
        mode: SourceMode::Worktree,
        out: Some(root.join(PACK_DIR)),
        as_of: Some("2026-11-23".into()),
    };
    let build = BuildPlugins {
        indexers: indexers.iter().map(|i| i.as_ref()).collect(),
        ..BuildPlugins::default()
    };
    kiwi_compiler::build(&options, &build).unwrap();
}

fn gate(root: &Path) -> PolicyGate {
    let policy = std::fs::read_to_string(root.join(".kiwi/policy.cedar")).unwrap();
    let directory =
        Directory::parse(&std::fs::read_to_string(root.join(".kiwi/principals.yaml")).unwrap()).unwrap();
    PolicyGate::new(&policy, directory).unwrap()
}

fn open_corpus(root: &Path) -> kiwi_mcp::Corpus {
    let config = KiwiConfig::parse(&std::fs::read_to_string(root.join(".kiwi/kiwi.toml")).unwrap()).unwrap();
    let options = ServiceOptions {
        corpus: root.to_owned(),
        packs: root.join(PACK_DIR),
        state_dir: root.join(".kiwi/state"),
        config,
        audit: true,
        proposals: false,
        stdout_is_protocol: false,
        snapshot: None,
    };
    let (_, factories) = plugins();
    kiwi_mcp::Corpus::open(
        options,
        gate(root),
        CorpusPlugins {
            factories,
            reranker: None,
        },
    )
    .unwrap()
}

fn federation(roots: &[&Path]) -> KiwiService {
    KiwiService::federate(roots.iter().map(|root| open_corpus(root)).collect()).unwrap()
}

fn pack(root: &Path) -> Pack {
    Pack::open(PackStore::new(root.join(PACK_DIR)).current().unwrap().unwrap()).unwrap()
}

/// Removes the documents and sections `visible` excludes (as the single-corpus leak
/// test does).
fn strip(root: &Path, pack: &Pack, view: &View) {
    for doc in pack.data().docs.iter() {
        let file = root.join(format!("{}.md", doc.path));
        if !view.contains(doc.root.to_native()) {
            std::fs::remove_file(&file).unwrap();
            continue;
        }
        let first = doc.first.to_native();
        let hidden: BTreeSet<u32> = (first..first + doc.count.to_native())
            .filter(|uid| !view.contains(*uid))
            .map(|uid| pack.atom(uid).unwrap().line.to_native())
            .collect();
        if hidden.is_empty() {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap();
        let mut out = Vec::new();
        let mut skipping: Option<usize> = None;
        let mut fenced = false;
        for (index, line) in text.lines().enumerate() {
            if line.trim_start().starts_with("```") {
                fenced = !fenced;
            }
            let hashes = line.chars().take_while(|c| *c == '#').count();
            let level =
                (!fenced && hashes > 0 && hashes <= 6 && line[hashes..].starts_with(' ')).then_some(hashes);
            if let (Some(limit), Some(level)) = (skipping, level)
                && level <= limit
            {
                skipping = None;
            }
            if skipping.is_none() && hidden.contains(&(index as u32 + 1)) {
                skipping = level;
            }
            if skipping.is_none() {
                out.push(line);
            }
        }
        std::fs::write(&file, out.join("\n") + "\n").unwrap();
    }
}

fn without_snapshots(text: &str) -> Value {
    let mut value: Value = serde_json::from_str(text).unwrap_or_else(|e| panic!("{e}: {text}"));
    let object = value.as_object_mut().unwrap();
    object.remove("snapshot");
    object.remove("snapshots");
    value
}

fn call(service: &KiwiService, who: &Identity, tool: &str, args: Value) -> Result<String, String> {
    service.call(who, tool, args).map_err(|e| e.to_string())
}

#[test]
fn hidden_corpora_and_sections_cannot_change_any_answer() {
    let workspace = common::workspace();
    let principals: Principals =
        serde_yaml_ng::from_slice(&std::fs::read(workspace.join("bench/principals.yaml")).unwrap()).unwrap();
    let questions: Questions =
        serde_yaml_ng::from_slice(&std::fs::read(workspace.join("bench/questions.yaml")).unwrap()).unwrap();
    let mut queries: Vec<Vec<String>> = questions.questions.iter().map(|q| q.queries.clone()).collect();
    for word in [
        CANARY,
        "Halcyon",
        "Northwind",
        "treasury sweep",
        "TRS-1",
        "ADR-005",
    ] {
        queries.push(vec![word.to_owned()]);
    }
    let traces = [
        "acme:PRD-001",
        "beta:PRD-001",
        "acme:RFC-013",
        "beta:TRS-1",
        "TRS-1",
    ];
    let maps = [
        json!({"depth": 2, "budget": 20_000}),
        json!({"scope": "beta:", "depth": 3, "budget": 20_000}),
        json!({"scope": "acme:product", "depth": 3, "budget": 20_000}),
        json!({"scope": "product", "depth": 2, "budget": 20_000}),
    ];

    let acme = Corpus::copy();
    let beta = Corpus::copy();
    make_beta(&beta.root);
    build(&acme.root);
    build(&beta.root);
    let full = federation(&[&acme.root, &beta.root]);
    let (acme_pack, beta_pack) = (pack(&acme.root), pack(&beta.root));
    let (acme_gate, beta_gate) = (gate(&acme.root), gate(&beta.root));

    let mut compared = 0usize;
    let mut seen = BTreeSet::new();
    for case in &principals.principals {
        if !seen.insert((case.id.clone(), case.agent.clone(), case.channel.clone())) {
            continue;
        }
        let who = Identity::new(case.id.clone(), case.agent.clone(), case.channel.clone());
        let acme_view = acme_gate.view(&acme_pack, &who).unwrap();
        let beta_view = beta_gate.view(&beta_pack, &who).unwrap();
        let sees_beta = !beta_view.visible().is_empty();

        let acme_stripped = Corpus::copy();
        strip(&acme_stripped.root, &acme_pack, &acme_view);
        let beta_stripped = Corpus::copy();
        make_beta(&beta_stripped.root);
        strip(&beta_stripped.root, &beta_pack, &beta_view);
        build(&acme_stripped.root);
        build(&beta_stripped.root);
        let stripped = federation(&[&acme_stripped.root, &beta_stripped.root]);

        // Literal searches page across the corpora: the same pages and cursors, and
        // nothing of a corpus the caller cannot see.
        for literal in ["the", "Saga", CANARY] {
            let mut args = json!({ "literal": literal, "ignore_case": true, "budget": 120 });
            let mut corpora = BTreeSet::new();
            loop {
                let a = call(&full, &who, "kiwi_search", args.clone()).unwrap();
                let b = call(&stripped, &who, "kiwi_search", args.clone()).unwrap();
                assert_eq!(
                    without_snapshots(&a),
                    without_snapshots(&b),
                    "{} literal {args}",
                    case.id
                );
                if !sees_beta {
                    assert!(
                        !a.contains(CANARY) && !a.contains("beta:"),
                        "{} sees beta: {a}",
                        case.id
                    );
                }
                compared += 1;
                let page: Value = serde_json::from_str(&a).unwrap();
                for m in page["matches"].as_array().unwrap() {
                    corpora.insert(m["id"].as_str().unwrap().split(':').next().unwrap().to_owned());
                }
                match page.get("next") {
                    Some(next) => args = next.clone(),
                    None => break,
                }
            }
            if literal == CANARY && sees_beta {
                assert!(corpora.contains("beta"), "{}: {corpora:?}", case.id);
            }
        }
        for qs in &queries {
            let args = json!({ "queries": qs });
            let a = call(&full, &who, "kiwi_search", args.clone()).unwrap();
            let b = call(&stripped, &who, "kiwi_search", args).unwrap();
            assert_eq!(
                without_snapshots(&a),
                without_snapshots(&b),
                "{} search {qs:?}",
                case.id
            );
            if !sees_beta {
                assert!(
                    !a.contains(CANARY) && !a.contains("beta:"),
                    "{} sees beta: {a}",
                    case.id
                );
            }
            compared += 1;
        }
        for args in &maps {
            let a = call(&full, &who, "kiwi_map", args.clone()).unwrap();
            let b = call(&stripped, &who, "kiwi_map", args.clone()).unwrap();
            assert_eq!(
                without_snapshots(&a),
                without_snapshots(&b),
                "{} map {args}",
                case.id
            );
            compared += 1;
        }
        for id in traces {
            let args = json!({ "id": id });
            match (
                call(&full, &who, "kiwi_trace", args.clone()),
                call(&stripped, &who, "kiwi_trace", args),
            ) {
                (Ok(a), Ok(b)) => assert_eq!(
                    without_snapshots(&a),
                    without_snapshots(&b),
                    "{} trace {id}",
                    case.id
                ),
                (Err(a), Err(b)) => assert_eq!(a, b, "{} trace {id}", case.id),
                (a, b) => panic!("{} trace {id}: {a:?} vs {b:?}", case.id),
            }
            compared += 1;
        }
        // The snapshots a caller is told about are those of the corpora it can read.
        let answer: Value =
            serde_json::from_str(&call(&full, &who, "kiwi_search", json!({"queries": ["payout"]})).unwrap())
                .unwrap();
        assert_eq!(
            answer["snapshots"].get("beta").is_some(),
            sees_beta,
            "{}",
            case.id
        );
    }
    assert!(compared > 300, "compared {compared} answers");
}

#[test]
fn a_hidden_namespace_answers_like_an_absent_one() {
    let acme = Corpus::copy();
    let beta = Corpus::copy();
    make_beta(&beta.root);
    build(&acme.root);
    build(&beta.root);
    let service = federation(&[&acme.root, &beta.root]);
    let alice = Identity::new("alice", "claude-code", "stdio");
    let same = |a: Result<String, String>, b: Result<String, String>, hidden: &str, absent: &str| {
        let normalise = |r: Result<String, String>, name: &str| match r {
            Ok(text) => Ok(text.replace(name, "NS")),
            Err(text) => Err(text.replace(name, "NS")),
        };
        assert_eq!(normalise(a, hidden), normalise(b, absent));
    };
    for (hidden, absent) in [("beta", "nope")] {
        for (tool, args) in [
            (
                "kiwi_read",
                json!({"ids": [format!("{hidden}:treasury"), format!("{hidden}:adr/adr-005-idempotency-keys")]}),
            ),
            ("kiwi_trace", json!({"id": format!("{hidden}:TRS-1")})),
            ("kiwi_map", json!({"scope": format!("{hidden}:")})),
            ("kiwi_map", json!({"scope": hidden})),
            (
                "kiwi_search",
                json!({"queries": ["treasury"], "filters": {"namespace": [hidden]}}),
            ),
            (
                "kiwi_feedback",
                json!({"query": "q", "verdict": "missing", "namespace": hidden}),
            ),
        ] {
            let absent_args: Value = serde_json::from_str(&args.to_string().replace(hidden, absent)).unwrap();
            same(
                call(&service, &alice, tool, args),
                call(&service, &alice, tool, absent_args),
                hidden,
                absent,
            );
        }
    }
}

/// A finance-only corpus named `namespace`, in `hidden` mode `mode`.
fn hidden_corpus(namespace: &str, mode: &str) -> Corpus {
    let corpus = Corpus::copy();
    make_beta(&corpus.root);
    let path = corpus.root.join(".kiwi/kiwi.toml");
    let config = std::fs::read_to_string(&path)
        .unwrap()
        .replace("namespace = \"beta\"", &format!("namespace = \"{namespace}\""))
        .replace("hidden = \"invisible\"", &format!("hidden = \"{mode}\""));
    std::fs::write(&path, config).unwrap();
    build(&corpus.root);
    corpus
}

#[test]
fn a_hidden_corpus_named_like_a_path_or_in_stub_mode_changes_nothing() {
    // `adr` is also a directory of the visible corpus: the hidden corpus's name must
    // not stop `adr` from being read as a path, and its stub mode must not count its
    // hidden matches for a caller who can see nothing of it.
    let acme = Corpus::copy();
    build(&acme.root);
    let named = hidden_corpus("adr", "stub");
    let other = hidden_corpus("zzz", "invisible");
    let with_named = federation(&[&acme.root, &named.root]);
    let with_other = federation(&[&acme.root, &other.root]);
    let alice = Identity::new("alice", "claude-code", "stdio");
    for (tool, args) in [
        ("kiwi_map", json!({"scope": "adr"})),
        ("kiwi_map", json!({"scope": "adr/"})),
        ("kiwi_map", json!({"scope": "adr:"})),
        (
            "kiwi_read",
            json!({"ids": ["adr:treasury", "adr/adr-005-idempotency-keys"]}),
        ),
        ("kiwi_trace", json!({"id": "adr:TRS-1"})),
        (
            "kiwi_search",
            json!({"queries": ["treasury sweep", "TRS-1", "idempotency"]}),
        ),
    ] {
        let a = call(&with_named, &alice, tool, args.clone());
        let b = call(&with_other, &alice, tool, args.clone());
        assert_eq!(a, b, "{tool} {args}");
    }
    let map: Value =
        serde_json::from_str(&call(&with_named, &alice, "kiwi_map", json!({"scope": "adr"})).unwrap())
            .unwrap();
    assert!(map.to_string().contains("acme:adr/"), "{map}");
    let search: Value = serde_json::from_str(
        &call(
            &with_named,
            &alice,
            "kiwi_search",
            json!({"queries": ["treasury sweep"]}),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(search.get("redacted").is_none(), "{search}");
}

/// A reranker that reverses whatever it is given, to make its influence visible.
struct Reversing;

impl kiwi_core::plugin::Reranker for Reversing {
    fn name(&self) -> &str {
        "rerank"
    }
    fn capabilities(&self) -> kiwi_core::plugin::Capabilities {
        kiwi_core::plugin::Capabilities {
            content: true,
            labels: false,
            network: Vec::new(),
        }
    }
    fn score(&self, _query: &str, passages: &[String]) -> kiwi_core::Result<Vec<f32>> {
        Ok((0..passages.len()).map(|i| i as f32).collect())
    }
}

#[test]
fn a_hidden_first_corpus_sets_no_search_setting() {
    // The first corpus is one the caller sees nothing of, with its own budget, limit
    // and reranker: none of them may shape the answers about the visible corpus.
    let acme = Corpus::copy();
    build(&acme.root);
    let tuned = hidden_corpus("aaa", "invisible");
    let path = tuned.root.join(".kiwi/kiwi.toml");
    let config = std::fs::read_to_string(&path).unwrap()
        + "\n[map]\nbudget = 60\n\n[search]\nbudget = 60\nlimit = 1\n\n[read]\nbudget = 60\n";
    std::fs::write(&path, config).unwrap();
    let plain = hidden_corpus("aaa", "invisible");
    let open = |root: &Path, reranker: Option<Arc<dyn kiwi_core::plugin::Reranker>>| {
        let config =
            KiwiConfig::parse(&std::fs::read_to_string(root.join(".kiwi/kiwi.toml")).unwrap()).unwrap();
        let (_, factories) = plugins();
        kiwi_mcp::Corpus::open(
            ServiceOptions {
                corpus: root.to_owned(),
                packs: root.join(PACK_DIR),
                state_dir: root.join(".kiwi/state"),
                config,
                audit: false,
                proposals: false,
                stdout_is_protocol: false,
                snapshot: None,
            },
            gate(root),
            CorpusPlugins {
                factories,
                reranker: reranker.map(|r| (r, 10)),
            },
        )
        .unwrap()
    };
    let with_tuned = KiwiService::federate(vec![
        open(&tuned.root, Some(Arc::new(Reversing))),
        open(&acme.root, None),
    ])
    .unwrap();
    let with_plain = KiwiService::federate(vec![open(&plain.root, None), open(&acme.root, None)]).unwrap();
    let alice = Identity::new("alice", "claude-code", "stdio");
    for (tool, args) in [
        (
            "kiwi_search",
            json!({"queries": ["idempotency key retries", "checkout pricing"]}),
        ),
        ("kiwi_search", json!({"queries": ["vision"], "budget": 4000})),
        ("kiwi_map", json!({"depth": 2})),
        ("kiwi_read", json!({"ids": ["acme:product/checkout/prd"]})),
    ] {
        let a = call(&with_tuned, &alice, tool, args.clone()).map(|t| without_snapshots(&t));
        let b = call(&with_plain, &alice, tool, args.clone()).map(|t| without_snapshots(&t));
        assert_eq!(a, b, "{tool} {args}");
    }
    let answer: Value = serde_json::from_str(
        &call(
            &with_tuned,
            &alice,
            "kiwi_search",
            json!({"queries": ["idempotency key retries"]}),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        answer["budget"]["limit"],
        kiwi_core::DEFAULT_SEARCH_BUDGET,
        "{answer}"
    );
    assert!(answer["results"].as_array().unwrap().len() > 1, "{answer}");
}

#[test]
fn ids_citations_and_audit_are_per_corpus() {
    let acme = Corpus::copy();
    let beta = Corpus::copy();
    make_beta(&beta.root);
    build(&acme.root);
    build(&beta.root);
    let service = federation(&[&acme.root, &beta.root]);
    let fiona = Identity::new("fiona", "claude-code", "stdio");

    let search: Value = serde_json::from_str(
        &call(
            &service,
            &fiona,
            "kiwi_search",
            json!({"queries": ["treasury sweep", "idempotency key"]}),
        )
        .unwrap(),
    )
    .unwrap();
    let ids: Vec<&str> = search["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert!(
        ids.iter()
            .all(|id| id.starts_with("acme:") || id.starts_with("beta:")),
        "{ids:?}"
    );
    assert!(ids.contains(&"beta:treasury"), "{ids:?}");
    assert!(search["snapshot"].as_str().unwrap().starts_with("fed-"));

    // A bare id that fiona sees in both corpora is ambiguous; one only in beta is not.
    let ambiguous = call(&service, &fiona, "kiwi_read", json!({"ids": ["ADR-005"]})).unwrap_err();
    assert!(ambiguous.contains("several namespaces"), "{ambiguous}");
    let read: Value = serde_json::from_str(
        &call(
            &service,
            &fiona,
            "kiwi_read",
            json!({"ids": ["TRS-1", "acme:ADR-005"]}),
        )
        .unwrap(),
    )
    .unwrap();
    let atoms = read["atoms"].as_array().unwrap();
    assert_eq!(atoms[0]["id"], "beta:treasury");
    let beta_snapshot = pack(&beta.root).snapshot().to_owned();
    assert_eq!(
        atoms[0]["cite"],
        format!("kiwi://atom/beta:treasury@{beta_snapshot}")
    );
    assert!(atoms[1]["id"].as_str().unwrap().starts_with("acme:adr/adr-005"));
    let text = service
        .read_resource(&fiona, &format!("kiwi://atom/beta:treasury@{beta_snapshot}"))
        .unwrap();
    assert!(text.contains(CANARY));

    // A trace stays inside its corpus and qualifies every node.
    let trace: Value =
        serde_json::from_str(&call(&service, &fiona, "kiwi_trace", json!({"id": "beta:PRD-001"})).unwrap())
            .unwrap();
    assert!(
        trace["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|n| n["id"].as_str().unwrap().starts_with("beta:"))
    );

    // Each corpus's audit log holds its own records, with its namespace.
    drop(service);
    for (root, namespace) in [(&acme.root, "acme"), (&beta.root, "beta")] {
        let log = std::fs::read_to_string(root.join(".kiwi/state/audit.log")).unwrap();
        assert!(log.lines().count() >= 3, "{namespace}: {log}");
        assert!(
            log.lines()
                .all(|l| l.contains(&format!("\"namespace\":\"{namespace}\"")))
        );
        assert!(kiwi_policy::audit::verify(&root.join(".kiwi/state/audit.log"), None).is_ok());
    }
    // Each record names its own corpus's atoms by their unqualified stable ids; a call
    // refused before any corpus answered is recorded in each.
    let records = |root: &Path| -> Vec<Value> {
        std::fs::read_to_string(root.join(".kiwi/state/audit.log"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    };
    let (acme_log, beta_log) = (records(&acme.root), records(&beta.root));
    let of = |log: &[Value], tool: &str, status: &str| -> Vec<Value> {
        log.iter()
            .filter(|r| r["tool"] == tool && r["status"] == status)
            .cloned()
            .collect()
    };
    assert_eq!(of(&acme_log, "kiwi_read", "refused").len(), 1, "the ambiguous id");
    assert_eq!(of(&beta_log, "kiwi_read", "refused").len(), 1, "the ambiguous id");
    let acme_read = &of(&acme_log, "kiwi_read", "ok")[0];
    let beta_read = &of(&beta_log, "kiwi_read", "ok")[0];
    assert_eq!(acme_read["ids"][0], "adr/adr-005-idempotency-keys");
    assert_eq!(beta_read["ids"], json!(["treasury"]));
    assert_eq!(beta_read["docs"], json!(["treasury"]));
    assert_eq!(
        (&beta_read["results"], &acme_read["results"]),
        (&json!(2), &json!(2)),
        "the call's results, on each corpus's record"
    );
    // One call leaves a record in each corpus it consulted, every one with the call's
    // id; no two calls share one.
    assert_eq!(acme_read["call"], beta_read["call"]);
    assert_eq!(
        of(&acme_log, "kiwi_read", "refused")[0]["call"],
        of(&beta_log, "kiwi_read", "refused")[0]["call"]
    );
    for log in [&acme_log, &beta_log] {
        let calls: Vec<&str> = log.iter().map(|r| r["call"].as_str().unwrap()).collect();
        assert!(calls.iter().all(|c| c.len() == 32), "{calls:?}");
        let distinct: std::collections::BTreeSet<&str> = calls.iter().copied().collect();
        assert_eq!(distinct.len(), calls.len(), "{calls:?}");
    }
    assert_ne!(
        acme_read["call"],
        of(&acme_log, "kiwi_read", "refused")[0]["call"]
    );
    assert_eq!(of(&beta_log, "resource", "ok")[0]["ids"], json!(["treasury"]));
    assert!(of(&acme_log, "resource", "ok").is_empty());
    for record in acme_log.iter().chain(&beta_log) {
        for id in record["ids"].as_array().into_iter().flatten() {
            let id = id.as_str().unwrap();
            assert!(!id.starts_with("acme:") && !id.starts_with("beta:"), "{id}");
        }
    }
}

#[test]
fn duplicate_namespaces_are_refused() {
    let acme = Corpus::copy();
    let again = Corpus::copy();
    build(&acme.root);
    build(&again.root);
    let error = KiwiService::federate(vec![open_corpus(&acme.root), open_corpus(&again.root)]).unwrap_err();
    assert!(error.to_string().contains("namespace `acme`"), "{error}");
}

/// A reranker on this machine: the example policy lets local plugins read everything.
#[cfg(feature = "rerank")]
struct LocalByLength;

#[cfg(feature = "rerank")]
impl kiwi_core::plugin::Reranker for LocalByLength {
    fn name(&self) -> &str {
        "rerank"
    }
    fn capabilities(&self) -> kiwi_core::plugin::Capabilities {
        kiwi_core::plugin::Capabilities {
            content: true,
            labels: false,
            network: Vec::new(),
        }
    }
    fn score(&self, _query: &str, passages: &[String]) -> kiwi_core::Result<Vec<f32>> {
        Ok(passages.iter().map(|p| p.len() as f32).collect())
    }
}

#[cfg(feature = "rerank")]
#[test]
fn the_lead_reranker_reads_each_corpus_only_as_its_policy_allows() {
    let server = common::MockServer::start(|r| {
        let body: Value = serde_json::from_str(&r.body).unwrap();
        let n = body["documents"].as_array().unwrap().len();
        let results: Vec<Value> = (0..n)
            .map(|i| json!({"index": i, "relevance_score": i}))
            .collect();
        (200, json!({ "results": results }).to_string())
    });
    let acme = Corpus::copy();
    let beta = Corpus::copy();
    make_beta(&beta.root);
    build(&acme.root);
    build(&beta.root);
    let with = |root: &Path, reranker: Arc<dyn kiwi_core::plugin::Reranker>| {
        let config =
            KiwiConfig::parse(&std::fs::read_to_string(root.join(".kiwi/kiwi.toml")).unwrap()).unwrap();
        let (_, factories) = plugins();
        kiwi_mcp::Corpus::open(
            ServiceOptions {
                corpus: root.to_owned(),
                packs: root.join(PACK_DIR),
                state_dir: root.join(".kiwi/state"),
                config,
                audit: false,
                proposals: false,
                stdout_is_protocol: false,
                snapshot: None,
            },
            gate(root),
            CorpusPlugins {
                factories,
                reranker: Some((reranker, 10)),
            },
        )
        .unwrap()
    };
    let remote = Arc::new(
        kiwi_rerank::HttpReranker::new(&kiwi_rerank::HttpSettings {
            url: format!("{}/rerank", server.url),
            format: kiwi_rerank::ApiFormat::Cohere,
            model: "m".into(),
            api_key_env: None,
            timeout: std::time::Duration::from_secs(5),
        })
        .unwrap(),
    );
    let service = KiwiService::federate(vec![
        with(&acme.root, remote),
        with(&beta.root, Arc::new(LocalByLength)),
    ])
    .unwrap();
    let fiona = Identity::new("fiona", "claude-code", "stdio");
    let answer = call(
        &service,
        &fiona,
        "kiwi_search",
        json!({"queries": ["treasury sweep account", "glossary payout settlement"]}),
    )
    .unwrap();
    assert!(answer.contains("beta:treasury"), "{answer}");
    let sent: Vec<String> = server
        .requests()
        .iter()
        .flat_map(|r| {
            let body: Value = serde_json::from_str(&r.body).unwrap();
            serde_json::from_value::<Vec<String>>(body["documents"].clone()).unwrap()
        })
        .collect();
    assert!(!sent.is_empty());
    // Beta is confidential and its policy keeps network plugins to public content.
    assert!(sent.iter().all(|p| !p.contains(CANARY)), "{sent:?}");
    assert!(
        sent.iter()
            .all(|p| p.starts_with("GLOSSARY") || p.starts_with("VISION")),
        "{sent:?}"
    );
}

#[test]
fn an_audit_failure_in_a_hidden_corpus_is_neither_fatal_nor_revealed() {
    let acme = Corpus::copy();
    let beta = Corpus::copy();
    make_beta(&beta.root);
    build(&acme.root);
    build(&beta.root);
    let service = federation(&[&acme.root, &beta.root]);
    let alice = Identity::new("alice", "claude-code", "stdio");
    let fiona = Identity::new("fiona", "claude-code", "stdio");
    let ask = json!({"queries": ["idempotency key"]});
    let before = call(&service, &alice, "kiwi_search", ask.clone()).unwrap();
    // Beta's log can no longer be extended (a forged line ends it).
    let log = beta.root.join(".kiwi/state/audit.log");
    let mut text = std::fs::read_to_string(&log).unwrap();
    text.push_str("forged\n");
    std::fs::write(&log, text).unwrap();
    // Alice, who sees nothing of beta, is answered as before, by every tool...
    let cached = call(&service, &alice, "kiwi_search", ask.clone()).unwrap();
    assert_eq!(cached, before);
    let fresh = call(
        &service,
        &alice,
        "kiwi_search",
        json!({"queries": ["retry budget"]}),
    );
    assert!(fresh.is_ok(), "{fresh:?}");
    for (tool, args) in [
        ("kiwi_map", json!({})),
        ("kiwi_read", json!({"ids": ["acme:ADR-005"]})),
        ("kiwi_trace", json!({"id": "acme:ADR-005"})),
        ("kiwi_feedback", json!({"query": "q", "verdict": "missing"})),
    ] {
        assert!(call(&service, &alice, tool, args.clone()).is_ok(), "{tool}");
    }
    // ...while fiona, who reads beta, gets an internal error: her answer cannot be audited.
    let refused = call(&service, &fiona, "kiwi_search", ask).unwrap_err();
    assert!(refused.contains("internal error"), "{refused}");
}
