//! Audit sinks, end to end through the `kiwi` binary: an HTTP sink ships the local
//! chain line for line, a dead one never blocks an answer, and a stdout sink cannot
//! share standard output with the MCP protocol.

#![allow(clippy::unwrap_used)]

mod common;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{Corpus, KIWI, MockServer, configure, dead_url};

/// Drives a stdio session through three searches and stops it.
fn session(corpus: &Corpus, extra: &[&str]) -> std::process::ExitStatus {
    use std::io::{BufRead, BufReader, Write};
    let mut child = Command::new(KIWI)
        .args(["serve", ".", "--principal", "alice", "--agent", "claude-code"])
        .args(extra)
        .current_dir(&corpus.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
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
    for (id, query) in [(2, "ADR-005"), (3, "take rate"), (4, "saga")] {
        let started = Instant::now();
        send(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"kiwi_search","arguments":{{"queries":["{query}"]}}}}}}"#
        ));
        line.clear();
        stdout.read_line(&mut line).unwrap();
        assert!(line.contains("\"result\""), "{line}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a sink slowed an answer"
        );
    }
    // Closing stdin ends the session; the sinks flush on the way out. (The closure's
    // last use above ends its borrow of `stdin`.)
    drop(stdin);
    child.wait().unwrap()
}

#[test]
fn the_http_sink_ships_the_chain_and_a_dead_one_never_blocks() {
    ships_the_chain(&[]);
}

#[test]
fn the_sinks_flush_when_a_watching_server_stops() {
    // Watchers hold their own handles on the service; the sinks still ship everything
    // when the session ends.
    ships_the_chain(&["--watch", "--worktree"]);
}

fn ships_the_chain(extra: &[&str]) {
    let collector = MockServer::start(|_| (204, String::new()));
    let corpus = Corpus::copy();
    configure(
        &corpus.root,
        &format!(
            "namespace = \"acme\"\n[[audit.sinks]]\nkind = \"http\"\nurl = \"{}/audit\"\nbatch = 2\nflush_ms = 50\n[[audit.sinks]]\nkind = \"http\"\nurl = \"{}/audit\"\nretries = 3\ntimeout_ms = 300\n[[audit.sinks]]\nkind = \"stderr\"\n",
            collector.url,
            dead_url()
        ),
    );
    corpus.build(&[]);
    assert!(session(&corpus, extra).success());
    let local = std::fs::read_to_string(corpus.root.join(".kiwi/state/audit.log")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let shipped = loop {
        let lines: Vec<String> = collector
            .requests()
            .iter()
            .flat_map(|r| r.body.lines().map(str::to_owned).collect::<Vec<_>>())
            .collect();
        if lines.len() >= 3 || Instant::now() > deadline {
            break lines;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let local: Vec<&str> = local.lines().collect();
    assert_eq!(local.len(), 3);
    assert_eq!(shipped, local, "the sink ships the local lines exactly");
    assert!(collector.requests().iter().all(|r| {
        r.headers
            .to_ascii_lowercase()
            .contains("content-type: application/x-ndjson")
    }));
}

#[test]
fn a_stdout_sink_cannot_share_stdio_with_the_protocol() {
    let corpus = Corpus::copy();
    configure(
        &corpus.root,
        "namespace = \"acme\"\n[[audit.sinks]]\nkind = \"stdout\"\n",
    );
    corpus.build(&[]);
    let out = Command::new(KIWI)
        .args(["serve", ".", "--principal", "alice"])
        .current_dir(&corpus.root)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("stderr sink"));
}
