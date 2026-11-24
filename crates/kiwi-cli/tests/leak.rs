//! Leak tests for every principal in `bench/principals.yaml`.
//!
//! 1. Disclosure: no response of any tool names an atom outside the caller's view or a
//!    `must_not_see` atom, and no response contains a canary word from a hidden section.
//! 2. Non-interference: for each principal, the corpus is rewritten without everything
//!    that principal may not see. Every search, map, read, trace and proposal answer
//!    must then be byte-identical (apart from snapshot and branch ids) to the answer on
//!    the full corpus — same atoms, same order, same scores, same snippets, same lint
//!    findings, same refusals. Hidden content cannot shift a single byte of what the
//!    principal receives.
//! 3. The audit records of those calls name the same atoms and documents on both
//!    corpora, with the same status, result count and budget: a record never names an
//!    atom or a document hidden from its caller.
//!
//! The corpus is the example corpus plus adversarial documents: access on a root
//! heading, frontmatter behind a byte order mark, fences with trailing whitespace, CRLF,
//! setext and indented headings, `KIWIOWNERS` rules naming a bare directory and a
//! directory with a trailing slash at any depth, a hidden
//! title equal to a visible id, a hidden document id equal to a visible term, a hidden
//! amendment of a visible ADR, a hidden
//! glossary, and a short visible section whose only child is hidden. Who may read each
//! adversarial section is written down by hand, so a misread declaration is caught even
//! when it is misread the same way on both sides.

#![allow(clippy::unwrap_used)]

mod common;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use common::Corpus;
use kiwi_compiler::build::{BuildPlugins, PACK_DIR, load_owners};
use kiwi_compiler::{BuildOptions, KiwiConfig, SourceMode};
use kiwi_core::api::{Level, MapRequest, ReadRequest, SearchRequest, TraceRequest};
use kiwi_core::plugin::{Indexer, RetrieverFactory};
use kiwi_core::{Engine, EngineConfig, Pack, PackStore};
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

type Plugins = (Vec<Box<dyn Indexer>>, Vec<Arc<dyn RetrieverFactory>>);

fn plugins() -> Plugins {
    #[allow(unused_mut)]
    let mut indexers: Vec<Box<dyn Indexer>> = Vec::new();
    #[allow(unused_mut)]
    let mut factories: Vec<Arc<dyn RetrieverFactory>> = Vec::new();
    #[cfg(feature = "bm25")]
    {
        indexers.push(Box::new(kiwi_bm25::Bm25Indexer));
        factories.push(Arc::new(kiwi_bm25::Bm25Factory::new(true)));
    }
    #[cfg(feature = "vec")]
    {
        indexers.push(Box::new(kiwi_vec::VecIndexer::new(Arc::new(
            kiwi_vec::HashEmbedder,
        ))));
        factories.push(Arc::new(kiwi_vec::VecFactory::new(std::env::temp_dir())));
    }
    (indexers, factories)
}

fn engine(root: &Path) -> Engine {
    let (indexers, factories) = plugins();
    let options = BuildOptions {
        root: root.to_owned(),
        mode: SourceMode::Auto,
        out: Some(root.join(PACK_DIR)),
        as_of: Some("2026-11-23".into()),
    };
    let build = BuildPlugins {
        indexers: indexers.iter().map(|i| i.as_ref()).collect(),
        ..BuildPlugins::default()
    };
    kiwi_compiler::build(&options, &build).unwrap();
    let path = PackStore::new(root.join(PACK_DIR)).current().unwrap().unwrap();
    let pack = Pack::open(path).unwrap();
    let retrievers = factories.iter().filter_map(|f| f.open(&pack).unwrap()).collect();
    Engine::new(Arc::new(pack), retrievers, EngineConfig::default())
}

fn gate(root: &Path) -> PolicyGate {
    let policy = std::fs::read_to_string(root.join(".kiwi/policy.cedar")).unwrap();
    let directory =
        Directory::parse(&std::fs::read_to_string(root.join(".kiwi/principals.yaml")).unwrap()).unwrap();
    PolicyGate::new(&policy, directory).unwrap()
}

/// The adversarial documents, with the principals who may read each canary word.
const CANARIES: [(&str, &str, &[&str]); 11] = [
    ("Quillfeather", "acquisitions/term-sheet", &["erin"]),
    ("Ravenbrook", "teams/deals/pipeline", &["erin"]),
    ("Marrowind", "adr/adr-090-board-vote", &["erin"]),
    ("Bramblequartz", "product/bom-notes", &["fiona", "erin"]),
    ("Wrenfold", "product/fence-notes", &["erin"]),
    ("Gorsebeck", "product/crlf-notes", &["fiona", "erin"]),
    (
        "Thistlegrove",
        "product/checkout/notes#margin-notes",
        &["fiona", "erin"],
    ),
    (
        "Hollowmere",
        "product/checkout/notes#budget-line",
        &["fiona", "erin"],
    ),
    ("Pinecastle", "finance/terms#burn-rate", &["fiona", "erin"]),
    (
        "Quernstone",
        "product/checkout/steps#hidden-step",
        &["fiona", "erin"],
    ),
    (
        "Larkspurrow",
        "product/checkout/notes#rollout",
        &["alice", "priya", "fiona", "erin"],
    ),
];

/// Whether `principal` through `agent` may read a canary of `audience`. Every canary
/// but the internal `Larkspurrow` is confidential or restricted: the summarizer's grant
/// stops at internal, and the untrusted agent reads only public content.
fn may_read(principal: &str, agent: &str, word: &str, audience: &[&str]) -> bool {
    let agent_reaches = match agent {
        "claude-code" | "cursor" => true,
        "summarizer" => word == "Larkspurrow",
        _ => false,
    };
    agent_reaches && audience.contains(&principal)
}

fn filler(word: &str) -> String {
    format!("{word} appears here so that a leak of this section is easy to spot. ").repeat(8)
}

/// Adds the adversarial documents to a copy of the corpus.
fn adversarial(root: &Path) {
    let write = |path: &str, text: String| {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
    };
    let owners = std::fs::read_to_string(root.join("KIWIOWNERS")).unwrap();
    std::fs::write(
        root.join("KIWIOWNERS"),
        owners + "acquisitions     label=restricted groups=exec\ndeals/           label=restricted groups=exec\n",
    )
    .unwrap();
    write(
        "acquisitions/term-sheet.md",
        format!(
            "---\nid: Saga\ntitle: ADR-005\nowner: \"@corp-dev\"\nsummary: The Quillfeather term sheet.\n---\n{}\n",
            filler("Quillfeather")
        ),
    );
    // `deals/` names a directory called deals anywhere, as in CODEOWNERS.
    write(
        "teams/deals/pipeline.md",
        format!(
            "---\nid: DEALS-1\nowner: \"@corp-dev\"\nsummary: The deal pipeline.\n---\n# Deal pipeline\n\n{}\n",
            filler("Ravenbrook")
        ),
    );
    write(
        "adr/adr-090-board-vote.md",
        format!(
            "---\nid: ADR-090\ntype: adr\nstatus: draft\nowner: \"@exec\"\nsummary: A board vote.\namends: [ADR-005]\n---\n# Board vote {{#board-vote access=exec}}\n\n{}\n\n## Outcome {{#outcome}}\n\n{}\n",
            filler("Marrowind"),
            filler("Marrowind")
        ),
    );
    write(
        "product/bom-notes.md",
        format!(
            "\u{feff}---\nid: BOM-1\naccess: finance\nowner: \"@team-finance\"\nsummary: Notes.\n---\n# BOM notes\n\n{}\n",
            filler("Bramblequartz")
        ),
    );
    write(
        "product/fence-notes.md",
        format!(
            "--- \nid: FENCE-1\naccess: legal\nowner: \"@legal\"\nsummary: Notes.\n---\t\n# Fence notes\n\n{}\n",
            filler("Wrenfold")
        ),
    );
    write(
        "product/crlf-notes.md",
        format!(
            "---\r\nid: CRLF-1\r\naccess: finance\r\nowner: \"@team-finance\"\r\nsummary: Notes.\r\n---\r\n# CRLF notes\r\n\r\n{}\r\n",
            filler("Gorsebeck")
        ),
    );
    write(
        "product/checkout/notes.md",
        format!(
            "---\nid: CHK-NOTES\nowner: \"@team-checkout\"\nsummary: Working notes on checkout.\n---\n# Checkout notes\n\n{}\n\n## Overview\n\nA short overview.\n\n### Margin {{#margin-notes access=finance}}\n\n{}\n\nBudget line {{#budget-line access=finance}}\n-----------\n\n{}\n\n  ## Rollout {{#rollout}}\n\n{}\n",
            "The checkout working notes collect what the team learns while shipping the new basket flow. "
                .repeat(4),
            filler("Thistlegrove"),
            filler("Hollowmere"),
            filler("Larkspurrow")
        ),
    );
    // A hidden ATX section ends a paragraph without a blank line, and a setext section
    // follows it: the cut must not join the paragraph to that heading. The hidden
    // section also defines a link reference and a footnote a visible title names.
    write(
        "product/checkout/titles.md",
        format!(
            "---\nid: CHK-TITLES\nowner: \"@team-checkout\"\nsummary: Titles.\n---\n# Checkout titles\n\n{}\n\n## Title A {{#title-a}}\n\n{}\n## Payroll {{#payroll access=finance}}\n\n{}\n\n[ledger]: https://example.com/ledger\n\n[^audit]: Checked by the auditors.\n\nTitle B {{#title-b}}\n-------\n\n{}\n\n## Title C, see [ledger] and [^audit] {{#title-c}}\n\n{}\n",
            "The checkout titles name each step the way the receipt prints it. ".repeat(4),
            "Title A is what the basket shows before payment, as the receipt prints it. ".repeat(4),
            filler("Pinecastle"),
            "Title B is what the receipt shows once the payment went through. ".repeat(4),
            "Title C names a reference that only a hidden section defines. ".repeat(4)
        ),
    );
    // A visible section followed by an indented hidden heading, which a body ending in
    // a list would take in.
    write(
        "product/checkout/steps.md",
        format!(
            "---\nid: CHK-STEPS\nowner: \"@team-checkout\"\nsummary: Checkout steps.\n---\n# Checkout steps\n\n{}\n\n## Steps {{#steps}}\n\n{}\n\n  ## Hidden step {{#hidden-step access=finance}}\n\n{}\n",
            "The checkout steps list what the basket flow does from the first click to the receipt. "
                .repeat(4),
            "Each step of the basket flow is listed here with the service that owns it and its timeout. "
                .repeat(4),
            filler("Quernstone")
        ),
    );
    write(
        "finance/terms.md",
        format!(
            "---\ntype: glossary\nowner: \"@team-finance\"\nsummary: Finance terms.\n---\n# Finance terms\n\n## Burn rate {{#burn-rate}}\n\nAliases: runway burn\n\nHow fast cash goes. {}\n",
            filler("Pinecastle")
        ),
    );
}

/// Commits the work tree as the first commit of a new repository.
fn commit(root: &Path) {
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["add", "-A"],
        vec![
            "-c",
            "user.email=t@test",
            "-c",
            "user.name=T",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    ] {
        let status = std::process::Command::new("git")
            .args(&args)
            .current_dir(root)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }
}

/// The adversarial corpus, committed.
fn full_corpus() -> Corpus {
    let corpus = Corpus::copy();
    adversarial(&corpus.root);
    commit(&corpus.root);
    corpus
}

/// The adversarial corpus without everything `identity` may not read, cut by the
/// compiler's own section spans, committed.
fn stripped_corpus(gate: &PolicyGate, identity: &Identity) -> Corpus {
    let corpus = Corpus::copy();
    adversarial(&corpus.root);
    let root = &corpus.root;
    let snapshot = kiwi_compiler::source::read(root, &SourceMode::Worktree).unwrap();
    let config = kiwi_compiler::build::load_config(&snapshot).unwrap();
    let owners = load_owners(&snapshot).unwrap();
    let files: Vec<(String, Vec<u8>)> = snapshot
        .markdown()
        .map(|(path, bytes)| (path.clone(), bytes.clone()))
        .collect();
    for (path, bytes) in files {
        let text = String::from_utf8(bytes).unwrap();
        let doc =
            kiwi_compiler::compile::compile_doc(&path, &text, &owners.defaults(&path), &config).unwrap();
        let readable = |chain: &[kiwi_core::model::AccessDecl]| gate.allows_chain(identity, chain);
        if !readable(&doc.sections[0].chain).unwrap() {
            std::fs::remove_file(root.join(&path)).unwrap();
            continue;
        }
        let stripped = kiwi_compiler::propose::strip_hidden(&text, &doc.sections, readable).unwrap();
        // The twin parses as the visible part of the real document: same sections, same
        // titles, levels and parents.
        let defaults = owners.defaults(&path);
        let (_, real) =
            kiwi_compiler::compile::compile_with_outline(&path, &text, &defaults, &config).unwrap();
        let (_, twin) =
            kiwi_compiler::compile::compile_with_outline(&path, &stripped, &defaults, &config).unwrap();
        let seen: Vec<bool> = real.iter().map(|s| readable(&s.chain).unwrap()).collect();
        assert!(
            kiwi_compiler::propose::view_holds(&real, &seen, &twin),
            "{path}: the twin parses differently\n{stripped}"
        );
        if stripped != text {
            std::fs::write(root.join(&path), stripped).unwrap();
        }
    }
    commit(root);
    corpus
}

fn service(root: &Path) -> KiwiService {
    let config = KiwiConfig::parse(&std::fs::read_to_string(root.join(".kiwi/kiwi.toml")).unwrap()).unwrap();
    let options = ServiceOptions {
        corpus: root.to_owned(),
        packs: root.join(PACK_DIR),
        state_dir: root.join(".kiwi/state"),
        config,
        audit: true,
        proposals: true,
        stdout_is_protocol: false,
        snapshot: None,
    };
    let (_, factories) = plugins();
    let plugins = CorpusPlugins {
        factories,
        reranker: None,
    };
    KiwiService::open(options, gate(root), plugins).unwrap()
}

/// The records of a corpus's audit log.
fn audit_records(root: &Path) -> Vec<Value> {
    match std::fs::read_to_string(root.join(".kiwi/state/audit.log")) {
        Ok(text) => text.lines().map(|l| serde_json::from_str(l).unwrap()).collect(),
        Err(_) => Vec::new(),
    }
}

/// A proposal's answer without what names the commit (snapshot, id, branch, link).
fn proposal_answer(service: &KiwiService, identity: &Identity, request: &Value) -> Result<Value, String> {
    match service.call(identity, "kiwi_propose", request.clone()) {
        Ok(text) => {
            let mut value: Value = serde_json::from_str(&text).unwrap();
            let object = value.as_object_mut().unwrap();
            for key in ["snapshot", "id", "branch", "link"] {
                object.remove(key);
            }
            Ok(value)
        }
        Err(error) => Err(error.to_string()),
    }
}

/// Proposals that touch hidden content: create, case-variant, delete and set at a
/// hidden path, edit a document with hidden sections and a hidden section, link to and
/// supersede hidden documents, reuse a hidden id and a hidden term, change access; and
/// on documents with hidden sections: headings and link definitions in a body, a body
/// ending in a list before an indented hidden heading, a deletion, a whole-file change
/// and an access change.
fn proposals() -> Vec<Value> {
    let change = |change: Value| serde_json::json!({"changes": [change], "rationale": "r"});
    let new_adr = "---\nid: ADR-091\ntype: adr\nstatus: approved\nowner: \"@team-platform\"\nsummary: Retries.\nsupersedes: [FIN-001, Saga, BOM-1, ADR-090]\n---\n# Retries\n\nSee [budget](../finance/budget-2027.md), [notes](../product/bom-notes.md), [sheet](../acquisitions/term-sheet.md#x), [vote](adr-090-board-vote.md), ADR-090, BOM-1 and [ADR-005](adr-005-idempotency-keys.md). Quillfeather.\n";
    let same_id =
        "---\nid: FIN-001\nowner: \"@x\"\nsummary: S.\n---\n# Same id\n\nA document that reuses an id.\n";
    let glossary = "---\ntype: glossary\nowner: \"@x\"\nsummary: S.\n---\n# Ops terms\n\n## Burn rate {#burn-rate}\n\nAliases: runway burn\n\nHow fast cash goes.\n";
    vec![
        change(
            serde_json::json!({"path": "product/bom-notes.md", "content": "# Mine\n\nNotes of my own.\n"}),
        ),
        change(
            serde_json::json!({"path": "Product/BOM-notes.md", "content": "# Mine\n\nNotes of my own.\n"}),
        ),
        change(serde_json::json!({"path": "acquisitions/term-sheet.md", "content": "# Mine\n"})),
        change(serde_json::json!({"path": "product/bom-notes.md"})),
        change(serde_json::json!({"path": "product/bom-notes.md", "set": {"status": "approved"}})),
        change(
            serde_json::json!({"section": "product/checkout/notes", "content": "A new introduction to the checkout notes."}),
        ),
        change(
            serde_json::json!({"section": "product/checkout/notes#rollout", "content": "Rollout starts in May."}),
        ),
        change(serde_json::json!({"section": "product/checkout/notes#margin-notes", "content": "x"})),
        change(
            serde_json::json!({"section": "ADR-005", "content": "Every write API takes an idempotency key."}),
        ),
        change(serde_json::json!({"section": "Saga", "content": "x"})),
        change(serde_json::json!({"path": "product/checkout/notes.md", "set": {"status": "approved"}})),
        change(serde_json::json!({"path": "adr/adr-091-retries.md", "content": new_adr})),
        change(serde_json::json!({"path": "adr/adr-092-same-id.md", "content": same_id})),
        change(serde_json::json!({"path": "ops/terms.md", "content": glossary})),
        change(
            serde_json::json!({"path": "adr/adr-001-postgres.md", "set": {"access": {"label": "public"}}}),
        ),
        change(
            serde_json::json!({"section": "product/checkout/notes#overview", "content": "A short overview.\n\n### Added {#added}\n\nA new subsection."}),
        ),
        change(
            serde_json::json!({"section": "product/checkout/notes#overview", "content": "See [the PRD].\n\n[the PRD]: prd.md"}),
        ),
        change(
            serde_json::json!({"section": "product/checkout/notes#overview", "content": "Overview:\n\n- one\n- two"}),
        ),
        change(
            serde_json::json!({"section": "product/checkout/steps#steps", "content": "Steps:\n\n- add to basket\n- pay"}),
        ),
        change(serde_json::json!({"path": "product/checkout/notes.md"})),
        change(
            serde_json::json!({"path": "product/checkout/notes.md", "content": "---\nid: CHK-NOTES\nowner: \"@team-checkout\"\nsummary: Working notes on checkout.\n---\n# Checkout notes\n\nRewritten notes.\n\n## Overview\n\nA rewritten overview.\n\n- with a list\n\n  ## Rollout {#rollout}\n\nRollout starts in May.\n"}),
        ),
        change(
            serde_json::json!({"path": "product/checkout/notes.md", "content": "---\nid: CHK-NOTES\nowner: \"@team-checkout\"\nsummary: Working notes on checkout.\n---\n# Checkout notes\n\nRewritten notes.\n\n## Overview\n\nA rewritten overview.\n\n## Rollout {#rollout}\n\nRollout starts in May.\n"}),
        ),
        change(
            serde_json::json!({"path": "product/checkout/notes.md", "set": {"access": {"label": "public"}}}),
        ),
        change(
            serde_json::json!({"path": "product/checkout/steps.md", "set": {"access": {"label": "public"}}}),
        ),
        // Probes of hidden sections: their anchors and titles, a bad value, a demotion
        // under their level, a block left open where they are put back, and several
        // changes at once.
        change(
            serde_json::json!({"section": "product/checkout/notes#overview", "content": "Mine.\n\n### Margin {#margin-notes}\n\nMine too."}),
        ),
        change(
            serde_json::json!({"section": "product/checkout/notes#overview", "content": "Mine.\n\n### Salaries\n\nMine too."}),
        ),
        change(
            serde_json::json!({"section": "product/checkout/steps#steps", "content": "Mine.\n\n## Hidden step\n\nMine too."}),
        ),
        change(serde_json::json!({"path": "product/checkout/notes.md", "set": {"status": "bogus"}})),
        change(
            serde_json::json!({"section": "product/checkout/titles#title-b", "content": "Title B, rewritten."}),
        ),
        change(
            serde_json::json!({"section": "product/checkout/titles#title-c", "content": "Title C, rewritten."}),
        ),
        change(
            serde_json::json!({"section": "product/checkout/notes#overview", "content": "Text.\n\n# Top {#top}\n\nA new top section."}),
        ),
        change(
            serde_json::json!({"path": "product/checkout/steps.md", "content": "---\nid: CHK-STEPS\n---\n# Checkout steps\n\n## Steps {#steps}\n\nSteps.\n\n```\nan open fence\n"}),
        ),
        change(
            serde_json::json!({"path": "product/checkout/steps.md", "content": "---\nid: CHK-STEPS\n---\n# Checkout steps\n\n#### Steps {#steps}\n\nSteps, demoted.\n"}),
        ),
        serde_json::json!({"changes": [
            {"section": "product/checkout/notes#overview", "content": "Mine.\n\n### Margin {#margin-notes}\n\nMine too."},
            {"path": "product/checkout/notes.md", "set": {"status": "bogus"}}
        ], "rationale": "r"}),
        serde_json::json!({"changes": [
            {"section": "product/checkout/notes#overview", "content": "Mine.\n\n### Margin {#margin-notes}\n\nMine too."},
            {"section": "product/checkout/notes#rollout", "content": "Rollout starts in June."}
        ], "rationale": "r"}),
        // Paths that run into hidden files and directories, visible ones and reserved
        // names.
        change(serde_json::json!({"path": "product/bom-notes.md/new.md", "content": "# New\n"})),
        change(serde_json::json!({"path": "acquisitions/term-sheet.md/new.md", "content": "# New\n"})),
        change(serde_json::json!({"path": "teams/deals", "content": "# New\n"})),
        change(serde_json::json!({"path": "vision.md/new.md", "content": "# New\n"})),
        change(serde_json::json!({"path": "KIWIOWNERS/notes.md", "content": "# New\n"})),
        change(serde_json::json!({"path": "README.md", "content": "# New\n"})),
    ]
}

/// An answer without its snapshot id, wherever it appears (citations carry it too).
fn without_snapshot(text: String) -> Value {
    let value: Value = serde_json::from_str(&text).unwrap();
    let text = match value["snapshot"].as_str() {
        Some(snapshot) if !snapshot.is_empty() => text.replace(snapshot, "SNAPSHOT"),
        _ => text,
    };
    let mut value: Value = serde_json::from_str(&text).unwrap();
    value.as_object_mut().unwrap().remove("snapshot");
    value
}

#[test]
fn hidden_content_cannot_change_any_answer() {
    let workspace = common::workspace();
    let principals: Principals =
        serde_yaml_ng::from_slice(&std::fs::read(workspace.join("bench/principals.yaml")).unwrap()).unwrap();
    let questions: Questions =
        serde_yaml_ng::from_slice(&std::fs::read(workspace.join("bench/questions.yaml")).unwrap()).unwrap();
    let mut queries: Vec<Vec<String>> = questions.questions.iter().map(|q| q.queries.clone()).collect();
    for word in [
        "Halcyon",
        "Loomwright",
        "Tern",
        "Northwind",
        "take rate pricing",
        "budget 2027",
    ] {
        queries.push(vec![word.to_owned()]);
    }
    for (word, _, _) in CANARIES {
        queries.push(vec![word.to_owned()]);
    }
    queries.push(vec!["ADR-005".into(), "saga steps".into()]);
    queries.push(vec!["burn rate".into(), "runway burn".into()]);
    let traces = [
        "PRD-001",
        "PRD-004",
        "RFC-013",
        "ADR-003",
        "vision",
        "glossary",
        "ADR-005",
        "Saga",
        "ADR-090",
        "CHK-NOTES",
    ];
    let reads: Vec<String> = [
        "ADR-005",
        "Saga",
        "acquisitions/term-sheet",
        "adr/adr-090-board-vote",
        "adr/adr-090-board-vote#outcome",
        "product/checkout/notes",
        "product/checkout/notes#overview",
        "product/checkout/notes#margin-notes",
        "product/checkout/notes#budget-line",
        "product/checkout/notes#rollout",
        "product/bom-notes",
        "glossary#saga",
        "finance/terms#burn-rate",
    ]
    .iter()
    .map(|id| (*id).to_owned())
    .collect();

    let full = full_corpus();
    let full_engine = engine(&full.root);
    let full_gate = gate(&full.root);
    let full_service = service(&full.root);
    let mut compared = 0usize;
    let mut seen = BTreeSet::new();
    for case in &principals.principals {
        if !seen.insert((case.id.clone(), case.agent.clone(), case.channel.clone())) {
            continue;
        }
        let identity = Identity::new(case.id.clone(), case.agent.clone(), case.channel.clone());
        let full_view = full_gate.view(full_engine.pack(), &identity).unwrap();

        let stripped = stripped_corpus(&full_gate, &identity);
        let stripped_engine = engine(&stripped.root);
        let stripped_view = gate(&stripped.root)
            .view(stripped_engine.pack(), &identity)
            .unwrap();
        assert_eq!(
            stripped_view.visible().len(),
            stripped_engine.pack().atom_count() as u64,
            "{}: the stripped corpus holds only what the principal sees",
            case.id
        );
        assert_eq!(
            stripped_view.visible().len(),
            full_view.visible().len(),
            "{}",
            case.id
        );

        for qs in &queries {
            let request = SearchRequest {
                queries: qs.clone(),
                ..Default::default()
            };
            let a = serde_json::to_string(&full_engine.search(&full_view, &request).unwrap().0).unwrap();
            let b =
                serde_json::to_string(&stripped_engine.search(&stripped_view, &request).unwrap().0).unwrap();
            assert_eq!(
                without_snapshot(a),
                without_snapshot(b),
                "{} search {qs:?}",
                case.id
            );
            compared += 1;
        }
        for (scope, depth) in [(None, Some(3)), (Some("product".to_owned()), Some(4))] {
            let request = MapRequest {
                scope,
                depth,
                budget: Some(20_000),
            };
            let a = serde_json::to_string(&full_engine.map(&full_view, &request).unwrap().0).unwrap();
            let b = serde_json::to_string(&stripped_engine.map(&stripped_view, &request).unwrap().0).unwrap();
            assert_eq!(without_snapshot(a), without_snapshot(b), "{} map", case.id);
            compared += 1;
        }
        for context in [None, Some(kiwi_core::api::ReadContext::Children)] {
            let request = ReadRequest {
                ids: reads.clone(),
                level: Some(Level::L2),
                context,
            };
            let a = serde_json::to_string(&full_engine.read(&full_view, &request).unwrap().0).unwrap();
            let b =
                serde_json::to_string(&stripped_engine.read(&stripped_view, &request).unwrap().0).unwrap();
            assert_eq!(without_snapshot(a), without_snapshot(b), "{} read", case.id);
            compared += 1;
        }
        let full_before = audit_records(&full.root).len();
        let stripped_service = service(&stripped.root);
        let stripped_before = audit_records(&stripped.root).len();
        for request in proposals() {
            let a = proposal_answer(&full_service, &identity, &request);
            let b = proposal_answer(&stripped_service, &identity, &request);
            assert_eq!(a, b, "{} propose {request}", case.id);
            compared += 1;
        }
        for id in traces {
            let request = TraceRequest {
                id: id.into(),
                ..Default::default()
            };
            let a = full_engine
                .trace(&full_view, &request)
                .map(|r| serde_json::to_string(&r.0).unwrap());
            let b = stripped_engine
                .trace(&stripped_view, &request)
                .map(|r| serde_json::to_string(&r.0).unwrap());
            match (a, b) {
                (Ok(a), Ok(b)) => {
                    assert_eq!(without_snapshot(a), without_snapshot(b), "{} trace {id}", case.id)
                }
                (Err(_), Err(_)) => {}
                (a, b) => panic!("{} trace {id}: {a:?} vs {b:?}", case.id),
            }
            compared += 1;
        }

        // Every tool through the service: the same answers, and audit records that name
        // the same atoms and documents, none hidden from the caller.
        let mut calls: Vec<(&str, Value)> = queries
            .iter()
            .map(|qs| ("kiwi_search", json!({ "queries": qs })))
            .collect();
        calls.push(("kiwi_map", json!({"depth": 3, "budget": 20_000})));
        calls.push((
            "kiwi_map",
            json!({"scope": "product", "depth": 4, "budget": 20_000}),
        ));
        calls.push(("kiwi_read", json!({ "ids": reads })));
        calls.push(("kiwi_read", json!({"ids": reads, "context": "children"})));
        calls.extend(traces.iter().map(|id| ("kiwi_trace", json!({ "id": id }))));
        calls.push((
            "kiwi_feedback",
            json!({"query": "q", "ids": reads, "verdict": "useful"}),
        ));
        for (tool, args) in &calls {
            let a = full_service
                .call(&identity, tool, args.clone())
                .map(without_snapshot)
                .map_err(|e| e.to_string());
            let b = stripped_service
                .call(&identity, tool, args.clone())
                .map(without_snapshot)
                .map_err(|e| e.to_string());
            assert_eq!(a, b, "{} {tool} {args}", case.id);
            compared += 1;
        }
        for id in &reads {
            let resource = |service: &KiwiService, root: &Path| {
                let snapshot = PackStore::new(root.join(PACK_DIR))
                    .current()
                    .unwrap()
                    .map(|path| Pack::open(path).unwrap().snapshot().to_owned())
                    .unwrap();
                service
                    .read_resource(&identity, &format!("kiwi://atom/{id}@{snapshot}"))
                    .map_err(|e| e.to_string().replace(&snapshot, "SNAPSHOT"))
            };
            assert_eq!(
                resource(&full_service, &full.root),
                resource(&stripped_service, &stripped.root),
                "{} resource {id}",
                case.id
            );
        }
        let full_records = audit_records(&full.root).split_off(full_before);
        let stripped_records = audit_records(&stripped.root).split_off(stripped_before);
        assert_eq!(full_records.len(), stripped_records.len(), "{}", case.id);
        assert!(full_records.len() >= calls.len() + reads.len());
        for (a, b) in full_records.iter().zip(&stripped_records) {
            for key in ["principal", "tool", "status", "ids", "docs", "results", "budget"] {
                assert_eq!(a[key], b[key], "{} {key}: {a} vs {b}", case.id);
            }
            let ids: Vec<&str> = a["ids"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|id| id.as_str().unwrap())
                .collect();
            for id in &ids {
                let uid = full_engine.pack().resolve_id(id).unwrap();
                assert!(
                    uid.is_some_and(|uid| full_view.contains(uid)),
                    "{}: a record names {id}, hidden from them",
                    case.id
                );
            }
            for doc in a["docs"].as_array().into_iter().flatten() {
                let doc = doc.as_str().unwrap();
                assert!(
                    ids.iter().any(|id| id.split('#').next() == Some(doc)),
                    "{}: a record names the document {doc} of none of its ids",
                    case.id
                );
            }
            assert!(a.get("queries").is_none(), "requests are not recorded by default");
        }
    }
    assert!(compared > 400, "compared {compared} answers");
}

#[test]
fn adversarial_declarations_are_enforced() {
    let full = full_corpus();
    let engine = engine(&full.root);
    let gate = gate(&full.root);
    // As in `bench/principals.yaml`: restricted content is read only over HTTP.
    let pairs = [
        ("carl", "claude-code", "stdio"),
        ("alice", "claude-code", "stdio"),
        ("priya", "cursor", "stdio"),
        ("fiona", "claude-code", "stdio"),
        ("fiona", "summarizer", "stdio"),
        ("erin", "claude-code", "http"),
        ("erin", "untrusted-bot", "stdio"),
    ];
    for (principal, agent, channel) in pairs {
        let identity = Identity::new(principal, agent, channel);
        let view = gate.view(engine.pack(), &identity).unwrap();
        for (word, id, audience) in CANARIES {
            let expected = may_read(principal, agent, word, audience);
            let uid = engine
                .pack()
                .resolve_id(id)
                .unwrap()
                .unwrap_or_else(|| panic!("{id}"));
            assert_eq!(view.contains(uid), expected, "{principal}/{agent} reading {id}");
            let request = SearchRequest {
                queries: vec![word.to_owned()],
                budget: Some(20_000),
                ..Default::default()
            };
            let answer = serde_json::to_string(&engine.search(&view, &request).unwrap().0).unwrap();
            let read = ReadRequest {
                ids: vec![(*id).to_owned()],
                level: Some(Level::L2),
                context: Some(kiwi_core::api::ReadContext::Children),
            };
            let opened = serde_json::to_string(&engine.read(&view, &read).unwrap().0).unwrap();
            if !expected {
                assert!(!answer.contains(word), "{principal}/{agent}: {word} in {answer}");
            }
            assert_eq!(
                opened.contains(word),
                expected,
                "{principal}/{agent}: {word} in {opened}"
            );
        }
        // The id ADR-005 names the ADR for everyone who may read it, never the hidden
        // document titled `ADR-005`.
        let uid = engine.pack().resolve_id("ADR-005").unwrap().unwrap();
        assert_eq!(
            engine.pack().atom(uid).unwrap().id.as_str(),
            "adr/adr-005-idempotency-keys"
        );
    }
}

#[test]
fn malformed_declarations_stop_the_build() {
    let cases = [
        ("product/x.md", "---\naccess: finance\n# Never closed\n\nText.\n"),
        ("product/x.md", "\n---\naccess: finance\n---\n# Late fence\n"),
        ("product/x.md", "---\nacess: finance\n---\n# Misspelt key\n"),
        ("product/x.md", "---\naccess:\n---\n# Empty access\n"),
        ("product/x.md", "---\naccess: null\n---\n# Null access\n"),
        (
            "product/x.md",
            "---\naccess: { label: confidential, group: [finance] }\n---\n# Misspelt group\n",
        ),
        (
            "product/x.md",
            "# X\n\n## Pricing {#pricing acess=finance}\n\nText.\n",
        ),
        (
            "product/x.md",
            "# X\n\n## Pricing {#pricing access=finance} draft\n\nText.\n",
        ),
        ("product/x.md", "# X\n\n> ## Quoted {access=finance}\n"),
        (
            "KIWIOWNERS",
            "* label=internal\nfinance/** label=confidential grops=finance\n",
        ),
    ];
    for (path, text) in cases {
        let corpus = Corpus::copy();
        std::fs::write(corpus.root.join(path), text).unwrap();
        let out = corpus.kiwi(&["build", ".", "--embedder", "hash", "--worktree"]);
        assert!(!out.status.success(), "{path}: {text:?} built");
        assert!(
            PackStore::new(corpus.root.join(PACK_DIR))
                .current()
                .unwrap()
                .is_none(),
            "nothing is served"
        );
    }
}

#[test]
fn bench_leak_suite_is_clean() {
    let workspace = common::workspace();
    let out = std::process::Command::new(common::KIWI)
        .args([
            "bench",
            "fixtures/acme-kb",
            "--embedder",
            "hash",
            "--repeat",
            "1",
            "--json",
        ])
        .current_dir(&workspace)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["leak_violations"], serde_json::json!([]), "{report:#}");
    assert_eq!(report["missing_visibility"], serde_json::json!([]), "{report:#}");
    assert!(report["leak_checks"].as_u64().unwrap() > 400);
    for mode in report["modes"].as_array().unwrap() {
        if mode["mode"] != "grep+read" {
            assert_eq!(mode["leaks"], 0, "{mode}");
        }
    }
}

#[test]
fn bench_runs_on_a_git_checkout() {
    // A clone's `.git` holds read-only object files; the bench must not copy them.
    let corpus = Corpus::git();
    let workspace = common::workspace();
    let out = std::process::Command::new(common::KIWI)
        .args([
            "bench",
            corpus.root.to_str().unwrap(),
            "--questions",
            workspace.join("bench/questions.yaml").to_str().unwrap(),
            "--principals",
            workspace.join("bench/principals.yaml").to_str().unwrap(),
            "--embedder",
            "hash",
            "--repeat",
            "1",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        report["incremental_atoms"].as_u64().map(|n| n > 0),
        Some(true),
        "{report:#}"
    );
    assert!(corpus.root.join(".git").is_dir());
}
