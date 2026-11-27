//! The access files (`KIWIOWNERS`, the policy, its tests, the principals) are read
//! verbatim through `kiwi_read`, each guarded like any file of the corpus by the access
//! `KIWIOWNERS` declares for its path, through the same policy, agent grant and audit.

#![allow(clippy::unwrap_used)]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use common::Corpus;
use serde_json::{Value, json};

const FILES: [&str; 4] = [
    "KIWIOWNERS",
    ".kiwi/policy.cedar",
    ".kiwi/policy.tests.yaml",
    ".kiwi/principals.yaml",
];

fn read(corpus: &Corpus, ids: &[&str], principal: &str, agent: &str) -> Value {
    let args = json!({ "ids": ids, "level": "L0" }).to_string();
    let out = corpus.kiwi(&[
        "query",
        ".",
        "kiwi_read",
        &args,
        "--principal",
        principal,
        "--agent",
        agent,
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

/// The ids of the files an answer gave.
fn given(answer: &Value) -> Vec<&str> {
    answer["files"]
        .as_array()
        .map(|files| files.iter().map(|f| f["id"].as_str().unwrap()).collect())
        .unwrap_or_default()
}

#[test]
fn an_access_file_is_read_verbatim_by_who_may_read_its_path() {
    let corpus = Corpus::copy();
    corpus.build(&[]);
    // `*` declares everything internal: staff read the access files whole and verbatim.
    let staff = read(&corpus, &FILES, "alice", "claude-code");
    assert_eq!(given(&staff), FILES, "{staff}");
    for file in staff["files"].as_array().unwrap() {
        let path = file["id"].as_str().unwrap();
        let disk = std::fs::read_to_string(corpus.root.join(path)).unwrap();
        assert_eq!(file["text"], disk, "{path}");
    }
    assert!(staff.get("not_found").is_none(), "{staff}");

    // Who may not read internal content gets what a path that does not exist gets.
    let outsider = read(&corpus, &FILES, "carl", "claude-code");
    assert_eq!(outsider["not_found"], json!(FILES), "{outsider}");
    assert!(outsider.get("files").is_none(), "{outsider}");
    let missing = read(&corpus, &[".kiwi/missing.yaml"], "carl", "claude-code");
    let alone = read(&corpus, &["KIWIOWNERS"], "carl", "claude-code");
    assert_eq!(
        alone.to_string().replace("KIWIOWNERS", ".kiwi/missing.yaml"),
        missing.to_string()
    );

    // The agent's grant caps it as it caps content: public only reads none of them.
    let capped = read(&corpus, &FILES, "alice", "untrusted-bot");
    assert!(capped.get("files").is_none(), "{capped}");

    // Sections and files in one read: each where it belongs, in request order.
    let mixed = read(
        &corpus,
        &[".kiwi/policy.cedar", "vision", "KIWIOWNERS"],
        "alice",
        "cli",
    );
    assert_eq!(given(&mixed), [".kiwi/policy.cedar", "KIWIOWNERS"]);
    assert_eq!(mixed["atoms"][0]["id"], "vision");
}

#[test]
fn kiwiowners_narrows_an_access_file_like_any_path() {
    let corpus = Corpus::copy();
    let owners = corpus.root.join("KIWIOWNERS");
    let mut rules = std::fs::read_to_string(&owners).unwrap();
    rules.push_str(".kiwi/principals.yaml  label=confidential groups=finance\n");
    std::fs::write(&owners, rules).unwrap();
    corpus.build(&[]);
    let engineer = read(&corpus, &FILES, "alice", "claude-code");
    assert_eq!(
        given(&engineer),
        ["KIWIOWNERS", ".kiwi/policy.cedar", ".kiwi/policy.tests.yaml"]
    );
    assert_eq!(engineer["not_found"], json!([".kiwi/principals.yaml"]));
    let finance = read(&corpus, &[".kiwi/principals.yaml"], "fiona", "claude-code");
    assert_eq!(given(&finance), [".kiwi/principals.yaml"]);
}

#[test]
fn a_read_access_file_is_audited_by_its_path() {
    let corpus = Corpus::copy();
    corpus.build(&[]);
    let mut child = Command::new(common::KIWI)
        .args(["serve", ".", "--principal", "alice", "--agent", "claude-code"])
        .current_dir(&corpus.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        for message in [
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"kiwi_read","arguments":{"ids":["vision",".kiwi/policy.cedar"],"level":"L0"}}}"#,
        ] {
            stdin.write_all(message.as_bytes()).unwrap();
            stdin.write_all(b"\n").unwrap();
            stdin.flush().unwrap();
            if message.contains("\"id\"") {
                line.clear();
                stdout.read_line(&mut line).unwrap();
            }
        }
        assert!(line.contains(r#"\"files\""#), "{line}");
    }
    let _ = child.kill();
    let _ = child.wait();
    let log = std::fs::read_to_string(corpus.root.join(".kiwi/state/audit.log")).unwrap();
    let record: Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
    assert_eq!(record["tool"], "kiwi_read");
    assert_eq!(
        record["docs"],
        json!(["vision", ".kiwi/policy.cedar"]),
        "{record}"
    );
}
