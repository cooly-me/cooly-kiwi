//! The six MCP tools over stdio, through a real MCP client and a `kiwi serve` process.

#![allow(clippy::unwrap_used)]

mod common;

use common::{Corpus, KIWI};
use rmcp::model::{CallToolRequestParams, ReadResourceRequestParams};
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};

async fn connect(corpus: &Corpus, principal: &str, agent: &str) -> RunningService<RoleClient, ()> {
    let mut command = tokio::process::Command::new(KIWI);
    command
        .args(["serve", ".", "--principal", principal, "--agent", agent])
        .current_dir(&corpus.root);
    let transport = TokioChildProcess::new(command).unwrap();
    ().serve(transport).await.unwrap()
}

async fn call(client: &RunningService<RoleClient, ()>, tool: &str, args: Value) -> (String, bool) {
    let params =
        CallToolRequestParams::new(tool.to_owned()).with_arguments(args.as_object().unwrap().clone());
    let result = client.call_tool(params).await.unwrap();
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default();
    (text, result.is_error == Some(true))
}

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("{e}: {text}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn lists_six_tools_and_the_atom_template() {
    let corpus = Corpus::copy();
    corpus.build(&[]);
    let client = connect(&corpus, "alice", "claude-code").await;
    let tools = client.list_all_tools().await.unwrap();
    let mut names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "kiwi_feedback",
            "kiwi_map",
            "kiwi_propose",
            "kiwi_read",
            "kiwi_search",
            "kiwi_trace"
        ]
    );
    let templates = client.list_resource_templates(None).await.unwrap();
    assert_eq!(
        templates.resource_templates[0].uri_template,
        "kiwi://atom/{id}@{rev}"
    );
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn map_read_and_resource() {
    let corpus = Corpus::copy();
    corpus.build(&[]);
    let client = connect(&corpus, "alice", "claude-code").await;
    let (text, error) = call(
        &client,
        "kiwi_map",
        json!({"scope": "adr/", "depth": 2, "budget": 2000}),
    )
    .await;
    assert!(!error, "{text}");
    let map = json(&text);
    assert_eq!(map["scope"], "adr/");
    assert_eq!(map["nodes"].as_array().unwrap().len(), 8);
    assert!(map["budget"]["used"].as_u64().unwrap() <= 2000);

    let (text, _) = call(
        &client,
        "kiwi_read",
        json!({"ids": ["ADR-005", "product/checkout/prd#pricing", "nope#x"], "context": "children"}),
    )
    .await;
    let read = json(&text);
    let atom = &read["atoms"][0];
    assert_eq!(atom["id"], "adr/adr-005-idempotency-keys");
    assert_eq!(atom["level"], "L2");
    assert!(!atom["context"].as_array().unwrap().is_empty());
    // Hidden and absent ids are reported alike.
    assert_eq!(
        read["not_found"],
        json!(["product/checkout/prd#pricing", "nope#x"])
    );
    assert!(read.get("redacted").is_none());

    let cite = atom["cite"].as_str().unwrap().to_owned();
    let resource = client
        .read_resource(ReadResourceRequestParams::new(cite.replace(
            "kiwi://atom/adr/adr-005-idempotency-keys",
            "kiwi://atom/adr/adr-005-idempotency-keys#decision",
        )))
        .await
        .unwrap();
    let body = serde_json::to_string(&resource.contents).unwrap();
    assert!(body.contains("## Decision"), "{body}");
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn search_is_budgeted_governed_and_byte_stable() {
    let corpus = Corpus::copy();
    corpus.build(&[]);
    let args = json!({"queries": ["ADR-005", "double charge prevention idempotency"], "budget": 600});
    let first = connect(&corpus, "alice", "claude-code").await;
    let (a, _) = call(&first, "kiwi_search", args.clone()).await;
    let (b, _) = call(&first, "kiwi_search", args.clone()).await;
    first.cancel().await.unwrap();
    let second = connect(&corpus, "alice", "claude-code").await;
    let (c, _) = call(&second, "kiwi_search", args.clone()).await;
    second.cancel().await.unwrap();
    assert_eq!(a, b, "same process, same bytes");
    assert_eq!(a, c, "another process, same bytes");
    let search = json(&a);
    // The budget is for each query.
    assert_eq!(search["budget"]["limit"], 1200);
    assert!(search["budget"]["used"].as_u64().unwrap() <= 1200);
    let ids: Vec<&str> = search["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert!(ids.iter().any(|id| id.starts_with("adr/adr-005")), "{ids:?}");
    assert_eq!(search["results"][0]["level"], "L1");

    // The same query for a contractor: nothing internal.
    let carl = connect(&corpus, "carl", "claude-code").await;
    let (text, _) = call(&carl, "kiwi_search", args).await;
    let search = json(&text);
    for result in search["results"].as_array().unwrap() {
        let id = result["id"].as_str().unwrap();
        assert!(id.starts_with("glossary") || id.starts_with("vision"), "{id}");
    }
    let (text, error) = call(&carl, "kiwi_search", json!({"queries": ["x"], "bogus": 1})).await;
    assert!(error, "unknown fields are refused: {text}");
    // A field named for another one is refused with the call the tool takes.
    let (text, error) = call(&carl, "kiwi_read", json!({"id": "vision"})).await;
    assert!(error, "{text}");
    assert!(text.contains(r#"{"ids":["vision"]}"#), "{text}");
    carl.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn search_and_map_by_type_standard() {
    let corpus = Corpus::copy();
    corpus.build(&[]);
    let client = connect(&corpus, "alice", "claude-code").await;

    let tools = client.list_all_tools().await.unwrap();
    let search_tool = tools.iter().find(|t| t.name == "kiwi_search").unwrap();
    let schema = serde_json::to_string(&search_tool.input_schema).unwrap();
    assert!(schema.contains("`standard`"), "{schema}");
    assert!(
        search_tool
            .description
            .as_deref()
            .unwrap_or("")
            .contains("standard")
    );

    let ids_of = |text: &str| -> Vec<String> {
        json(text)["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_owned())
            .collect()
    };

    // Exact queries, so the test holds with every retrieval plugin off: a heading of the
    // standard and an ADR's id. Unfiltered, both match; each type filter keeps its own.
    let queries = json!(["Code review", "ADR-005"]);
    let search =
        |filters: Value| json!({"queries": queries, "filters": filters, "budget": 4000, "limit": 50});
    let (text, error) = call(&client, "kiwi_search", search(json!({}))).await;
    assert!(!error, "{text}");
    let ids = ids_of(&text);
    assert!(ids.iter().any(|id| id.starts_with("adr/adr-005")), "{ids:?}");
    assert!(
        ids.iter()
            .any(|id| id == "standards/engineering-quality#code-review"),
        "{ids:?}"
    );

    let (text, error) = call(&client, "kiwi_search", search(json!({"type": ["standard"]}))).await;
    assert!(!error, "{text}");
    let ids = ids_of(&text);
    assert!(
        ids.iter()
            .any(|id| id == "standards/engineering-quality#code-review"),
        "{ids:?}"
    );
    assert!(
        ids.iter()
            .all(|id| id.starts_with("standards/engineering-quality")),
        "{ids:?}"
    );

    let (text, _) = call(&client, "kiwi_search", search(json!({"type": ["adr"]}))).await;
    let ids = ids_of(&text);
    assert!(ids.iter().any(|id| id.starts_with("adr/adr-005")), "{ids:?}");
    assert!(ids.iter().all(|id| id.starts_with("adr/")), "{ids:?}");

    let (text, _) = call(
        &client,
        "kiwi_search",
        search(json!({"type": ["standard", "adr"]})),
    )
    .await;
    let ids = ids_of(&text);
    assert!(ids.iter().any(|id| id.starts_with("adr/adr-005")), "{ids:?}");
    assert!(ids.iter().any(|id| id.starts_with("standards/")), "{ids:?}");
    assert!(
        ids.iter()
            .all(|id| id.starts_with("adr/") || id.starts_with("standards/")),
        "{ids:?}"
    );

    // Filters alone list the standard's sections.
    let (text, _) = call(
        &client,
        "kiwi_search",
        json!({"filters": {"type": ["standard"]}, "budget": 4000}),
    )
    .await;
    let ids = ids_of(&text);
    assert!(
        ids.contains(&"standards/engineering-quality#releases".to_owned()),
        "{ids:?}"
    );
    assert!(ids.iter().all(|id| id.starts_with("standards/")), "{ids:?}");

    let (text, error) = call(&client, "kiwi_map", json!({"scope": "standards/"})).await;
    assert!(!error, "{text}");
    assert!(text.contains("standard · approved"), "{text}");
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn trace_walks_prd_rfc_adr_code() {
    let corpus = Corpus::copy();
    corpus.build(&[]);
    let client = connect(&corpus, "alice", "claude-code").await;
    let (text, error) = call(
        &client,
        "kiwi_trace",
        json!({"id": "PRD-001", "direction": "both", "depth": 2}),
    )
    .await;
    assert!(!error, "{text}");
    let trace = json(&text);
    let nodes: Vec<&str> = trace["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap())
        .collect();
    assert!(nodes.contains(&"rfcs/rfc-010-checkout-saga"), "{nodes:?}");
    assert!(nodes.contains(&"adr/adr-007-async-capture"), "{nodes:?}");
    assert!(nodes.iter().any(|n| n.starts_with("code:")), "{nodes:?}");
    let edges = trace["edges"].as_array().unwrap();
    assert!(
        edges
            .iter()
            .any(|e| e["rel"] == "implements" && e["from"] == "rfcs/rfc-010-checkout-saga")
    );
    let (text, error) = call(
        &client,
        "kiwi_trace",
        json!({"id": "ADR-007", "relation": "supersedes", "direction": "out"}),
    )
    .await;
    assert!(!error);
    assert!(text.contains("adr/adr-003-sync-capture"));
    let (_, error) = call(&client, "kiwi_trace", json!({"id": "legal/dpa-negotiation"})).await;
    assert!(error, "hidden start atoms are unknown");
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn feedback_feeds_the_gap_queue() {
    let corpus = Corpus::copy();
    corpus.build(&[]);
    let client = connect(&corpus, "alice", "claude-code").await;
    let (text, error) = call(
        &client,
        "kiwi_feedback",
        json!({"query": "how do refunds reach the bank", "ids": ["product/checkout/prd#pricing"], "verdict": "missing"}),
    )
    .await;
    assert!(!error, "{text}");
    assert_eq!(json(&text)["gap"], true);
    let (_, _) = call(&client, "kiwi_search", json!({"queries": ["zzzzqqq"]})).await;
    let (_, error) = call(
        &client,
        "kiwi_feedback",
        json!({"query": "q", "verdict": "excellent"}),
    )
    .await;
    assert!(error);
    client.cancel().await.unwrap();
    let gaps = corpus.kiwi(&["gaps", ".", "--principal", "alice"]);
    let gaps = String::from_utf8(gaps.stdout).unwrap();
    assert!(gaps.contains("how do refunds reach the bank"), "{gaps}");
    assert!(gaps.contains("zzzzqqq"), "{gaps}");
    // A writer who sees less than alice gets her reports counted, not shown: a query
    // can tell what its asker read.
    let carl = corpus.kiwi(&["gaps", ".", "--principal", "carl"]);
    let carl = String::from_utf8(carl.stdout).unwrap();
    assert!(!carl.contains("refunds") && !carl.contains("zzzzqqq"), "{carl}");
    assert!(carl.contains("2 report(s) withheld"), "{carl}");
    // The hidden id is never written down.
    let log = std::fs::read_to_string(corpus.root.join(".kiwi/state/feedback.jsonl")).unwrap();
    assert!(!log.contains("pricing"), "{log}");
}

#[tokio::test(flavor = "multi_thread")]
async fn propose_opens_a_branch_with_lint() {
    let corpus = Corpus::git();
    corpus.build(&[]);
    let client = connect(&corpus, "alice", "claude-code").await;
    let doc = "---\nid: ADR-009\ntype: adr\nstatus: draft\nowner: \"@team-platform\"\nsummary: Retry budgets for outbound calls.\nreview_by: 2027-06-01\n---\n\n# Retry budgets\n\nSee [ADR-005](adr-005-idempotency-keys.md) and [gone](missing.md).\n\n## Decision {#decision}\n\nEvery outbound call gets a retry budget of three attempts with jittered backoff, and idempotency keys make the retries safe for writes.\n";
    let (text, error) = call(
        &client,
        "kiwi_propose",
        json!({"changes": [{"path": "adr/adr-009-retry-budgets.md", "content": doc}], "rationale": "Agents keep asking about retries."}),
    )
    .await;
    assert!(!error, "{text}");
    let proposal = json(&text);
    let branch = proposal["branch"].as_str().unwrap();
    assert!(branch.starts_with("kiwi/propose/alice/"));
    assert_eq!(proposal["merge"], "review");
    assert!(
        proposal["lint"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["rule"] == "broken-link")
    );
    let shown = std::process::Command::new("git")
        .args(["show", "--stat", "--format=%B", branch])
        .current_dir(&corpus.root)
        .output()
        .unwrap();
    let shown = String::from_utf8(shown.stdout).unwrap();
    assert!(shown.contains("adr/adr-009-retry-budgets.md"), "{shown}");
    assert!(shown.contains("Proposed-by: alice via claude-code"), "{shown}");
    // The work tree and main are untouched.
    assert!(!corpus.root.join("adr/adr-009-retry-budgets.md").exists());
    let (text, error) = call(
        &client,
        "kiwi_propose",
        json!({"changes": [{"path": "../escape.md", "content": "x"}], "rationale": "r"}),
    )
    .await;
    assert!(error, "{text}");
    client.cancel().await.unwrap();

    // A contractor may not propose; a finance-only file needs finance.
    let carl = connect(&corpus, "carl", "claude-code").await;
    let (text, error) = call(
        &carl,
        "kiwi_propose",
        json!({"changes": [{"path": "vision.md", "content": "# V\n"}], "rationale": "r"}),
    )
    .await;
    assert!(error, "{text}");
    carl.cancel().await.unwrap();
    let alice = connect(&corpus, "alice", "claude-code").await;
    let (text, error) = call(
        &alice,
        "kiwi_propose",
        json!({"changes": [{"path": "finance/budget-2027.md", "content": "# B\n"}], "rationale": "r"}),
    )
    .await;
    assert!(error, "{text}");
    alice.cancel().await.unwrap();
    let paul = connect(&corpus, "paul", "claude-code").await;
    let (text, error) = call(
        &paul,
        "kiwi_propose",
        json!({"changes": [{"path": "adr/adr-001-postgres.md", "content": std::fs::read_to_string(corpus.root.join("adr/adr-001-postgres.md")).unwrap() + "\nOne more line.\n"}], "rationale": "clarify"}),
    )
    .await;
    assert!(!error, "{text}");
    assert_eq!(json(&text)["merge"], "direct");
    let (text, error) = call(
        &paul,
        "kiwi_propose",
        json!({"changes": [{"section": "ADR-001", "content": "Postgres is the system of record."}], "rationale": "clarify"}),
    )
    .await;
    assert!(!error, "{text}");
    assert_eq!(json(&text)["merge"], "direct");
    paul.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn propose_edits_sections_and_frontmatter_in_place() {
    let corpus = Corpus::git();
    corpus.build(&[]);
    let alice = connect(&corpus, "alice", "claude-code").await;
    let (text, error) = call(
        &alice,
        "kiwi_propose",
        json!({
            "changes": [
                {"section": "adr/adr-005-idempotency-keys#decision", "content": "Every write API requires an idempotency key, kept for 48 hours."},
                {"path": "adr/adr-008-push-provider.md", "set": {"status": "approved", "review_by": "2027-09-01"}}
            ],
            "rationale": "Keys are now kept for 48 hours; the push provider decision was agreed."
        }),
    )
    .await;
    assert!(!error, "{text}");
    let branch = json(&text)["branch"].as_str().unwrap().to_owned();
    let show = |path: &str| {
        let out = std::process::Command::new("git")
            .args(["show", &format!("{branch}:{path}")])
            .current_dir(&corpus.root)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    };
    let adr5 = show("adr/adr-005-idempotency-keys.md");
    assert!(
        adr5.contains(
            "{#decision}\n\nEvery write API requires an idempotency key, kept for 48 hours.\n\n## "
        ),
        "{adr5}"
    );
    let adr8 = show("adr/adr-008-push-provider.md");
    assert!(
        adr8.contains("status: \"approved\"") && adr8.contains("review_by: \"2027-09-01\""),
        "{adr8}"
    );
    // A whole-file rewrite keeps the finance section alice cannot see.
    let (text, error) = call(
        &alice,
        "kiwi_propose",
        json!({"changes": [{"path": "product/checkout/prd.md", "content": "# Checkout\n\nRewritten.\n"}], "rationale": "r"}),
    )
    .await;
    assert!(!error, "{text}");
    let rewrite = json(&text);
    assert_eq!(rewrite["merge"], "review");
    let out = std::process::Command::new("git")
        .args([
            "show",
            &format!("{}:product/checkout/prd.md", rewrite["branch"].as_str().unwrap()),
        ])
        .current_dir(&corpus.root)
        .output()
        .unwrap();
    let prd = String::from_utf8(out.stdout).unwrap();
    assert!(
        prd.starts_with("# Checkout\n\nRewritten.\n\n## Pricing {#pricing access=finance}"),
        "{prd}"
    );
    // Hidden sections are unknown.
    let (text, error) = call(
        &alice,
        "kiwi_propose",
        json!({"changes": [{"section": "product/checkout/prd#pricing", "content": "x"}], "rationale": "r"}),
    )
    .await;
    assert!(error && text.contains("no section"), "{text}");
    alice.cancel().await.unwrap();
}

/// Polls `check` on fresh answers of a running server until it holds.
async fn eventually(
    client: &RunningService<RoleClient, ()>,
    tool: &str,
    args: Value,
    check: impl Fn(&str) -> bool,
) -> String {
    let mut last = String::new();
    for _ in 0..100 {
        let (text, _) = call(client, tool, args.clone()).await;
        if check(&text) {
            return text;
        }
        last = text;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    panic!("never held: {last}");
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_reloads_the_policy_with_the_pack() {
    let corpus = Corpus::copy();
    corpus.build(&["--worktree"]);
    let mut command = tokio::process::Command::new(KIWI);
    command
        .args([
            "serve",
            ".",
            "--watch",
            "--worktree",
            "--embedder",
            "hash",
            "--principal",
            "alice",
            "--agent",
            "claude-code",
        ])
        .current_dir(&corpus.root);
    let client = ().serve(TokioChildProcess::new(command).unwrap()).await.unwrap();
    let read = json!({"ids": ["adr/adr-005-idempotency-keys#decision", "vision"]});
    let (text, _) = call(&client, "kiwi_read", read.clone()).await;
    assert!(text.contains("adr/adr-005-idempotency-keys#decision"), "{text}");
    // A policy change alone (no Markdown changes) takes effect: internal content is no
    // longer readable by staff.
    let policy = corpus.root.join(".kiwi/policy.cedar");
    let text = std::fs::read_to_string(&policy).unwrap();
    std::fs::write(
        &policy,
        text.replace(
            "permit (principal in Kiwi::Group::\"staff\", action == Kiwi::Action::\"read\", resource)",
            "permit (principal in Kiwi::Group::\"nobody\", action == Kiwi::Action::\"read\", resource)",
        ),
    )
    .unwrap();
    let text = eventually(&client, "kiwi_read", read.clone(), |t| {
        !t.contains("adr-005-idempotency-keys#decision\",\"level")
    })
    .await;
    assert!(text.contains("\"id\":\"vision\""), "public content stays: {text}");
    // The configuration is reloaded with them: stub mode reports what is withheld.
    let config = corpus.root.join(".kiwi/kiwi.toml");
    let toml = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        toml.replace("hidden = \"invisible\"", "hidden = \"stub\""),
    )
    .unwrap();
    eventually(&client, "kiwi_read", read, |t| t.contains("\"redacted\"")).await;
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn watching_a_commit_follows_commits_not_the_working_files() {
    let corpus = Corpus::git();
    corpus.build(&[]);
    let mut command = tokio::process::Command::new(KIWI);
    command
        .args([
            "serve",
            ".",
            "--watch",
            "--embedder",
            "hash",
            "--principal",
            "alice",
            "--agent",
            "claude-code",
        ])
        .current_dir(&corpus.root);
    let client = ().serve(TokioChildProcess::new(command).unwrap()).await.unwrap();
    let read = json!({"ids": ["vision"]});
    let (text, _) = call(&client, "kiwi_read", read.clone()).await;
    let first = json(&text)["snapshot"].as_str().unwrap().to_owned();
    assert!(!first.starts_with("dir:"), "{text}");
    // An edit of the working files is not served; a commit of it is, as a commit.
    let vision = corpus.root.join("vision.md");
    let mut edited = std::fs::read_to_string(&vision).unwrap();
    edited.push_str("\nThe Fernwhistle line was added.\n");
    std::fs::write(&vision, edited).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    let (text, _) = call(&client, "kiwi_read", read.clone()).await;
    assert_eq!(json(&text)["snapshot"], first.as_str());
    for args in [
        vec!["add", "vision.md"],
        vec![
            "-c",
            "user.email=t@test",
            "-c",
            "user.name=T",
            "commit",
            "-q",
            "-m",
            "edit",
        ],
    ] {
        let status = std::process::Command::new("git")
            .args(&args)
            .current_dir(&corpus.root)
            .status()
            .unwrap();
        assert!(status.success());
    }
    let served = first.clone();
    let text = eventually(&client, "kiwi_read", read, move |t| !t.contains(served.as_str())).await;
    let snapshot = json(&text)["snapshot"].as_str().unwrap().to_owned();
    assert!(snapshot != first && !snapshot.starts_with("dir:"), "{snapshot}");
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn internal_errors_reach_the_client_without_details() {
    let corpus = Corpus::copy();
    corpus.build(&[]);
    let client = connect(&corpus, "alice", "claude-code").await;
    let (text, error) = call(&client, "kiwi_search", json!({"queries": ["saga"]})).await;
    assert!(!error, "{text}");
    // The audit log is forged behind the server's back: the next answer cannot be
    // audited, so it fails, and the client learns nothing about the server.
    let log = corpus.root.join(".kiwi/state/audit.log");
    let mut text = std::fs::read_to_string(&log).unwrap();
    text.push_str("forged\n");
    std::fs::write(&log, text).unwrap();
    let params = CallToolRequestParams::new("kiwi_search")
        .with_arguments(json!({"queries": ["retry budget"]}).as_object().unwrap().clone());
    let failure = client.call_tool(params).await.unwrap_err().to_string();
    assert!(failure.contains("internal error (reference "), "{failure}");
    let root = corpus.root.to_string_lossy();
    assert!(
        !failure.contains(root.as_ref()) && !failure.contains("audit.log"),
        "{failure}"
    );
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_proposal_shows_only_a_reference() {
    // The forge push fails (no such remote): git's output names the repository and the
    // remote, and must reach only the operator's log and the audit log.
    let corpus = Corpus::git();
    let config = std::fs::read_to_string(corpus.root.join(".kiwi/kiwi.toml")).unwrap();
    std::fs::write(
        corpus.root.join(".kiwi/kiwi.toml"),
        config + "\n[forge]\nkind = \"github\"\nremote = \"no-such-remote\"\n",
    )
    .unwrap();
    let committed = std::process::Command::new("git")
        .args([
            "-c",
            "user.email=t@test",
            "-c",
            "user.name=T",
            "commit",
            "-qam",
            "forge",
        ])
        .current_dir(&corpus.root)
        .status()
        .unwrap();
    assert!(committed.success());
    corpus.build(&[]);
    let alice = connect(&corpus, "alice", "claude-code").await;
    let (text, error) = call(
        &alice,
        "kiwi_propose",
        json!({"changes": [{"section": "adr/adr-005-idempotency-keys#decision", "content": "Keys are kept for 48 hours."}], "rationale": "r"}),
    )
    .await;
    alice.cancel().await.unwrap();
    assert!(error, "{text}");
    let reference = text
        .strip_prefix("the proposal could not be prepared (reference ")
        .and_then(|rest| rest.strip_suffix(')'))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(
        !text.contains("no-such-remote") && !text.contains("git"),
        "{text}"
    );
    let audit = std::fs::read_to_string(corpus.root.join(".kiwi/state/audit.log")).unwrap();
    let note = audit
        .lines()
        .find(|line| line.contains(reference))
        .unwrap_or_else(|| panic!("{audit}"));
    assert!(note.contains("no-such-remote"), "{note}");
    let record: Value = json(note);
    assert_eq!(record["status"], "failed");
    assert_eq!(record["reference"], reference);
    assert!(record.get("ids").is_none() && record.get("docs").is_none());
}
