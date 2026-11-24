//! `kiwi bench` is a quick tool: on the example corpus (BM25, vectors with the hash
//! embedder, the leak suite) it finishes well within a minute, finds no leak and keeps
//! its recall.

#![cfg(all(feature = "bm25", feature = "vec"))]
#![allow(clippy::unwrap_used)]

mod common;

use std::time::{Duration, Instant};

use serde_json::Value;

/// The bench's time budget on the example corpus (a debug build on a loaded laptop
/// stays well inside it).
const BUDGET: Duration = Duration::from_secs(60);

#[test]
fn the_bench_is_quick_and_finds_no_leak() {
    let root = common::workspace();
    let started = Instant::now();
    let out = std::process::Command::new(common::KIWI)
        .args(["bench", "fixtures/acme-kb", "--embedder", "hash", "--json"])
        .current_dir(&root)
        .output()
        .unwrap();
    let took = started.elapsed();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(took < BUDGET, "kiwi bench took {took:?}");
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(report["leak_checks"].as_u64().unwrap() > 100, "{report}");
    assert!(
        report["leak_violations"].as_array().unwrap().is_empty(),
        "{report}"
    );
    assert!(
        report["missing_visibility"].as_array().unwrap().is_empty(),
        "{report}"
    );
    let bm25 = report["modes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["mode"] == "bm25")
        .unwrap();
    assert_eq!(bm25["leaks"], 0);
    assert!(bm25["recall_at_5"].as_f64().unwrap() >= 0.8, "{bm25}");
}
