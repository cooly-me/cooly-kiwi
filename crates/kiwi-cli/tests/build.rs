//! `kiwi build`: a pack per snapshot, and nothing recompiled or rewritten without change.

#![allow(clippy::unwrap_used)]

mod common;

use common::Corpus;

#[test]
fn rebuild_without_change_compiles_and_writes_nothing() {
    let corpus = Corpus::copy();
    let first = String::from_utf8(corpus.build(&["--json"]).stdout).unwrap();
    let first: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert!(first["atoms"].as_u64().unwrap() > 100);
    assert_eq!(first["atoms_reused"], 0);
    assert_eq!(first["written"], true);
    let second: serde_json::Value = serde_json::from_slice(&corpus.build(&["--json"]).stdout).unwrap();
    assert_eq!(second["atoms_rebuilt"], 0, "no atom is compiled again");
    assert_eq!(second["written"], false, "the identical pack is not rewritten");
    assert_eq!(second["hash"], first["hash"]);

    // One changed file recompiles only its own atoms.
    let path = corpus.root.join("adr/adr-006-integer-money.md");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("\nOne more sentence.\n");
    std::fs::write(&path, text).unwrap();
    let third: serde_json::Value = serde_json::from_slice(&corpus.build(&["--json"]).stdout).unwrap();
    let rebuilt = third["atoms_rebuilt"].as_u64().unwrap();
    assert!(rebuilt > 0 && rebuilt < 10, "rebuilt {rebuilt}");
}

#[test]
fn git_source_pins_the_commit() {
    let corpus = Corpus::git();
    let head = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&corpus.root)
        .output()
        .unwrap();
    let head = String::from_utf8(head.stdout).unwrap().trim().to_owned();
    let report: serde_json::Value = serde_json::from_slice(&corpus.build(&["--json"]).stdout).unwrap();
    assert_eq!(report["source"], "git");
    assert_eq!(report["snapshot"], head);
    // Uncommitted edits are not part of the commit's pack.
    std::fs::write(corpus.root.join("adr/new.md"), "# New\n\nUncommitted.\n").unwrap();
    let again: serde_json::Value = serde_json::from_slice(&corpus.build(&["--json"]).stdout).unwrap();
    assert_eq!(again["atoms"], report["atoms"]);
}

#[test]
fn no_default_features_build_still_navigates() {
    // The binary under test may have every plugin; navigation must not depend on any:
    // a pack without plugin segments answers map and read.
    let corpus = Corpus::copy();
    corpus.build(&["--no-bm25", "--no-vec"]);
    let map = corpus.query("kiwi_map", r#"{"scope":"adr/","depth":1}"#, "alice");
    assert!(map.contains("adr/adr-005-idempotency-keys"));
    let read = corpus.query("kiwi_read", r#"{"ids":["ADR-005"],"level":"L1"}"#, "alice");
    assert!(read.contains("\"level\":\"L1\""));
}

#[test]
fn builds_keep_only_the_newest_packs() {
    let corpus = Corpus::copy();
    let config = corpus.root.join(".kiwi/kiwi.toml");
    let mut toml = std::fs::read_to_string(&config).unwrap();
    toml.push_str("\n[packs]\nkeep = 2\n");
    std::fs::write(&config, toml).unwrap();
    let packs = || {
        std::fs::read_dir(corpus.root.join(".kiwi/packs"))
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "kpack")
            })
            .count()
    };
    let path = corpus.root.join("adr/adr-006-integer-money.md");
    let mut hashes = Vec::new();
    for n in 0..4 {
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(&format!("\nRevision {n} of this decision.\n"));
        std::fs::write(&path, text).unwrap();
        let report: serde_json::Value = serde_json::from_slice(&corpus.build(&["--json"]).stdout).unwrap();
        hashes.push(report["hash"].as_str().unwrap().to_owned());
        assert!(packs() <= 2, "after build {n}: {} packs", packs());
    }
    let current = std::fs::read_to_string(corpus.root.join(".kiwi/packs/CURRENT")).unwrap();
    assert_eq!(current.trim(), hashes[3]);
    let out = corpus.kiwi(&["gc", ".", "--keep", "1"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(packs(), 1);
    assert!(
        corpus
            .root
            .join(format!(".kiwi/packs/{}.kpack", hashes[3]))
            .exists()
    );
}

fn git(root: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(["-c", "user.email=t@test", "-c", "user.name=T"])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[test]
fn a_revision_is_served_only_from_its_own_pack() {
    let corpus = Corpus::git();
    let first = git(&corpus.root, &["rev-parse", "HEAD"]);
    corpus.build(&[]);
    // A second commit, built last, so its pack is the current one.
    std::fs::write(
        corpus.root.join("vision-addendum.md"),
        "---\naccess: { label: public }\n---\n# Addendum\n\nThe Fernwhistle programme starts next year.\n",
    )
    .unwrap();
    git(&corpus.root, &["add", "vision-addendum.md"]);
    git(&corpus.root, &["commit", "-q", "-m", "second"]);
    corpus.build(&[]);
    let read = |rev: &str| {
        corpus.kiwi(&[
            "query",
            ".",
            "kiwi_read",
            r#"{"ids": ["vision-addendum"]}"#,
            "--principal",
            "alice",
            "--rev",
            rev,
        ])
    };
    // The first commit is answered from its own pack, under its own policy.
    let old = read(&first);
    assert!(old.status.success(), "{}", String::from_utf8_lossy(&old.stderr));
    let old = String::from_utf8(old.stdout).unwrap();
    assert!(old.contains(&first), "{old}");
    assert!(!old.contains("Fernwhistle"), "{old}");
    assert!(
        String::from_utf8(read("HEAD").stdout)
            .unwrap()
            .contains("Fernwhistle")
    );
    // A revision without a pack is refused, never served from the current pack.
    git(&corpus.root, &["commit", "-q", "--allow-empty", "-m", "third"]);
    let unbuilt = read("HEAD");
    assert!(!unbuilt.status.success());
    assert!(
        String::from_utf8_lossy(&unbuilt.stderr).contains("no pack of snapshot"),
        "{}",
        String::from_utf8_lossy(&unbuilt.stderr)
    );
    let export = corpus.kiwi(&["export", ".", "--principal", "alice", "--rev", &first]);
    assert!(
        export.status.success(),
        "{}",
        String::from_utf8_lossy(&export.stderr)
    );
    assert!(!String::from_utf8_lossy(&export.stdout).contains("Fernwhistle"));
}

#[test]
fn working_files_are_served_only_from_their_own_pack() {
    let corpus = Corpus::copy();
    corpus.build(&[]);
    assert!(
        corpus
            .kiwi(&["query", ".", "kiwi_map", "{}", "--principal", "alice"])
            .status
            .success()
    );
    // A policy changed after the build governs only a pack built from it.
    let policy = corpus.root.join(".kiwi/policy.cedar");
    let mut text = std::fs::read_to_string(&policy).unwrap();
    text.push_str("\n// changed\n");
    std::fs::write(&policy, text).unwrap();
    let stale = corpus.kiwi(&["query", ".", "kiwi_map", "{}", "--principal", "alice"]);
    assert!(!stale.status.success());
    corpus.build(&[]);
    assert!(
        corpus
            .kiwi(&["query", ".", "kiwi_map", "{}", "--principal", "alice"])
            .status
            .success()
    );
}

#[test]
fn watching_serves_the_working_files_never_a_revision() {
    let corpus = Corpus::copy();
    let out = corpus.kiwi(&["serve", ".", "--watch", "--rev", "HEAD"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cannot be used with"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn packs_and_state_stay_out_of_git() {
    let corpus = Corpus::git();
    corpus.build(&[]);
    corpus.query("kiwi_map", "{}", "alice");
    for dir in [".kiwi/packs", ".kiwi/state"] {
        assert!(corpus.root.join(dir).join(".gitignore").exists(), "{dir}");
    }
    let status = git(&corpus.root, &["status", "--porcelain"]);
    assert!(status.is_empty(), "{status}");
}

#[test]
fn proposals_need_a_server_of_a_commit() {
    let corpus = Corpus::git();
    corpus.build(&["--worktree"]);
    let out = corpus.kiwi(&[
        "query",
        ".",
        "kiwi_propose",
        r#"{"changes": [{"section": "ADR-001", "content": "New text."}], "rationale": "r"}"#,
        "--principal",
        "paul",
        "--worktree",
    ]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("server of a commit"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
