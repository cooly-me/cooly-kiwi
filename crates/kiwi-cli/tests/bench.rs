//! `kiwi bench` on the example corpus (BM25, vectors with the hash embedder, the leak
//! suite): it finds no leak, and a baseline it falls behind fails the run.

#![cfg(all(feature = "bm25", feature = "vec"))]
#![allow(clippy::unwrap_used)]

mod common;

use serde_json::Value;

#[test]
fn the_bench_finds_no_leak_and_fails_below_its_baseline() {
    let root = common::workspace();
    let dir = tempfile::tempdir().unwrap();
    // Recall above 1 cannot be met: the run must report the regression and fail.
    let baseline = dir.path().join("baseline.toml");
    std::fs::write(
        &baseline,
        "[tolerance]\nquality = 0.0\nlatency = 1000.0\nlatency_floor_ms = 1000.0\n\n[modes.bm25]\nrecall_at_5 = 1.01\n",
    )
    .unwrap();
    let out = std::process::Command::new(common::KIWI)
        .args([
            "bench",
            "fixtures/acme-kb",
            "--embedder",
            "hash",
            "--json",
            "--baseline",
        ])
        .arg(&baseline)
        .current_dir(&root)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("regression: bm25 recall@5"), "{stderr}");
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
    for mode in report["modes"].as_array().unwrap() {
        if mode["mode"] != "grep+read" {
            assert_eq!(mode["leaks"], 0, "{mode}");
        }
    }
    assert!(report["reload_ms"].as_f64().unwrap() > 0.0, "{report}");
}
