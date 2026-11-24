//! What each audit record says about its call: who used how many tokens and read which
//! atoms and documents, by stable id, with the call's status, budget, result count and
//! latency; and what the caller asked for only when the corpus records requests.

#![allow(clippy::unwrap_used)]

mod common;

use std::path::Path;
use std::sync::Arc;

use common::Corpus;
use kiwi_compiler::KiwiConfig;
use kiwi_compiler::build::PACK_DIR;
use kiwi_compiler::compile::count_tokens;
use kiwi_mcp::{CorpusPlugins, KiwiService, ServiceOptions, ToolError};
use kiwi_policy::{Directory, Identity, PolicyGate};
use serde_json::{Value, json};

/// A word no document holds, sent as a query.
const PRIVATE_QUERY: &str = "quokka-zanzibar";

fn service(root: &Path, record_requests: bool) -> KiwiService {
    let mut config =
        KiwiConfig::parse(&std::fs::read_to_string(root.join(".kiwi/kiwi.toml")).unwrap()).unwrap();
    config.audit.record_requests = record_requests;
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
    let policy = std::fs::read_to_string(root.join(".kiwi/policy.cedar")).unwrap();
    let directory =
        Directory::parse(&std::fs::read_to_string(root.join(".kiwi/principals.yaml")).unwrap()).unwrap();
    let plugins = CorpusPlugins {
        factories: vec![Arc::new(kiwi_bm25::Bm25Factory::new(true))],
        reranker: None,
    };
    KiwiService::open(options, PolicyGate::new(&policy, directory).unwrap(), plugins).unwrap()
}

fn records(root: &Path) -> Vec<Value> {
    std::fs::read_to_string(root.join(".kiwi/state/audit.log"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn last(root: &Path) -> Value {
    records(root).pop().unwrap()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| items.iter().map(|v| v.as_str().unwrap().to_owned()).collect())
        .unwrap_or_default()
}

/// The document of a stable id: the path before its anchor.
fn doc_of(id: &str) -> String {
    id.split('#').next().unwrap().to_owned()
}

/// The distinct documents of `ids`, in first-seen order.
fn docs_of(ids: &[String]) -> Vec<String> {
    let mut docs: Vec<String> = Vec::new();
    for doc in ids.iter().map(|id| doc_of(id)) {
        if !docs.contains(&doc) {
            docs.push(doc);
        }
    }
    docs
}

fn map_ids(nodes: &Value, out: &mut Vec<String>) {
    for node in nodes.as_array().unwrap() {
        out.push(node["id"].as_str().unwrap().to_owned());
        if let Some(children) = node.get("children") {
            map_ids(children, out);
        }
    }
}

/// The fields every record of a call carries, checked against the body the caller got.
fn assert_call(record: &Value, tool: &str, status: &str, body: &str) {
    assert_eq!(record["tool"], tool, "{record}");
    assert_eq!(record["status"], status, "{record}");
    assert_eq!(record["tokens"], count_tokens(body), "{record}");
    assert!(record["latency_ms"].is_u64(), "{record}");
    assert!(record.get("queries").is_none(), "{record}");
}

fn shown(result: Result<String, ToolError>) -> (String, bool) {
    match result {
        Ok(text) => (text, true),
        Err(error) => (error.to_string(), false),
    }
}

#[test]
fn every_tool_records_tokens_ids_and_documents() {
    let corpus = Corpus::git();
    corpus.build(&[]);
    let root = &corpus.root;
    let service = service(root, false);
    let alice = Identity::new("alice", "claude-code", "stdio");

    // kiwi_map: every listed node, the budget applied.
    let text = service
        .call(&alice, "kiwi_map", json!({"scope": "adr/"}))
        .unwrap();
    let answer: Value = serde_json::from_str(&text).unwrap();
    let record = last(root);
    assert_call(&record, "kiwi_map", "ok", &text);
    let mut listed = Vec::new();
    map_ids(&answer["nodes"], &mut listed);
    let mut ids = strings(&record["ids"]);
    assert_eq!(record["results"], listed.len());
    listed.sort();
    ids.sort();
    assert_eq!(ids, listed);
    assert!(ids.iter().all(|id| id.starts_with("adr/")), "{ids:?}");
    assert_eq!(record["budget"], answer["budget"]["limit"]);
    assert_eq!(record["atoms"].as_array().unwrap().len(), ids.len());

    // kiwi_search: the results in answer order and their documents (an exact id, found
    // without the lexical index too).
    let text = service
        .call(
            &alice,
            "kiwi_search",
            json!({"queries": ["ADR-005", PRIVATE_QUERY]}),
        )
        .unwrap();
    let answer: Value = serde_json::from_str(&text).unwrap();
    let record = last(root);
    assert_call(&record, "kiwi_search", "ok", &text);
    let results: Vec<String> = answer["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect();
    assert!(!results.is_empty());
    assert_eq!(strings(&record["ids"]), results);
    assert_eq!(strings(&record["docs"]), docs_of(&results));
    assert_eq!(record["results"], results.len());
    assert_eq!(record["budget"], answer["budget"]["limit"]);
    // A cached answer is recorded the same way.
    let again = service
        .call(
            &alice,
            "kiwi_search",
            json!({"queries": ["ADR-005", PRIVATE_QUERY]}),
        )
        .unwrap();
    assert_eq!(again, text);
    let cached = last(root);
    for key in ["ids", "docs", "results", "budget", "tokens", "status"] {
        assert_eq!(cached[key], record[key], "{key}");
    }

    // kiwi_read: what was returned; a hidden id is only counted.
    let text = service
        .call(
            &alice,
            "kiwi_read",
            json!({"ids": ["ADR-005", "finance/budget-2027"]}),
        )
        .unwrap();
    let record = last(root);
    assert_call(&record, "kiwi_read", "ok", &text);
    assert_eq!(strings(&record["ids"]), ["adr/adr-005-idempotency-keys"]);
    assert_eq!(strings(&record["docs"]), ["adr/adr-005-idempotency-keys"]);
    assert_eq!((&record["results"], &record["blocked"]), (&json!(1), &json!(1)));
    assert!(record.get("budget").is_none());

    // kiwi_trace: every node; code paths are no atoms.
    let text = service
        .call(&alice, "kiwi_trace", json!({"id": "ADR-005"}))
        .unwrap();
    let answer: Value = serde_json::from_str(&text).unwrap();
    let record = last(root);
    assert_call(&record, "kiwi_trace", "ok", &text);
    let nodes = answer["nodes"].as_array().unwrap();
    let atoms: Vec<String> = nodes
        .iter()
        .map(|n| n["id"].as_str().unwrap().to_owned())
        .filter(|id| !id.starts_with("code:"))
        .collect();
    assert_eq!(strings(&record["ids"]), atoms);
    assert_eq!(record["results"], nodes.len());

    // kiwi_feedback: the atoms it names that the caller sees.
    let text = service
        .call(
            &alice,
            "kiwi_feedback",
            json!({"query": "keys", "ids": ["ADR-005", "finance/budget-2027"], "verdict": "useful"}),
        )
        .unwrap();
    let record = last(root);
    assert_call(&record, "kiwi_feedback", "ok", &text);
    assert_eq!(strings(&record["ids"]), ["adr/adr-005-idempotency-keys"]);
    assert_eq!(record["blocked"], 1);
    assert!(record.get("results").is_none() && record.get("budget").is_none());

    // The resource: one atom of the pinned snapshot.
    let snapshot = record["snapshot"].as_str().unwrap().to_owned();
    let text = service
        .read_resource(
            &alice,
            &format!("kiwi://atom/adr/adr-005-idempotency-keys#decision@{snapshot}"),
        )
        .unwrap();
    let record = last(root);
    assert_call(&record, "resource", "ok", &text);
    assert_eq!(strings(&record["ids"]), ["adr/adr-005-idempotency-keys#decision"]);
    assert_eq!(strings(&record["docs"]), ["adr/adr-005-idempotency-keys"]);
    assert_eq!(record["results"], 1);

    // kiwi_propose: refused, with its cause kept in the local note only.
    let (text, ok) = shown(service.call(
        &alice,
        "kiwi_propose",
        json!({"changes": [{"path": "finance/budget-2027.md", "content": "# B\n"}], "rationale": "r"}),
    ));
    assert!(!ok);
    let record = last(root);
    assert_call(&record, "kiwi_propose", "refused", &text);
    for key in ["ids", "docs", "results", "budget", "reference"] {
        assert!(record.get(key).is_none(), "{key}: {record}");
    }
    assert!(record["note"].as_str().unwrap().starts_with("refused: "));

    // Opened: the sections and documents it changes.
    let (text, ok) = shown(service.call(
        &alice,
        "kiwi_propose",
        json!({"changes": [
            {"section": "ADR-005", "content": "Every write API takes an idempotency key."},
            {"path": "adr/adr-001-postgres.md", "set": {"status": "deprecated"}},
            {"path": "adr/adr-099-new.md", "content": "# New\n\nA new decision.\n"}
        ], "rationale": "clarify"}),
    ));
    assert!(ok, "{text}");
    let record = last(root);
    assert_call(&record, "kiwi_propose", "ok", &text);
    assert_eq!(
        strings(&record["ids"]),
        ["adr/adr-005-idempotency-keys", "adr/adr-001-postgres"],
        "a new file names no atom of the pack"
    );
    assert_eq!(
        strings(&record["docs"]),
        ["adr/adr-005-idempotency-keys", "adr/adr-001-postgres"]
    );
    assert!(record.get("results").is_none());

    // Refusals before any corpus answered: invalid arguments, an unknown id.
    for (tool, args) in [
        ("kiwi_search", json!({"bogus": 1})),
        ("kiwi_trace", json!({"id": "no/such-atom"})),
    ] {
        let (text, ok) = shown(service.call(&alice, tool, args));
        assert!(!ok);
        let record = last(root);
        assert_call(&record, tool, "refused", &text);
        assert!(record.get("results").is_none() && record.get("ids").is_none());
    }
    let (text, ok) = shown(service.read_resource(&alice, "kiwi://atom/no/such-atom@abcdef0"));
    assert!(!ok);
    assert_call(&last(root), "resource", "refused", &text);

    // No request text is written unless the corpus asks for it.
    let log = std::fs::read_to_string(root.join(".kiwi/state/audit.log")).unwrap();
    assert!(!log.contains(PRIVATE_QUERY) && !log.contains("\"queries\""));
    assert!(kiwi_policy::audit::verify(&root.join(".kiwi/state/audit.log"), None).is_ok());
}

#[test]
fn requests_are_recorded_only_when_the_corpus_asks() {
    let corpus = Corpus::git();
    corpus.build(&[]);
    let root = &corpus.root;
    let service = service(root, true);
    let alice = Identity::new("alice", "claude-code", "stdio");
    let content = "Keys are kept for forty-eight hours, says a private draft.";
    let cases = [
        (
            "kiwi_search",
            json!({"queries": ["idempotency keys", PRIVATE_QUERY], "filters": {"type": ["adr"]}}),
            vec!["idempotency keys", PRIVATE_QUERY],
        ),
        (
            "kiwi_read",
            json!({"ids": ["ADR-005", "finance/budget-2027"]}),
            vec!["ADR-005", "finance/budget-2027"],
        ),
        ("kiwi_map", json!({"scope": "adr/", "depth": 1}), vec!["adr/"]),
        ("kiwi_map", json!({}), vec![]),
        ("kiwi_trace", json!({"id": "ADR-005"}), vec!["ADR-005"]),
        (
            "kiwi_feedback",
            json!({"query": "how long are keys kept", "verdict": "missing", "note": "a note"}),
            vec!["how long are keys kept"],
        ),
        (
            "kiwi_propose",
            json!({"changes": [{"section": "ADR-005", "content": content}], "rationale": "a reason"}),
            vec!["ADR-005"],
        ),
        // A refused call keeps what it asked for too.
        (
            "kiwi_read",
            json!({"ids": ["ADR-005"], "bogus": true}),
            vec!["ADR-005"],
        ),
    ];
    for (tool, args, expected) in cases {
        let _ = service.call(&alice, tool, args.clone());
        let record = last(root);
        assert_eq!(record["tool"], tool);
        let queries = strings(&record["queries"]);
        assert_eq!(queries, expected, "{tool} {args}");
    }
    let snapshot = last(root)["snapshot"].as_str().unwrap().to_owned();
    let uri = format!("kiwi://atom/ADR-005@{snapshot}");
    service.read_resource(&alice, &uri).unwrap();
    assert_eq!(strings(&last(root)["queries"]), [uri]);
    // Only what was asked for: no contents, rationales, filters or notes.
    let log = std::fs::read_to_string(root.join(".kiwi/state/audit.log")).unwrap();
    for absent in [content, "a reason", "a note", "\"adr\"]"] {
        assert!(!log.contains(absent), "{absent}");
    }
}

#[test]
fn a_failure_shares_its_reference_with_the_record() {
    let corpus = Corpus::copy();
    corpus.build(&[]);
    let root = &corpus.root;
    let service = service(root, false);
    let alice = Identity::new("alice", "claude-code", "stdio");
    // The feedback file cannot be written: the call fails inside the server.
    std::fs::create_dir_all(root.join(".kiwi/state/feedback.jsonl")).unwrap();
    let error = service
        .call(
            &alice,
            "kiwi_feedback",
            json!({"query": "keys", "verdict": "missing"}),
        )
        .unwrap_err();
    let ToolError::Failed(reference) = &error else {
        panic!("{error:?}");
    };
    assert_eq!(
        error.to_string(),
        format!("internal error (reference {reference})")
    );
    let record = last(root);
    assert_call(&record, "kiwi_feedback", "failed", &error.to_string());
    assert_eq!(record["reference"], reference.as_str());
    let note = record["note"].as_str().unwrap();
    assert!(
        note.starts_with(&format!("failed (reference {reference}): ")),
        "{note}"
    );
}
