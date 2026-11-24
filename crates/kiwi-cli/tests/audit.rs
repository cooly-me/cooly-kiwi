//! The audit log written by a real session verifies, and any edited line fails
//! `kiwi audit verify`.

#![allow(clippy::unwrap_used)]

mod common;

use common::Corpus;

/// Drives one served session over stdio: `searches` searches, each audited.
fn session(corpus: &Corpus, searches: usize) {
    let mut child = std::process::Command::new(common::KIWI)
        .args(["serve", ".", "--principal", "alice", "--agent", "claude-code"])
        .current_dir(&corpus.root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    {
        use std::io::{BufRead, BufReader, Write};
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut send = |line: &str| {
            stdin.write_all(line.as_bytes()).unwrap();
            stdin.write_all(b"\n").unwrap();
            stdin.flush().unwrap();
        };
        send(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#,
        );
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        let queries = ["ADR-005", "take rate", "saga"];
        for n in 0..searches {
            let id = n + 2;
            let query = queries[n % queries.len()];
            send(&format!(
                r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"kiwi_search","arguments":{{"queries":["{query}"]}}}}}}"#
            ));
            line.clear();
            stdout.read_line(&mut line).unwrap();
            assert!(line.contains("\"result\""), "{line}");
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn servers_on_one_corpus_share_one_chain_and_survive_a_torn_line() {
    let corpus = Corpus::copy();
    corpus.build(&[]);
    // Two servers at once on one corpus: one linear chain, no fork.
    std::thread::scope(|scope| {
        for _ in 0..2 {
            scope.spawn(|| session(&corpus, 20));
        }
    });
    let log = corpus.root.join(".kiwi/state/audit.log");
    let verify = || {
        let out = corpus.kiwi(&["audit", "verify", log.to_str().unwrap()]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
        String::from_utf8(out.stdout).unwrap()
    };
    assert!(verify().contains("40 records"));
    // A crash in the middle of a write: reported, and the next server goes on.
    let mut file = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
    std::io::Write::write_all(&mut file, b"{\"seq\":41,\"ts\":").unwrap();
    drop(file);
    assert!(verify().contains("torn tail"));
    session(&corpus, 1);
    let report = verify();
    assert!(report.contains("42 records"), "{report}");
    assert!(report.contains("break at line 41"), "{report}");
}

#[test]
fn tampering_with_any_line_is_detected() {
    let corpus = Corpus::copy();
    corpus.build(&[]);
    // `kiwi query` does not audit; a served session does.
    session(&corpus, 3);

    let log = corpus.root.join(".kiwi/state/audit.log");
    let ok = corpus.kiwi(&["audit", "verify", log.to_str().unwrap()]);
    assert!(ok.status.success(), "{}", String::from_utf8_lossy(&ok.stdout));
    assert!(String::from_utf8_lossy(&ok.stdout).contains("3 records"));

    let original = std::fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = original.lines().collect();
    for target in 0..lines.len() {
        let mut edited: Vec<String> = lines.iter().map(|l| (*l).to_owned()).collect();
        edited[target] = edited[target]
            .replacen("\"blocked\":", "\"blocked\":1", 1)
            .replacen("\"blocked\":10", "\"blocked\":0", 1);
        if edited[target] == lines[target] {
            edited[target] = edited[target].replacen("alice", "mallory", 1);
        }
        std::fs::write(&log, edited.join("\n") + "\n").unwrap();
        let out = corpus.kiwi(&["audit", "verify", log.to_str().unwrap()]);
        assert_eq!(
            out.status.code(),
            Some(1),
            "line {} edit not detected",
            target + 1
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains(&format!("line {}", target + 1)));
    }
}

/// A log the Kiwi 0.1.0 binary wrote: records of every tool, a refused proposal's
/// note and a quarantined torn line.
const V0_1_0_LOG: &str = include_str!("../../kiwi-policy/tests/fixtures/audit-v0.1.0.log");

#[test]
fn a_kiwi_0_1_0_log_verifies_grows_and_exports() {
    let corpus = Corpus::copy();
    corpus.build(&[]);
    let log = corpus.root.join(".kiwi/state/audit.log");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, V0_1_0_LOG).unwrap();
    let path = log.to_str().unwrap();
    let verify = corpus.kiwi(&["audit", "verify", path]);
    assert!(verify.status.success());
    assert!(String::from_utf8_lossy(&verify.stdout).contains("10 records"));
    // This version's server goes on with the old chain.
    session(&corpus, 2);
    let verify = corpus.kiwi(&["audit", "verify", path]);
    let report = String::from_utf8(verify.stdout).unwrap();
    assert!(verify.status.success(), "{report}");
    assert!(
        report.contains("12 records") && report.contains("break at line 9"),
        "{report}"
    );

    // The new records, as sinks receive them.
    let export = corpus.kiwi(&["audit", "export", path, "--since", "10", "--format", "jsonl"]);
    assert!(
        export.status.success(),
        "{}",
        String::from_utf8_lossy(&export.stderr)
    );
    let shipped: Vec<serde_json::Value> = String::from_utf8(export.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(shipped.len(), 2);
    for (record, seq) in shipped.iter().zip([11, 12]) {
        assert_eq!(record["seq"], seq);
        assert_eq!(record["status"], "ok");
        assert!(record["tokens"].is_u64() && record["latency_ms"].is_u64());
        assert!(!record["ids"].as_array().unwrap().is_empty());
    }
    // Everything, with the old note replaced by its MAC.
    let export = corpus.kiwi(&["audit", "export", path]);
    let text = String::from_utf8(export.stdout).unwrap();
    assert_eq!(text.lines().count(), 12);
    assert!(!text.contains("\"note\"") && text.contains("\"note_mac\""));
    assert!(!text.contains("you may not propose"));

    // Nothing is exported from a log that does not verify, or past a wrong anchor.
    let export = corpus.kiwi(&["audit", "export", path, "--anchor", &"0".repeat(64)]);
    assert_eq!(export.status.code(), Some(1));
    assert!(export.stdout.is_empty());
    let original = std::fs::read_to_string(&log).unwrap();
    std::fs::write(&log, original.replacen("\"carl\"", "\"mallory\"", 1)).unwrap();
    let export = corpus.kiwi(&["audit", "export", path, "--since", "11"]);
    assert_eq!(export.status.code(), Some(1));
    assert!(export.stdout.is_empty());
    assert!(String::from_utf8_lossy(&export.stderr).contains("TAMPERED: line 10"));
}
