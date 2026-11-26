//! `kiwi serve --follow`: a server picks up the packs another process builds, with no
//! restart and no failed call, alone or federated with a second corpus.

#![cfg(feature = "http")]
#![allow(clippy::unwrap_used, clippy::zombie_processes)]

mod common;

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::{Corpus, KIWI};
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};

struct Server {
    child: Child,
    url: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `kiwi serve <args> --follow` over HTTP on a free loopback port, as alice.
fn serve(root: &Path, args: &[&str]) -> Server {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let child = Command::new(KIWI)
        .arg("serve")
        .args(args)
        .args([
            "--follow",
            "--http",
            &format!("127.0.0.1:{port}"),
            "--insecure-principal",
            "alice",
            "--agent",
            "claude-code",
        ])
        .current_dir(root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..200 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Server {
                child,
                url: format!("http://127.0.0.1:{port}/mcp"),
            };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("server did not start");
}

async fn connect(server: &Server) -> RunningService<RoleClient, ()> {
    ().serve(StreamableHttpClientTransport::from_uri(server.url.clone()))
        .await
        .unwrap()
}

/// A tool call's text, or why it failed.
async fn call(client: &RunningService<RoleClient, ()>, tool: &str, args: &Value) -> Result<String, String> {
    let params =
        CallToolRequestParams::new(tool.to_owned()).with_arguments(args.as_object().unwrap().clone());
    let result = client.call_tool(params).await.map_err(|e| e.to_string())?;
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default();
    if result.is_error == Some(true) {
        Err(text)
    } else {
        Ok(text)
    }
}

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "user.email=t@test", "-c", "user.name=T"])
        .args(args)
        .current_dir(root)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// Adds `line` as a paragraph of the document `notes` and commits it; returns the
/// commit.
fn commit_line(root: &Path, line: &str) -> String {
    let notes = root.join("notes.md");
    let mut text = std::fs::read_to_string(&notes).unwrap_or_else(|_| "# Notes\n".to_owned());
    text.push_str(&format!("\n{line}\n"));
    std::fs::write(&notes, text).unwrap();
    git(root, &["add", "notes.md"]);
    git(root, &["commit", "-q", "-m", line]);
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn kiwi(root: &Path, args: &[&str]) {
    let out = Command::new(KIWI).args(args).current_dir(root).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

/// Calls `tool` until `holds` accepts the answer.
async fn eventually(
    client: &RunningService<RoleClient, ()>,
    tool: &str,
    args: Value,
    holds: impl Fn(&str) -> bool,
) -> String {
    let mut last = String::new();
    for _ in 0..100 {
        let text = call(client, tool, &args).await.unwrap();
        if holds(&text) {
            return text;
        }
        last = text;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("never held: {last}");
}

/// Builds made by another process are served while clients keep calling: every call
/// succeeds, and each answer comes whole from one snapshot (its text is that commit's).
#[tokio::test(flavor = "multi_thread")]
async fn new_packs_are_served_with_no_failed_call() {
    let corpus = Corpus::git();
    corpus.build(&[]);
    let server = serve(&corpus.root, &["."]);
    let read = json!({"ids": ["notes"]});
    let stop = Arc::new(AtomicBool::new(false));
    let mut clients = Vec::new();
    for _ in 0..4 {
        let client = connect(&server).await;
        let stop = Arc::clone(&stop);
        let read = read.clone();
        clients.push(tokio::spawn(async move {
            let mut answers: Vec<(String, String)> = Vec::new();
            let mut failures: Vec<String> = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                for (tool, args) in [
                    ("kiwi_read", read.clone()),
                    ("kiwi_search", json!({"queries": ["Wrenfield notes"]})),
                ] {
                    match call(&client, tool, &args).await {
                        Ok(text) => {
                            let answer: Value = serde_json::from_str(&text).unwrap();
                            let snapshot = answer["snapshot"].as_str().unwrap().to_owned();
                            if tool == "kiwi_read" {
                                let text = answer["atoms"][0]["text"].as_str().unwrap_or_default();
                                answers.push((snapshot, text.to_owned()));
                            }
                        }
                        Err(error) => failures.push(error),
                    }
                }
            }
            client.cancel().await.unwrap();
            (answers, failures)
        }));
    }

    let watcher = connect(&server).await;
    let mut lines = BTreeSet::new();
    let mut commits = Vec::new();
    for round in 0..3 {
        let line = format!("Wrenfield round {round} was added.");
        let commit = commit_line(&corpus.root, &line);
        corpus.build(&[]);
        let expected = commit.clone();
        eventually(&watcher, "kiwi_read", read.clone(), move |t| {
            t.contains(expected.as_str())
        })
        .await;
        lines.insert(line);
        commits.push(commit);
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    stop.store(true, Ordering::Relaxed);
    watcher.cancel().await.unwrap();

    let mut seen = BTreeSet::new();
    for client in clients {
        let (answers, failures) = client.await.unwrap();
        assert!(failures.is_empty(), "failed calls: {failures:?}");
        assert!(!answers.is_empty());
        for (snapshot, text) in answers {
            // The text holds exactly the lines committed up to its snapshot.
            let rounds = match commits.iter().position(|c| *c == snapshot) {
                Some(index) => index + 1,
                None => 0,
            };
            for (round, line) in lines.iter().enumerate() {
                assert_eq!(
                    text.contains(line.as_str()),
                    round < rounds,
                    "{snapshot}: {line} in {text}"
                );
            }
            seen.insert(snapshot);
        }
    }
    assert!(seen.contains(commits.last().unwrap()), "{seen:?}");
}

/// A following server starts on the pack its store's CURRENT names, even when HEAD has
/// moved on and is not built yet, and serves HEAD once it is; SIGHUP checks at once and
/// does not stop it.
#[tokio::test(flavor = "multi_thread")]
async fn starts_on_the_current_pack_and_survives_sighup() {
    let corpus = Corpus::git();
    corpus.build(&[]);
    let built = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&corpus.root)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    let head = commit_line(&corpus.root, "Wrenfield was added after the build.");
    let server = serve(&corpus.root, &["."]);
    let client = connect(&server).await;
    let read = json!({"ids": ["vision"]});
    let first: Value = serde_json::from_str(&call(&client, "kiwi_read", &read).await.unwrap()).unwrap();
    assert_eq!(first["snapshot"], built.as_str());

    #[cfg(unix)]
    {
        let status = Command::new("kill")
            .args(["-HUP", &server.child.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        tokio::time::sleep(Duration::from_millis(300)).await;
        let again: Value = serde_json::from_str(&call(&client, "kiwi_read", &read).await.unwrap()).unwrap();
        assert_eq!(again["snapshot"], built.as_str());
    }

    corpus.build(&[]);
    let expected = head.clone();
    eventually(&client, "kiwi_read", read, move |t| t.contains(expected.as_str())).await;
    client.cancel().await.unwrap();
}

/// Two corpora, each built by its own process into `<packs>/<namespace>`: one server
/// federates them, follows each store on its own, and keeps each corpus's state in
/// `<state>/<namespace>`.
#[tokio::test(flavor = "multi_thread")]
async fn federated_stores_are_followed_by_namespace() {
    let kb = Corpus::git();
    let catalogue = Corpus::git();
    let toml = catalogue.root.join(".kiwi/kiwi.toml");
    let text = std::fs::read_to_string(&toml).unwrap();
    std::fs::write(
        &toml,
        text.replace("namespace = \"acme\"", "namespace = \"catalogue\""),
    )
    .unwrap();
    git(&catalogue.root, &["commit", "-q", "-am", "catalogue"]);
    let dir = tempfile::tempdir().unwrap();
    let packs = dir.path().join("packs");
    let state = dir.path().join("state");
    for (root, namespace) in [(&kb.root, "acme"), (&catalogue.root, "catalogue")] {
        let store = packs.join(namespace);
        kiwi(
            root,
            &[
                "build",
                ".",
                "--embedder",
                "hash",
                "--packs",
                store.to_str().unwrap(),
            ],
        );
    }
    let server = serve(
        dir.path(),
        &[
            kb.root.to_str().unwrap(),
            catalogue.root.to_str().unwrap(),
            "--packs",
            packs.to_str().unwrap(),
            "--state",
            state.to_str().unwrap(),
        ],
    );
    let client = connect(&server).await;
    let search = json!({"queries": ["Wrenfield"], "filters": {"namespace": ["catalogue"]}});
    let before: Value = serde_json::from_str(&call(&client, "kiwi_search", &search).await.unwrap()).unwrap();
    assert!(!before.to_string().contains("catalogue:notes"), "{before}");

    let commit = commit_line(&catalogue.root, "Wrenfield is a new catalogue item.");
    let store = packs.join("catalogue");
    kiwi(
        &catalogue.root,
        &[
            "build",
            ".",
            "--embedder",
            "hash",
            "--packs",
            store.to_str().unwrap(),
        ],
    );
    let text = eventually(&client, "kiwi_search", search, |t| t.contains("catalogue:notes")).await;
    let after: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(after["snapshots"]["catalogue"], commit.as_str(), "{after}");
    client.cancel().await.unwrap();
    assert!(state.join("acme/audit.log").exists());
    assert!(state.join("catalogue/audit.log").exists());
}

/// A following server only reads its pack store: proposals still open when the store
/// is read-only to it (their compile cache is skipped, not required).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn proposals_open_with_a_read_only_store() {
    use std::os::unix::fs::PermissionsExt;
    let corpus = Corpus::git();
    corpus.build(&[]);
    let packs = corpus.root.join(".kiwi/packs");
    let cache = packs.join("cache");
    std::fs::remove_dir_all(&cache).unwrap();
    std::fs::set_permissions(&packs, std::fs::Permissions::from_mode(0o555)).unwrap();
    let server = serve(&corpus.root, &["."]);
    let client = connect(&server).await;
    let doc = "---\nid: ADR-009\ntype: adr\nstatus: draft\nsummary: Retry budgets for outbound calls.\n---\n\n# Retry budgets\n\nEvery outbound call gets a retry budget of three attempts with jittered backoff.\n";
    let answer = call(
        &client,
        "kiwi_propose",
        &json!({"changes": [{"path": "adr/adr-009-retry-budgets.md", "content": doc}], "rationale": "Retries."}),
    )
    .await;
    client.cancel().await.unwrap();
    std::fs::set_permissions(&packs, std::fs::Permissions::from_mode(0o755)).unwrap();
    let proposal: Value = serde_json::from_str(&answer.unwrap()).unwrap();
    assert!(
        proposal["branch"]
            .as_str()
            .unwrap()
            .starts_with("kiwi/propose/alice/"),
        "{proposal}"
    );
}
