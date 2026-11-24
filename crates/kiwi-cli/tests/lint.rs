//! `kiwi lint`, `kiwi policy test` and `kiwi gaps` on the fixture and on a broken corpus.

#![allow(clippy::unwrap_used)]

mod common;

use common::Corpus;

#[test]
fn fixture_lints_with_warnings_only() {
    let corpus = Corpus::copy();
    let out = corpus.kiwi(&["lint", ".", "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    let findings: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let rules: Vec<&str> = findings
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["rule"].as_str().unwrap())
        .collect();
    assert!(rules.contains(&"missing-owner"), "{rules:?}");
    assert!(rules.contains(&"review-overdue"), "{rules:?}");
}

#[test]
fn broken_corpus_fails_lint() {
    let corpus = Corpus::copy();
    std::fs::write(
        corpus.root.join("adr/adr-099-broken.md"),
        "---\nid: ADR-099\ntype: adr\nstatus: approved\nowner: \"@x\"\nsummary: s.\nsupersedes: [ADR-404]\n---\n\n# Broken\n\nSee [nothing](nothing.md).\n",
    )
    .unwrap();
    let glossary = corpus.root.join("glossary.md");
    let mut text = std::fs::read_to_string(&glossary).unwrap();
    text.push_str("\n## Cart {#cart}\n\nAliases: basket\n\nDefined twice on purpose.\n");
    std::fs::write(&glossary, text).unwrap();
    let out = corpus.kiwi(&["lint", ".", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let findings: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let rules: Vec<&str> = findings
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["rule"].as_str().unwrap())
        .collect();
    for rule in ["broken-link", "broken-reference", "duplicate-term"] {
        assert!(rules.contains(&rule), "{rule} missing in {rules:?}");
    }
}

#[test]
fn policy_tests_pass_and_catch_a_bad_policy() {
    let corpus = Corpus::copy();
    let out = corpus.kiwi(&["policy", "test", "."]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(String::from_utf8_lossy(&out.stdout).contains("23 passed, 0 failed"));
    let policy = corpus.root.join(".kiwi/policy.cedar");
    let mut text = std::fs::read_to_string(&policy).unwrap();
    text.push_str("\npermit (principal, action == Kiwi::Action::\"read\", resource);\n");
    std::fs::write(&policy, text).unwrap();
    let out = corpus.kiwi(&["policy", "test", "."]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn gaps_judge_an_asker_by_the_view_they_had_when_asking() {
    let corpus = Corpus::copy();
    corpus.build(&[]);
    let asked = corpus.kiwi(&[
        "query",
        ".",
        "kiwi_feedback",
        r#"{"query": "when is the halcyon budget signed", "verdict": "missing"}"#,
        "--principal",
        "fiona",
    ]);
    assert!(
        asked.status.success(),
        "{}",
        String::from_utf8_lossy(&asked.stderr)
    );
    // Fiona leaves finance: she now sees what Alice sees, but her question was asked
    // with finance's view.
    let principals = corpus.root.join(".kiwi/principals.yaml");
    let text = std::fs::read_to_string(&principals)
        .unwrap()
        .replace("groups: [finance, staff]", "groups: [staff]");
    std::fs::write(&principals, text).unwrap();
    corpus.build(&[]);
    let alice = corpus.kiwi(&["gaps", ".", "--principal", "alice"]);
    let alice = String::from_utf8(alice.stdout).unwrap();
    assert!(!alice.contains("halcyon"), "{alice}");
    assert!(alice.contains("1 report(s) withheld"), "{alice}");
    let erin = corpus.kiwi(&["gaps", ".", "--principal", "erin"]);
    assert!(String::from_utf8(erin.stdout).unwrap().contains("halcyon"));
}
