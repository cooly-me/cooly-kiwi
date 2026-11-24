//! A third-party WebAssembly plugin runs as a lint and an enricher, sandboxed.

#![cfg(feature = "wasm")]
#![allow(clippy::unwrap_used)]

mod common;

use common::Corpus;

#[test]
fn example_plugin_lints_through_kiwi_lint() {
    let corpus = Corpus::copy();
    std::fs::create_dir_all(corpus.root.join("plugins")).unwrap();
    std::fs::copy(
        common::workspace().join("plugins/example-lint/example_lint.wasm"),
        corpus.root.join("plugins/example_lint.wasm"),
    )
    .unwrap();
    let config = corpus.root.join(".kiwi/kiwi.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str(
        "\n[plugins.wasm]\ncomponents = [{ name = \"example-lint\", path = \"plugins/example_lint.wasm\" }]\n",
    );
    std::fs::write(&config, text).unwrap();
    std::fs::write(
        corpus.root.join("adr/adr-098-draft.md"),
        "---\nid: ADR-098\ntype: adr\nstatus: draft\nowner: \"@team-platform\"\nsummary: A draft.\n---\n\n# Draft decision\n\nTODO: decide between the two options described below in detail.\n",
    )
    .unwrap();
    let out = corpus.kiwi(&["lint", ".", "--json"]);
    let findings: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let hits: Vec<&serde_json::Value> = findings
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["rule"] == "example-lint/todo-marker")
        .collect();
    assert_eq!(hits.len(), 1, "{findings:#}");
    assert_eq!(hits[0]["id"], "adr/adr-098-draft");
    // The enricher runs at build time too.
    corpus.build(&[]);
}
