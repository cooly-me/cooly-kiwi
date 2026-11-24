//! Streamable HTTP with OAuth 2.1 bearer tokens: 401 with a resource-metadata challenge,
//! the protected-resource metadata, token validation, and on-behalf-of narrowing by
//! token scopes and by the HTTP channel.

#![cfg(feature = "http")]
#![allow(clippy::unwrap_used, clippy::zombie_processes)]

mod common;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{Corpus, KIWI};
use jsonwebtoken::{EncodingKey, Header, encode};
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::json;

const SECRET: &str = "test-secret-with-enough-entropy-for-hs256";
const ISSUER: &str = "https://issuer.test";

struct Server {
    child: Child,
    base: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start(corpus: &Corpus) -> Server {
    start_with(corpus, &[])
}

fn start_with(corpus: &Corpus, extra: &[&str]) -> Server {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let base = format!("http://127.0.0.1:{port}");
    let child = Command::new(KIWI)
        .args([
            "serve",
            ".",
            "--http",
            &format!("127.0.0.1:{port}"),
            "--auth-hs256-env",
            "KIWI_TEST_SECRET",
            "--issuer",
            ISSUER,
            "--resource",
            &format!("{base}/mcp"),
        ])
        .args(extra)
        .env("KIWI_TEST_SECRET", SECRET)
        .current_dir(&corpus.root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..200 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Server { child, base };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("server did not start");
}

fn token(sub: &str, scope: &str, audience: &str) -> String {
    let exp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 600;
    sign(
        json!({"sub": sub, "iss": ISSUER, "aud": audience, "exp": exp, "scope": scope, "client_id": "claude-code"}),
    )
}

fn sign(claims: serde_json::Value) -> String {
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(SECRET.as_bytes()).unwrap(),
    )
    .unwrap()
}

async fn search(base: &str, bearer: &str, query: &str) -> String {
    let config =
        StreamableHttpClientTransportConfig::with_uri(format!("{base}/mcp")).auth_header(bearer.to_owned());
    let transport = StreamableHttpClientTransport::from_config(config);
    let client = ().serve(transport).await.unwrap();
    let params = CallToolRequestParams::new("kiwi_search")
        .with_arguments(json!({"queries": [query]}).as_object().unwrap().clone());
    let result = client.call_tool(params).await.unwrap();
    let text = result.content[0].as_text().unwrap().text.clone();
    client.cancel().await.unwrap();
    text
}

#[tokio::test(flavor = "multi_thread")]
async fn bearer_tokens_gate_the_mcp_endpoint() {
    let corpus = Corpus::copy();
    corpus.build(&[]);
    let server = start(&corpus);
    let http = reqwest::Client::new();
    let resource = format!("{}/mcp", server.base);

    let response = http
        .post(&resource)
        .header("content-type", "application/json")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    let challenge = response.headers()["www-authenticate"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(challenge.contains("resource_metadata="), "{challenge}");
    // The challenge names a metadata URL that the server answers.
    let named = challenge
        .split("resource_metadata=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_owned();
    let response = http.get(&named).send().await.unwrap();
    assert_eq!(response.status(), 200, "{named}");
    let named: serde_json::Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(named["resource"], resource);

    let metadata: serde_json::Value = serde_json::from_slice(
        &http
            .get(format!("{}/.well-known/oauth-protected-resource", server.base))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(metadata["resource"], resource);
    assert_eq!(metadata["authorization_servers"][0], ISSUER);

    let wrong_audience = token("fiona", "kiwi:read", "https://elsewhere.test/mcp");
    let response = http
        .post(&resource)
        .bearer_auth(wrong_audience)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);

    // Fiona may read the finance-only pricing section...
    let full = search(&server.base, &token("fiona", "kiwi:read", &resource), "pricing").await;
    assert!(full.contains("product/checkout/prd#pricing"), "{full}");
    // ...but not through a token limited to public and internal labels.
    let narrowed = search(
        &server.base,
        &token(
            "fiona",
            "kiwi:read kiwi:label:public kiwi:label:internal",
            &resource,
        ),
        "pricing",
    )
    .await;
    assert!(!narrowed.contains("#pricing"), "{narrowed}");
    // Restricted content is readable by exec through an authenticated session (and
    // never over stdio, where the user is only asserted).
    let erin = search(
        &server.base,
        &token("erin", "kiwi:read", &resource),
        "acquisition targets",
    )
    .await;
    assert!(erin.contains("acquisition-targets"), "{erin}");
    let local = corpus.query("kiwi_search", r#"{"queries": ["acquisition targets"]}"#, "erin");
    assert!(!local.contains("acquisition-targets"), "{local}");

    // A token without a kiwi: scope is refused with 403 insufficient_scope.
    let response = http
        .post(&resource)
        .bearer_auth(token("fiona", "openid profile", &resource))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    let challenge = response.headers()["www-authenticate"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        challenge.contains("insufficient_scope") && challenge.contains("kiwi:read"),
        "{challenge}"
    );
    // A token not yet valid (nbf in the future) is refused.
    let exp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 600;
    let early = sign(
        json!({"sub": "fiona", "iss": ISSUER, "aud": resource, "exp": exp, "nbf": exp, "scope": "kiwi:read", "client_id": "claude-code"}),
    );
    let response = http
        .post(&resource)
        .bearer_auth(early)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    // The scheme is case-insensitive.
    let initialize = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
    let response = http
        .post(&resource)
        .header(
            "authorization",
            format!("bearer {}", token("fiona", "kiwi:read", &resource)),
        )
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(initialize)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    // A token that names no client, or an agent principals.yaml does not list, reads
    // nothing: unlisted agents get the default grant, which is empty.
    let anonymous =
        sign(json!({"sub": "fiona", "iss": ISSUER, "aud": resource, "exp": exp, "scope": "kiwi:read"}));
    let unlisted = sign(
        json!({"sub": "fiona", "iss": ISSUER, "aud": resource, "exp": exp, "scope": "kiwi:read", "client_id": "rogue"}),
    );
    for bearer in [anonymous, unlisted] {
        let text = search(&server.base, &bearer, "pricing").await;
        assert!(text.contains("\"results\":[]"), "{text}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_are_bounded() {
    let corpus = Corpus::copy();
    corpus.build(&[]);
    let server = start_with(&corpus, &["--max-sessions", "1"]);
    let resource = format!("{}/mcp", server.base);
    let config = StreamableHttpClientTransportConfig::with_uri(resource.clone()).auth_header(token(
        "fiona",
        "kiwi:read",
        &resource,
    ));
    let first = ().serve(StreamableHttpClientTransport::from_config(config)).await.unwrap();
    // A second session is refused while the first is open.
    let initialize = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
    let response = reqwest::Client::new()
        .post(&resource)
        .bearer_auth(token("fiona", "kiwi:read", &resource))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(initialize)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    // Closing it frees the slot.
    first.cancel().await.unwrap();
    let mut freed = false;
    for _ in 0..50 {
        let config = StreamableHttpClientTransportConfig::with_uri(resource.clone()).auth_header(token(
            "fiona",
            "kiwi:read",
            &resource,
        ));
        if let Ok(client) = ().serve(StreamableHttpClientTransport::from_config(config)).await {
            client.cancel().await.unwrap();
            freed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(freed, "the slot was not freed");
}

#[test]
fn no_corpus_configuration_reads_the_token_secret() {
    // An audit sink that names the HS256 secret would ship it to a collector of the
    // configuration's choosing. It is refused even when the operator lists it.
    let corpus = Corpus::copy();
    let config = std::fs::read_to_string(corpus.root.join(".kiwi/kiwi.toml")).unwrap();
    std::fs::write(
        corpus.root.join(".kiwi/kiwi.toml"),
        config
            + "\n[[audit.sinks]]\nkind = \"http\"\nurl = \"http://127.0.0.1:9/collect\"\nbearer_env = \"KIWI_TEST_SECRET\"\n",
    )
    .unwrap();
    corpus.build(&[]);
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let out = Command::new(KIWI)
        .args([
            "serve",
            ".",
            "--http",
            &format!("127.0.0.1:{port}"),
            "--auth-hs256-env",
            "KIWI_TEST_SECRET",
            "--issuer",
            ISSUER,
        ])
        .env("KIWI_TEST_SECRET", SECRET)
        .env("KIWI_SECRET_ENV", "KIWI_TEST_SECRET")
        .current_dir(&corpus.root)
        .output()
        .unwrap();
    assert!(!out.status.success(), "the server started");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("secret of the server"), "{stderr}");
    assert!(!stderr.contains(SECRET));
}
