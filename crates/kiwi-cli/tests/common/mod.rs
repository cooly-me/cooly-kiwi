//! Shared helpers: a private copy of the fixture corpus, built with the hash embedder.

#![allow(dead_code, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub const KIWI: &str = env!("CARGO_BIN_EXE_kiwi");

/// Words that appear only in sections the example policy keeps from network plugins
/// (it lets them read public content only).
pub const HIDDEN_WORDS: [&str; 4] = ["Halcyon", "Loomwright", "Tern", "Northwind"];

pub fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub fn fixture() -> PathBuf {
    workspace().join("fixtures/acme-kb")
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name == "packs" || name == "state" {
            continue;
        }
        let target = to.join(&name);
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// A temporary copy of the corpus.
pub struct Corpus {
    _dir: tempfile::TempDir,
    pub root: PathBuf,
}

impl Corpus {
    pub fn copy() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("acme-kb");
        copy_tree(&fixture(), &root);
        Self { _dir: dir, root }
    }

    /// A copy committed to a fresh Git repository.
    pub fn git() -> Self {
        let corpus = Self::copy();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["add", "-A"],
            vec![
                "-c",
                "user.email=t@test",
                "-c",
                "user.name=T",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        ] {
            let status = Command::new("git")
                .args(&args)
                .current_dir(&corpus.root)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        }
        corpus
    }

    pub fn kiwi(&self, args: &[&str]) -> Output {
        Command::new(KIWI)
            .args(args)
            .current_dir(&self.root)
            .output()
            .unwrap()
    }

    /// `kiwi build` with the hash embedder (no model download).
    pub fn build(&self, extra: &[&str]) -> Output {
        let mut args = vec!["build", ".", "--embedder", "hash"];
        args.extend_from_slice(extra);
        let out = self.kiwi(&args);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        out
    }

    pub fn query(&self, tool: &str, args: &str, principal: &str) -> String {
        let out = self.kiwi(&["query", ".", tool, args, "--principal", principal]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }
}

/// Runs `kiwi` in the corpus with extra environment variables.
pub fn kiwi_with_env(corpus: &Corpus, args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    let mut command = Command::new(KIWI);
    command.args(args).current_dir(&corpus.root);
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().unwrap()
}

/// One request a [`MockServer`] received.
#[derive(Debug, Clone)]
pub struct Received {
    pub path: String,
    pub headers: String,
    pub body: String,
}

type Handler = dyn Fn(&Received) -> (u16, String) + Send + Sync;

/// A local HTTP server that records every request and answers through a handler.
pub struct MockServer {
    pub url: String,
    pub received: std::sync::Arc<std::sync::Mutex<Vec<Received>>>,
}

impl MockServer {
    pub fn start(handler: impl Fn(&Received) -> (u16, String) + Send + Sync + 'static) -> Self {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let received = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = std::sync::Arc::clone(&received);
        let handler: std::sync::Arc<Handler> = std::sync::Arc::new(handler);
        std::thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                let log = std::sync::Arc::clone(&log);
                let handler = std::sync::Arc::clone(&handler);
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream);
                    let mut first = String::new();
                    if reader.read_line(&mut first).unwrap_or(0) == 0 {
                        return;
                    }
                    let path = first.split_whitespace().nth(1).unwrap_or("").to_owned();
                    let mut headers = String::new();
                    let mut length = 0usize;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                            length = v.trim().parse().unwrap_or(0);
                        }
                        headers.push_str(&line);
                    }
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).unwrap();
                    let request = Received {
                        path,
                        headers,
                        body: String::from_utf8_lossy(&body).into_owned(),
                    };
                    let (status, answer) = handler(&request);
                    log.lock().unwrap().push(request);
                    let mut stream = reader.into_inner();
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{answer}",
                        answer.len()
                    );
                });
            }
        });
        Self { url, received }
    }

    pub fn requests(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }
}

/// An address nothing listens on.
pub fn dead_url() -> String {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    format!("http://127.0.0.1:{port}")
}

/// Replaces `.kiwi/kiwi.toml` of a corpus.
pub fn configure(root: &Path, toml: &str) {
    std::fs::write(root.join(".kiwi/kiwi.toml"), toml).unwrap();
}

/// A deterministic toy embedding: word buckets of the folded text, so related texts
/// land near each other.
pub fn toy_embedding(text: &str) -> Vec<f32> {
    let mut v = vec![0.0f32; 64];
    for word in text.to_lowercase().split(|c: char| !c.is_alphanumeric()) {
        if word.len() > 2 {
            let h = word
                .bytes()
                .fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)));
            v[(h % 64) as usize] += 1.0;
        }
    }
    v[63] += 0.01;
    v
}

/// An OpenAI-style embeddings answer for a request body.
pub fn embeddings_answer(body: &str) -> String {
    let request: serde_json::Value = serde_json::from_str(body).unwrap();
    let inputs: Vec<String> = serde_json::from_value(request["input"].clone()).unwrap();
    let data: Vec<serde_json::Value> = inputs
        .iter()
        .enumerate()
        .map(|(index, text)| serde_json::json!({"index": index, "embedding": toy_embedding(text)}))
        .collect();
    serde_json::json!({"object": "list", "data": data}).to_string()
}
