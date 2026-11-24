//! Kiwi MCP server: six tools (`kiwi_map`, `kiwi_search`, `kiwi_read`,
//! `kiwi_trace`, `kiwi_propose`, `kiwi_feedback`) and the `kiwi://atom/{id}@{rev}`
//! resource, over stdio and Streamable HTTP with OAuth 2.1.
//!
//! Every call resolves the caller to an identity, asks each corpus's policy gate for
//! its view, answers from the immutable packs, caches the bytes per (snapshots,
//! classes, request), and appends one hash-chained audit record per corpus. One server
//! can federate several corpora by namespace.

#![cfg_attr(test, allow(clippy::unwrap_used))]

pub mod corpus;
pub mod feedback;
#[cfg(feature = "http")]
pub mod http;
pub mod server;
pub mod service;

pub use corpus::{Corpus, CorpusPlugins, ServiceOptions};
pub use server::{IdentitySource, KiwiServer, tools};
pub use service::{KiwiService, ToolError};

/// Serves one identity over standard input and output until the client disconnects.
pub async fn serve_stdio(
    service: std::sync::Arc<KiwiService>,
    identity: kiwi_policy::Identity,
) -> Result<(), String> {
    use rmcp::ServiceExt;
    let server = KiwiServer::new(service, IdentitySource::Fixed(identity));
    let running = server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| e.to_string())?;
    running.waiting().await.map_err(|e| e.to_string())?;
    Ok(())
}
