//! What one corpus serves across snapshots: a pinned read (`kiwi://atom/{id}@{rev}`)
//! needs the atom now and then, never reopens text hidden from the caller now, and
//! stays in the served source's lineage; a reload swaps the pack, the policy, the
//! principals and the configuration together.

#![allow(clippy::unwrap_used)]

mod common;

use std::path::Path;
use std::sync::Arc;

use common::Corpus;
use kiwi_compiler::build::{BuildPlugins, PACK_DIR};
use kiwi_compiler::{BuildOptions, KiwiConfig, SourceMode};
use kiwi_core::plugin::RetrieverFactory;
use kiwi_core::{Pack, PackStore};
use kiwi_mcp::{CorpusPlugins, KiwiService, ServiceOptions};
use kiwi_policy::{Directory, Identity, PolicyGate};
use serde_json::{Value, json};

fn factories() -> Vec<Arc<dyn RetrieverFactory>> {
    vec![Arc::new(kiwi_bm25::Bm25Factory::new(true))]
}

/// Builds the working files; returns the snapshot.
fn build(root: &Path) -> String {
    let options = BuildOptions {
        root: root.to_owned(),
        mode: SourceMode::Worktree,
        out: Some(root.join(PACK_DIR)),
        as_of: Some("2026-11-23".into()),
    };
    let indexer = kiwi_bm25::Bm25Indexer;
    let indexers: Vec<&dyn kiwi_core::plugin::Indexer> = vec![&indexer];
    let plugins = BuildPlugins {
        indexers,
        ..BuildPlugins::default()
    };
    kiwi_compiler::build(&options, &plugins).unwrap().snapshot
}

fn config(root: &Path) -> KiwiConfig {
    KiwiConfig::parse(&std::fs::read_to_string(root.join(".kiwi/kiwi.toml")).unwrap()).unwrap()
}

fn gate(root: &Path) -> PolicyGate {
    let policy = std::fs::read_to_string(root.join(".kiwi/policy.cedar")).unwrap();
    let directory =
        Directory::parse(&std::fs::read_to_string(root.join(".kiwi/principals.yaml")).unwrap()).unwrap();
    PolicyGate::new(&policy, directory).unwrap()
}

fn service(root: &Path) -> KiwiService {
    let options = ServiceOptions {
        corpus: root.to_owned(),
        packs: root.join(PACK_DIR),
        state_dir: root.join(".kiwi/state"),
        config: config(root),
        audit: true,
        proposals: false,
        stdout_is_protocol: false,
        snapshot: None,
    };
    KiwiService::open(
        options,
        gate(root),
        CorpusPlugins {
            factories: factories(),
            reranker: None,
        },
    )
    .unwrap()
}

fn edit(path: &Path, from: &str, to: &str) {
    let text = std::fs::read_to_string(path).unwrap();
    assert!(text.contains(from), "{from}");
    std::fs::write(path, text.replace(from, to)).unwrap();
}

fn resource(service: &KiwiService, who: &Identity, id: &str, rev: &str) -> Result<String, String> {
    service
        .read_resource(who, &format!("kiwi://atom/{id}@{rev}"))
        .map_err(|e| e.to_string())
}

#[test]
fn a_pinned_read_is_governed_by_the_access_now_and_then() {
    let corpus = Corpus::copy();
    let adr = corpus.root.join("adr/adr-005-idempotency-keys.md");
    let prd = corpus.root.join("product/checkout/prd.md");
    // Snapshot 1: the decision is internal, pricing is finance-only.
    let first = build(&corpus.root);
    // Snapshot 2: the decision is narrowed to finance, pricing is opened to staff.
    edit(
        &adr,
        "## Decision {#decision}",
        "## Decision {#decision access=finance}",
    );
    edit(
        &prd,
        "## Pricing {#pricing access=finance}",
        "## Pricing {#pricing}",
    );
    let second = build(&corpus.root);
    assert_ne!(first, second);
    let service = service(&corpus.root);
    let alice = Identity::new("alice", "claude-code", "stdio");
    let fiona = Identity::new("fiona", "claude-code", "stdio");
    let decision = "adr/adr-005-idempotency-keys#decision";
    let pricing = "product/checkout/prd#pricing";

    // Alice could read the decision at snapshot 1, but it is finance-only now: the old
    // snapshot does not reopen it, and it answers like an id that does not exist.
    let narrowed = resource(&service, &alice, decision, &first).unwrap_err();
    let absent = resource(&service, &alice, "adr/nope#x", &first).unwrap_err();
    assert_eq!(
        narrowed.replace(decision, "ID"),
        absent.replace("adr/nope#x", "ID")
    );
    // Pricing is open to her now, but its text at snapshot 1 was finance-only then.
    assert!(resource(&service, &alice, pricing, &first).is_err());
    assert!(resource(&service, &alice, pricing, &second).is_ok());
    // Fiona may read both now and then.
    assert!(
        resource(&service, &fiona, decision, &first)
            .unwrap()
            .contains("## Decision")
    );
    assert!(resource(&service, &fiona, pricing, &first).is_ok());
}

#[test]
fn a_pinned_read_needs_the_atom_now_and_never_reopens_hidden_text() {
    pinned_reads_never_reopen_hidden_text(false);
}

#[test]
fn a_crlf_corpus_gets_the_same_paragraph_check() {
    pinned_reads_never_reopen_hidden_text(true);
}

fn pinned_reads_never_reopen_hidden_text(crlf: bool) {
    let corpus = Corpus::copy();
    let adr = corpus.root.join("adr/adr-005-idempotency-keys.md");
    if crlf {
        let text = std::fs::read_to_string(&adr).unwrap();
        std::fs::write(&adr, text.replace('\n', "\r\n")).unwrap();
    }
    let first = build(&corpus.root);
    let text = std::fs::read_to_string(&adr).unwrap();
    let blank = if crlf { "\r\n\r\n" } else { "\n\n" };
    let start = text
        .find("Every API endpoint that creates or changes state")
        .unwrap();
    let end = text[start..].find(blank).map_or(text.len(), |n| start + n);
    let paragraph = text[start..end].to_owned();
    // Snapshot 2: the decision's first paragraph moves into an exec-only document (with
    // LF line endings, whatever the ADR uses), the decision itself stays readable, and
    // another ADR is deleted.
    std::fs::write(
        &adr,
        text.replace(&paragraph, "Write APIs take idempotency keys."),
    )
    .unwrap();
    std::fs::create_dir_all(corpus.root.join("vault")).unwrap();
    std::fs::write(
        corpus.root.join("vault/keys.md"),
        format!(
            "---\naccess: {{ label: restricted, groups: [exec] }}\n---\n# Keys\n\n{}\n",
            paragraph.replace("\r\n", "\n")
        ),
    )
    .unwrap();
    std::fs::remove_file(corpus.root.join("adr/adr-006-integer-money.md")).unwrap();
    let second = build(&corpus.root);
    assert_ne!(first, second);
    let service = service(&corpus.root);
    let alice = Identity::new("alice", "claude-code", "stdio");
    let decision = "adr/adr-005-idempotency-keys#decision";
    // The old decision holds the paragraph that only an exec-only document holds now.
    let moved = resource(&service, &alice, decision, &first).unwrap_err();
    let absent = resource(&service, &alice, "adr/nope#x", &first).unwrap_err();
    assert_eq!(moved.replace(decision, "ID"), absent.replace("adr/nope#x", "ID"));
    assert!(resource(&service, &alice, decision, &second).is_ok());
    // A document gone since is not read through an old snapshot.
    assert!(resource(&service, &alice, "ADR-006", &first).is_err());
    // Snapshots are named in full or by at least seven hex digits.
    let hex = first.strip_prefix("dir:").unwrap();
    assert!(resource(&service, &alice, "ADR-001", &format!("dir:{}", &hex[..7])).is_ok());
    assert!(resource(&service, &alice, "ADR-001", &format!("dir:{}", &hex[..3])).is_err());
}

#[test]
fn a_pinned_read_stays_in_the_served_lineage() {
    let corpus = Corpus::git();
    let git = |args: &[&str]| -> String {
        let out = std::process::Command::new("git")
            .args(["-c", "user.email=t@test", "-c", "user.name=T"])
            .args(args)
            .current_dir(&corpus.root)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    let build_rev = |mode: SourceMode| -> String {
        let options = BuildOptions {
            root: corpus.root.clone(),
            mode,
            out: Some(corpus.root.join(PACK_DIR)),
            as_of: Some("2026-11-23".into()),
        };
        kiwi_compiler::build(&options, &BuildPlugins::default())
            .unwrap()
            .snapshot
    };
    let first = build_rev(SourceMode::Git("HEAD".into()));
    // A commit on another branch, and the working files, each built.
    git(&["checkout", "-q", "-b", "other"]);
    std::fs::write(corpus.root.join("other.md"), "# Other\n\nOn another branch.\n").unwrap();
    git(&["add", "other.md"]);
    git(&["commit", "-q", "-m", "other"]);
    let other = build_rev(SourceMode::Git("other".into()));
    git(&["checkout", "-q", "main"]);
    let dir = build_rev(SourceMode::Worktree);
    // The served commit, on main, after the first.
    git(&["commit", "-q", "--allow-empty", "-m", "later"]);
    let served = build_rev(SourceMode::Git("main".into()));
    let service = service(&corpus.root);
    assert_eq!(service.corpora()[0].engine().pack().snapshot(), served);
    let alice = Identity::new("alice", "claude-code", "stdio");
    let read = |rev: &str| resource(&service, &alice, "ADR-001", rev);
    assert!(read(&served).is_ok());
    assert!(read(&first).is_ok(), "an ancestor of the served commit");
    assert!(read(&first[..7]).is_ok());
    assert!(read(&first[..6]).is_err(), "too short");
    assert!(read(&other).is_err(), "another branch");
    assert!(read(&dir).is_err(), "the working files of a commit-served corpus");
}

#[test]
fn a_reload_swaps_pack_policy_and_configuration_together() {
    let corpus = Corpus::copy();
    build(&corpus.root);
    let service = service(&corpus.root);
    let alice = Identity::new("alice", "claude-code", "stdio");
    let read = json!({"ids": ["adr/adr-005-idempotency-keys#decision"]});
    let before: Value =
        serde_json::from_str(&service.call(&alice, "kiwi_read", read.clone()).unwrap()).unwrap();
    assert_eq!(before["atoms"].as_array().unwrap().len(), 1);

    // A new snapshot: staff lose internal content, hidden content is stubbed, and the
    // summarizer agent is narrowed to public.
    edit(
        &corpus.root.join(".kiwi/policy.cedar"),
        "permit (principal in Kiwi::Group::\"staff\", action == Kiwi::Action::\"read\", resource)",
        "permit (principal in Kiwi::Group::\"nobody\", action == Kiwi::Action::\"read\", resource)",
    );
    edit(
        &corpus.root.join(".kiwi/kiwi.toml"),
        "hidden = \"invisible\"",
        "hidden = \"stub\"",
    );
    let snapshot = build(&corpus.root);
    let pack = PackStore::new(corpus.root.join(PACK_DIR))
        .current()
        .unwrap()
        .unwrap();
    let corpus_state = &service.corpora()[0];
    corpus_state
        .replace(&pack, gate(&corpus.root), config(&corpus.root), factories())
        .unwrap();
    let after: Value = serde_json::from_str(&service.call(&alice, "kiwi_read", read).unwrap()).unwrap();
    assert_eq!(after["snapshot"], snapshot);
    assert!(after["atoms"].as_array().unwrap().is_empty(), "{after}");
    assert_eq!(after["redacted"], 1, "stub mode came with the policy: {after}");

    // A namespace change needs a restart; the state stays as it was.
    edit(
        &corpus.root.join(".kiwi/kiwi.toml"),
        "namespace = \"acme\"",
        "namespace = \"other\"",
    );
    assert!(
        corpus_state
            .replace(&pack, gate(&corpus.root), config(&corpus.root), factories())
            .is_err()
    );
    assert_eq!(corpus_state.engine().pack().snapshot(), snapshot);
    let _ = Pack::open(&pack).unwrap();
}

/// Calls racing a stream of reloads each answer whole from one snapshot: the pack's
/// text and the snapshot it names always agree, and no call fails.
#[test]
fn calls_racing_reloads_answer_from_one_snapshot() {
    let corpus = Corpus::copy();
    let notes = corpus.root.join("notes.md");
    std::fs::write(&notes, "# Notes\n\nThe Alderwick line.\n").unwrap();
    let first = build(&corpus.root);
    let first_pack = PackStore::new(corpus.root.join(PACK_DIR))
        .current()
        .unwrap()
        .unwrap();
    std::fs::write(&notes, "# Notes\n\nThe Brindlemoor line.\n").unwrap();
    let second = build(&corpus.root);
    let second_pack = PackStore::new(corpus.root.join(PACK_DIR))
        .current()
        .unwrap()
        .unwrap();
    let service = Arc::new(service(&corpus.root));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let callers: Vec<_> = (0..4)
        .map(|_| {
            let service = Arc::clone(&service);
            let stop = Arc::clone(&stop);
            let (first, second) = (first.clone(), second.clone());
            std::thread::spawn(move || {
                let alice = Identity::new("alice", "claude-code", "stdio");
                let mut seen = std::collections::BTreeSet::new();
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let text = service
                        .call(&alice, "kiwi_read", json!({"ids": ["notes"]}))
                        .unwrap();
                    let read: Value = serde_json::from_str(&text).unwrap();
                    let snapshot = read["snapshot"].as_str().unwrap().to_owned();
                    let body = read["atoms"][0]["text"].as_str().unwrap();
                    let expected = if snapshot == first {
                        "Alderwick"
                    } else {
                        assert_eq!(snapshot, second);
                        "Brindlemoor"
                    };
                    assert!(body.contains(expected), "{snapshot}: {body}");
                    let search = service
                        .call(
                            &alice,
                            "kiwi_search",
                            json!({"queries": ["Alderwick Brindlemoor"]}),
                        )
                        .unwrap();
                    let search: Value = serde_json::from_str(&search).unwrap();
                    assert!(search["snapshot"] == first.as_str() || search["snapshot"] == second.as_str());
                    seen.insert(snapshot);
                }
                seen
            })
        })
        .collect();
    let corpus_state = &service.corpora()[0];
    for round in 0..200 {
        let pack = if round % 2 == 0 { &first_pack } else { &second_pack };
        corpus_state
            .replace(pack, gate(&corpus.root), config(&corpus.root), factories())
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let mut seen = std::collections::BTreeSet::new();
    for caller in callers {
        seen.extend(caller.join().unwrap());
    }
    assert_eq!(seen.len(), 2, "both snapshots were served: {seen:?}");
}
