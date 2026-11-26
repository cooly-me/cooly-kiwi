//! Answers sized for long research: a search of several queries answers every one of
//! them, and a read gives whole documents, says when its budget cut one short and
//! reads on from there.

#![allow(clippy::unwrap_used)]

mod common;

use std::collections::BTreeSet;

use common::Corpus;
use serde_json::{Value, json};

fn call(corpus: &Corpus, tool: &str, args: &Value) -> Value {
    serde_json::from_str(&corpus.query(tool, &args.to_string(), "alice")).unwrap()
}

fn strings(list: &Value) -> Vec<String> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap().to_owned())
        .collect()
}

fn ids(results: &Value) -> Vec<String> {
    results
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_owned())
        .collect()
}

// Natural-language queries need lexical search: without `bm25` a search answers exact ids,
// paths and terms only.
#[cfg(feature = "bm25")]
#[test]
fn a_search_of_five_queries_answers_every_one() {
    let corpus = Corpus::copy();
    corpus.build(&[]);
    let queries = [
        "double charge prevention idempotency",
        "saga compensation cancel orders release inventory",
        "database per service no shared database",
        "typo tolerance edit distance",
        "push failure fall back to email",
    ];
    let batch = call(&corpus, "kiwi_search", &json!({ "queries": queries }));
    let results = &batch["results"];
    let returned: BTreeSet<String> = ids(results).into_iter().collect();
    for query in queries {
        // What the query finds best on its own is in the batch, however strongly the
        // other queries agree on other atoms; the very best with its abstract.
        let alone = call(&corpus, "kiwi_search", &json!({ "queries": [query] }));
        let best = ids(&alone["results"]);
        assert!(best.len() >= 3, "{query}: {alone}");
        for id in &best[..3] {
            assert!(returned.contains(id), "{query}: {id} missing from the batch");
        }
        let top = results
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == best[0].as_str())
            .unwrap();
        assert_eq!(top["level"], "L1", "{query}: {top}");
        // The batch is as deep as asking each query on its own: one query's results
        // and budget are not shared out among five.
        assert!(
            returned.len() >= queries.len() * best.len() * 3 / 4,
            "{query}: {batch}"
        );
    }
    assert_eq!(
        batch["budget"]["limit"].as_u64(),
        alone_limit(&corpus).map(|limit| limit * queries.len() as u64)
    );
}

/// The budget one query gets alone.
fn alone_limit(corpus: &Corpus) -> Option<u64> {
    call(corpus, "kiwi_search", &json!({ "queries": ["refunds"] }))["budget"]["limit"].as_u64()
}

#[test]
fn a_read_gives_whole_documents_and_reads_on_where_its_budget_stopped() {
    let corpus = Corpus::copy();
    corpus.build(&[]);
    let doc = "product/checkout/prd";

    // A document comes whole: its own text and every section under it.
    let whole = call(&corpus, "kiwi_read", &json!({ "ids": [doc] }));
    assert!(whole.get("more").is_none(), "{whole}");
    let atom = &whole["atoms"][0];
    let sections = strings(&atom["sections"]);
    assert!(sections.len() > 3, "{atom}");
    assert_eq!(sections[0], doc);
    assert!(
        sections.iter().any(|id| id.ends_with("#failure-handling")),
        "{sections:?}"
    );
    let text = atom["text"].as_str().unwrap().to_owned();

    // On a small budget the answer says it was cut, what was left out and how to read
    // on; reading on as it says gives the rest, and nothing twice.
    let mut request = json!({ "ids": [doc], "budget": 300 });
    let mut read: Vec<String> = Vec::new();
    let mut parts: Vec<String> = Vec::new();
    let mut calls = 0;
    loop {
        calls += 1;
        assert!(calls <= sections.len(), "reading on does not progress");
        let answer = call(&corpus, "kiwi_read", &request);
        let atom = &answer["atoms"][0];
        let given = match atom.get("sections") {
            Some(list) => strings(list),
            None => vec![atom["id"].as_str().unwrap().to_owned()],
        };
        read.extend(given.iter().cloned());
        parts.push(atom["text"].as_str().unwrap().to_owned());
        let Some(more) = answer.get("more") else {
            break;
        };
        let left = more["sections"].as_u64().unwrap() as usize;
        assert_eq!(read.len() + left, sections.len(), "{more}");
        assert!(more["tokens"].as_u64().unwrap() > 0, "{more}");
        let left_out = more["left_out"].as_array().unwrap();
        assert!(!left_out.is_empty(), "{more}");
        assert!(
            left_out
                .iter()
                .all(|id| !read.contains(&id.as_str().unwrap().to_owned())),
            "{more}"
        );
        request = more["next"].clone();
    }
    assert!(calls > 1, "the budget never cut the read");
    assert_eq!(read, sections);
    assert_eq!(parts.join("\n\n"), text);
}
