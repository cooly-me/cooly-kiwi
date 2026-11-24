//! The OpenAI-compatible embedder, end to end through the `kiwi` binary against a
//! local mock server: it must send only what the policy lets it read, fail a build
//! loudly and a query softly.

#![cfg(feature = "openai")]
#![allow(clippy::unwrap_used)]

mod common;

use std::time::{Duration, Instant};

use common::{Corpus, HIDDEN_WORDS, MockServer, configure, dead_url, embeddings_answer, kiwi_with_env};
use serde_json::Value;

fn openai_config(url: &str, extra: &str) -> String {
    format!(
        "namespace = \"acme\"\n[plugins.vec]\nenabled = true\nembedder = \"openai\"\nmodel = \"toy-embed\"\nurl = \"{url}/v1\"\napi_key_env = \"KIWI_TEST_EMBED_KEY\"\ndimensions = 64\nbatch_size = 16\n{extra}"
    )
}

fn inputs(server: &MockServer) -> Vec<String> {
    server
        .requests()
        .iter()
        .flat_map(|r| {
            let body: Value = serde_json::from_str(&r.body).unwrap();
            serde_json::from_value::<Vec<String>>(body["input"].clone()).unwrap()
        })
        .collect()
}

/// Atoms a public-only reader sees: what network plugins may read under the policy.
fn public_atoms(corpus: &Corpus) -> usize {
    let out = corpus.kiwi(&["export", ".", "--format", "jsonl", "--principal", "carl"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().lines().count()
}

#[test]
fn the_embedding_client_sends_only_what_the_policy_allows() {
    let server = MockServer::start(|r| (200, embeddings_answer(&r.body)));
    let corpus = Corpus::copy();
    configure(&corpus.root, &openai_config(&server.url, ""));
    let env = [
        ("KIWI_TEST_EMBED_KEY", "sk-test-key"),
        ("KIWI_SECRET_ENV", "KIWI_TEST_EMBED_KEY"),
    ];
    let out = kiwi_with_env(&corpus, &["build", "."], &env);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let requests = server.requests();
    assert!(!requests.is_empty());
    for request in &requests {
        assert_eq!(request.path, "/v1/embeddings");
        assert!(
            request
                .headers
                .to_ascii_lowercase()
                .contains("authorization: bearer sk-test-key")
        );
        let body: Value = serde_json::from_str(&request.body).unwrap();
        assert_eq!(body["model"], "toy-embed");
        assert_eq!(body["dimensions"], 64);
        assert!(body["input"].as_array().unwrap().len() <= 16);
    }
    let sent = inputs(&server);
    for word in HIDDEN_WORDS {
        assert!(sent.iter().all(|t| !t.contains(word)), "{word} was sent");
    }
    assert_eq!(
        sent.len(),
        public_atoms(&corpus),
        "exactly the public atoms are embedded"
    );

    // Queries are embedded through the same client; a rebuild with no change sends
    // nothing (vectors are reused by atom hash).
    let before = server.requests().len();
    let out = kiwi_with_env(&corpus, &["build", "."], &env);
    assert!(out.status.success());
    assert_eq!(server.requests().len(), before);
    let out = kiwi_with_env(
        &corpus,
        &[
            "query",
            ".",
            "kiwi_search",
            r#"{"queries": ["what the company is for"]}"#,
            "--principal",
            "carl",
        ],
        &env,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let answer: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        answer
            .get("warnings")
            .is_none_or(|w| !w.to_string().contains("degraded")),
        "{answer}"
    );
    assert!(inputs(&server).iter().any(|t| t == "what the company is for"));
    // The key is never written anywhere in the corpus.
    let config = std::fs::read_to_string(corpus.root.join(".kiwi/kiwi.toml")).unwrap();
    assert!(!config.contains("sk-test-key"));
}

#[test]
fn embedding_failures_are_loud_at_build_and_soft_at_query() {
    let corpus = Corpus::copy();
    // A missing key fails before any request, naming the variable.
    configure(
        &corpus.root,
        &openai_config(&dead_url(), "retries = 1\ntimeout_ms = 500\n"),
    );
    let out = kiwi_with_env(&corpus, &["build", "."], &[]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("KIWI_TEST_EMBED_KEY"));
    // A variable the operator did not list in KIWI_SECRET_ENV is never read, whatever
    // its name.
    let out = kiwi_with_env(&corpus, &["build", "."], &[("KIWI_TEST_EMBED_KEY", "k")]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("KIWI_SECRET_ENV"));

    // A server that is down fails the build and leaves the previous pack current.
    let server = MockServer::start(|r| (200, embeddings_answer(&r.body)));
    let env = [
        ("KIWI_TEST_EMBED_KEY", "k"),
        ("KIWI_SECRET_ENV", "KIWI_TEST_EMBED_KEY"),
    ];
    configure(
        &corpus.root,
        &openai_config(&server.url, "retries = 1\ntimeout_ms = 500\n"),
    );
    assert!(kiwi_with_env(&corpus, &["build", "."], &env).status.success());
    let current = std::fs::read_to_string(corpus.root.join(".kiwi/packs/CURRENT")).unwrap();
    // A new public document (an internal one would not be sent at all).
    std::fs::write(
        corpus.root.join("notes.md"),
        "---\naccess: { label: public }\n---\n# New public note\n\nSomething new.\n",
    )
    .unwrap();
    let dead = dead_url();
    configure(
        &corpus.root,
        &openai_config(&dead, "retries = 1\ntimeout_ms = 500\n"),
    );
    let out = kiwi_with_env(&corpus, &["build", ".", "--worktree"], &env);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("127.0.0.1"));
    assert_eq!(
        std::fs::read_to_string(corpus.root.join(".kiwi/packs/CURRENT")).unwrap(),
        current
    );

    // At query time the vector tier is skipped and the answer says so. The files are
    // those of a pack again (a query is served only from the pack its files built);
    // the unchanged atoms keep their vectors, so this build sends nothing.
    std::fs::remove_file(corpus.root.join("notes.md")).unwrap();
    let out = kiwi_with_env(&corpus, &["build", ".", "--worktree"], &env);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let started = Instant::now();
    let out = kiwi_with_env(
        &corpus,
        &[
            "query",
            ".",
            "kiwi_search",
            r#"{"queries": ["idempotency key retries"]}"#,
            "--principal",
            "alice",
        ],
        &env,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let answer: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        answer["warnings"].to_string().contains("\"degraded\""),
        "{answer}"
    );
    assert!(!answer["results"].as_array().unwrap().is_empty());
    assert!(started.elapsed() < Duration::from_secs(10));
}
