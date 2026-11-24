//! The HTTP reranker, end to end through the `kiwi` binary against a local mock
//! server: it reads only atoms the policy lets it read, and when its server is down the
//! fused order stands.

#![cfg(feature = "rerank")]
#![allow(clippy::unwrap_used)]

mod common;

use common::{Corpus, HIDDEN_WORDS, MockServer, configure, dead_url, kiwi_with_env};
use serde_json::Value;

fn rerank_config(url: &str, enabled: bool) -> String {
    format!(
        "namespace = \"acme\"\n[plugins.rerank]\nenabled = {enabled}\nbackend = \"http\"\nurl = \"{url}/rerank\"\nformat = \"cohere\"\nmodel = \"toy-rerank\"\ntop_k = 10\ntimeout_ms = 2000\n"
    )
}

fn search(corpus: &Corpus, principal: &str, queries: &str) -> Value {
    let out = corpus.kiwi(&["query", ".", "kiwi_search", queries, "--principal", principal]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

fn ids(answer: &Value) -> Vec<String> {
    answer["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn the_http_reranker_reads_only_allowed_atoms_and_falls_back() {
    // Scores passages in reverse order, so a rerank is visible.
    let server = MockServer::start(|r| {
        let body: Value = serde_json::from_str(&r.body).unwrap();
        let n = body["documents"].as_array().unwrap().len();
        let results: Vec<Value> = (0..n)
            .map(|i| serde_json::json!({"index": i, "relevance_score": i as f64}))
            .collect();
        (200, serde_json::json!({ "results": results }).to_string())
    });
    let corpus = Corpus::copy();
    configure(&corpus.root, &rerank_config(&server.url, false));
    corpus.build(&[]);
    let queries = r#"{"queries": ["payout fee bank reconciliation", "idempotency key settlement glossary"]}"#;
    let plain = search(&corpus, "fiona", queries);
    assert!(server.requests().is_empty());

    // Reranking happens at query time: the rebuild the new configuration needs sends
    // nothing to the reranker.
    configure(&corpus.root, &rerank_config(&server.url, true));
    corpus.build(&[]);
    assert!(server.requests().is_empty());
    let reranked = search(&corpus, "fiona", queries);
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_str(&requests[0].body).unwrap();
    assert_eq!(body["model"], "toy-rerank");
    let documents: Vec<String> = serde_json::from_value(body["documents"].clone()).unwrap();
    assert!(documents.len() >= 2, "{documents:?}");
    for word in HIDDEN_WORDS {
        assert!(
            documents.iter().all(|d| !d.contains(word)),
            "{word} reached the reranker"
        );
    }
    // Only public atoms were sent: every passage starts with a public document's title.
    assert!(
        documents
            .iter()
            .all(|d| d.starts_with("GLOSSARY") || d.starts_with("VISION")),
        "{documents:?}"
    );
    let mut before = ids(&plain);
    let mut after = ids(&reranked);
    assert_ne!(before, after, "the reranker moved results");
    before.sort();
    after.sort();
    assert_eq!(before, after, "the same results, reordered");
    assert!(reranked.to_string().contains("\"rerank\""));

    // A loopback endpoint is reached directly even when a proxy is configured, so
    // plain http never passes through it.
    let proxy = dead_url();
    let out = kiwi_with_env(
        &corpus,
        &["query", ".", "kiwi_search", queries, "--principal", "fiona"],
        &[
            ("HTTPS_PROXY", proxy.as_str()),
            ("HTTP_PROXY", proxy.as_str()),
            ("ALL_PROXY", proxy.as_str()),
        ],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        server.requests().len(),
        2,
        "the rerank went to the server, not the proxy"
    );
    let proxied: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(ids(&proxied), ids(&reranked));

    // A reranker that is down: the fused order stands, and the answer says so.
    configure(&corpus.root, &rerank_config(&dead_url(), true));
    corpus.build(&[]);
    let fallback = search(&corpus, "fiona", queries);
    assert_eq!(ids(&fallback), ids(&plain));
    assert!(
        fallback["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["kind"] == "degraded" && w["tier"] == "rerank"),
        "{fallback}"
    );
}
