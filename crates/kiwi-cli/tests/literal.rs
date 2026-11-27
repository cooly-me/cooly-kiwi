//! A literal search lists every line that contains an exact string, page by page, with
//! the section and line `kiwi_read` gives it at, and refuses what it cannot answer.

#![allow(clippy::unwrap_used)]

mod common;

use std::collections::BTreeSet;

use common::Corpus;
use serde_json::{Value, json};

fn call(corpus: &Corpus, tool: &str, args: &Value) -> Result<Value, String> {
    let out = corpus.kiwi(&["query", ".", tool, &args.to_string(), "--principal", "alice"]);
    if out.status.success() {
        Ok(serde_json::from_slice(&out.stdout).unwrap())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

/// Every match of `args`, following `next` from page to page, and the pages it took.
fn every_match(corpus: &Corpus, args: Value) -> (Vec<Value>, usize) {
    let (mut matches, mut pages, mut args) = (Vec::new(), 0, args);
    loop {
        let page = call(corpus, "kiwi_search", &args).unwrap();
        pages += 1;
        matches.extend(page["matches"].as_array().unwrap().iter().cloned());
        match page.get("next") {
            Some(next) => args = next.clone(),
            None => return (matches, pages),
        }
    }
}

#[test]
fn a_literal_lists_every_matching_line_page_by_page() {
    let corpus = Corpus::copy();
    corpus.build(&[]);

    let (all, pages) = every_match(
        &corpus,
        json!({ "literal": "saga", "ignore_case": true, "budget": 32000 }),
    );
    assert_eq!(pages, 1);
    assert!(all.len() > 5, "{all:?}");
    // Small pages give the same matches in the same order, each once.
    let (paged, pages) = every_match(
        &corpus,
        json!({ "literal": "saga", "ignore_case": true, "budget": 50 }),
    );
    assert!(pages > 2, "{pages}");
    assert_eq!(paged, all);

    // Case: a case-sensitive literal finds a subset, each line containing it as written.
    let (exact, _) = every_match(&corpus, json!({ "literal": "Saga", "budget": 32000 }));
    assert!(!exact.is_empty() && exact.len() < all.len());
    let lines: BTreeSet<(String, u64)> = all
        .iter()
        .map(|m| (m["id"].as_str().unwrap().to_owned(), m["line"].as_u64().unwrap()))
        .collect();
    for m in &exact {
        assert!(m["text"].as_str().unwrap().contains("Saga"), "{m}");
        assert!(lines.contains(&(m["id"].as_str().unwrap().to_owned(), m["line"].as_u64().unwrap())));
    }

    // Each match is the line of its section's text as kiwi_read gives it.
    for m in &all {
        let read = call(&corpus, "kiwi_read", &json!({ "ids": [m["id"]] })).unwrap();
        let text = read["atoms"][0]["text"].as_str().unwrap();
        let line = text
            .lines()
            .nth(m["line"].as_u64().unwrap() as usize - 1)
            .unwrap();
        assert_eq!(line, m["text"].as_str().unwrap(), "{m}");
        assert!(line.to_lowercase().contains("saga"));
    }

    // Filters narrow it; a literal found nowhere is an empty list, not a gap.
    let (rfcs, _) = every_match(
        &corpus,
        json!({ "literal": "saga", "ignore_case": true, "filters": { "path": "rfcs/" }, "budget": 32000 }),
    );
    assert!(!rfcs.is_empty() && rfcs.len() < all.len());
    assert!(
        rfcs.iter()
            .all(|m| m["id"].as_str().unwrap().starts_with("rfcs/"))
    );
    let (none, _) = every_match(&corpus, json!({ "literal": "Zyxwvut" }));
    assert!(none.is_empty());
    let gaps = corpus.kiwi(&["gaps", "."]);
    assert!(!String::from_utf8_lossy(&gaps.stdout).contains("Zyxwvut"));
}

#[test]
fn a_literal_search_refuses_what_it_cannot_answer() {
    let corpus = Corpus::copy();
    corpus.build(&[]);
    for (args, said) in [
        (
            json!({ "literal": "saga", "queries": ["saga"] }),
            "replaces `queries`",
        ),
        (json!({ "literal": "saga", "limit": 5 }), "not `limit`"),
        (json!({ "literal": "  " }), "`literal` is empty"),
        (json!({ "literal": "saga\nsteps" }), "within one line"),
        (
            json!({ "literal": "saga", "after": "nowhere" }),
            "copy it from `next`",
        ),
        (
            json!({ "literal": "saga", "after": "no/such/doc:3" }),
            "copy it from `next`",
        ),
        (
            json!({ "queries": ["saga"], "ignore_case": true }),
            "go with `literal`",
        ),
    ] {
        let error = call(&corpus, "kiwi_search", &args).unwrap_err();
        assert!(error.contains(said), "{args}: {error}");
    }
}
