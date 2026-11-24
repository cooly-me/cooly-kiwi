//! The MCP handler: six tools and one resource template, served over stdio or
//! Streamable HTTP by `rmcp`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use kiwi_core::api::{MapRequest, ReadRequest, SearchRequest, TraceRequest};
use kiwi_policy::Identity;
use rmcp::handler::server::common::schema_for_type;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListResourceTemplatesResult, ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams,
    ReadResourceResponse, ReadResourceResult, ResourceContents, ResourceTemplate, ServerCapabilities,
    ServerConfig, Tool, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};

use crate::service::{FeedbackRequest, KiwiService, ProposeRequest, ToolError};

const INSTRUCTIONS: &str = "Kiwi serves a governed knowledge base compiled from Markdown in Git. \
Orient with kiwi_map, find with kiwi_search (pass several short queries: keywords, synonyms, ids), \
open only what you need with kiwi_read, follow why-chains with kiwi_trace. Every answer is pinned to \
a snapshot; cite kiwi://atom/{id}@{snapshot}. When a server federates several corpora, ids are \
namespace:id and each corpus's snapshot is in `snapshots`. Report gaps with kiwi_feedback; change \
knowledge only through kiwi_propose (it opens a branch and a pull request).";

/// Logs an internal failure for the operator (standard error, and `tracing`) and
/// returns the reference a client is given instead of the details: server paths, a
/// tool's output or a corpus the caller cannot see never reach an MCP client.
pub fn log_internal(message: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut hasher = blake3::Hasher::new();
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(&NEXT.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    hasher.update(kiwi_core::date::timestamp(kiwi_core::date::now_unix()).as_bytes());
    let reference = hasher.finalize().to_hex()[..12].to_owned();
    tracing::error!(reference = %reference, %message, "internal error");
    eprintln!("kiwi: internal error {reference}: {message}");
    reference
}

/// The error an MCP client receives for an internal failure.
fn internal(message: &str) -> ErrorData {
    failed(&log_internal(message))
}

/// The error an MCP client receives for an internal failure already logged under
/// `reference`.
fn failed(reference: &str) -> ErrorData {
    ErrorData::internal_error(format!("internal error (reference {reference})"), None)
}

/// How a handler learns who is calling.
#[derive(Debug, Clone)]
pub enum IdentitySource {
    /// One identity for the whole connection (stdio).
    Fixed(Identity),
    /// From the authenticated HTTP request (set by the auth layer).
    Request,
}

/// The MCP server handler.
#[derive(Debug, Clone)]
pub struct KiwiServer {
    service: Arc<KiwiService>,
    identity: IdentitySource,
}

fn tool<T: schemars::JsonSchema + std::any::Any>(
    name: &'static str,
    description: &'static str,
    read_only: bool,
) -> Tool {
    let annotations = if read_only {
        ToolAnnotations::new().read_only(true)
    } else {
        ToolAnnotations::new().read_only(false).destructive(false)
    };
    Tool::new(name, description, schema_for_type::<T>()).with_annotations(annotations)
}

/// The six tools, in a fixed order.
pub fn tools() -> Vec<Tool> {
    vec![
        tool::<MapRequest>(
            "kiwi_map",
            "Table of contents (L0 titles, L1 abstracts) of a scope: an atom id or a path prefix such as `adr/`. Use it to orient before reading.",
            true,
        ),
        tool::<SearchRequest>(
            "kiwi_search",
            "Ranked atoms (sections) for one or more queries you write: keywords, synonyms, ids or a one-line hypothetical answer. High-ranked results carry an L1 abstract, the rest an L0 title, within a token budget (default 800). Filters: type (vision, prd, rfc, adr, runbook, standard, glossary), status, owner, tags, path, namespace.",
            true,
        ),
        tool::<ReadRequest>(
            "kiwi_read",
            "Full text (L2, default) or L0/L1 of atoms by id (`path#anchor`, a document path, or a document id such as ADR-005), with typed links and optional parent/siblings/children.",
            true,
        ),
        tool::<TraceRequest>(
            "kiwi_trace",
            "Walk the typed graph from an atom to answer why: implements, supersedes, decided_by, depends_on, amends, amended_by and code by default (PRD -> RFC -> ADR -> code); relation `mentions` or `any` adds text mentions.",
            true,
        ),
        tool::<ProposeRequest>(
            "kiwi_propose",
            "Propose a knowledge change through Git (a branch and a pull request; nothing is written to the main branch). Each change is one of: {section: atom id, content: new body} to rewrite one section in place; {path, set: {frontmatter keys}} such as status or review_by; {path, content} for a new or whole file; {path} to delete. Returns the link, whether review is needed, and lint findings.",
            false,
        ),
        tool::<FeedbackRequest>(
            "kiwi_feedback",
            "Record whether atoms answered a query: useful, wrong, or missing (knowledge that should exist). Missing feeds the writers' knowledge-gap queue.",
            false,
        ),
    ]
}

impl KiwiServer {
    /// A handler over `service`.
    pub fn new(service: Arc<KiwiService>, identity: IdentitySource) -> Self {
        Self { service, identity }
    }

    fn identity(&self, context: &RequestContext<RoleServer>) -> Result<Identity, ErrorData> {
        match &self.identity {
            IdentitySource::Fixed(identity) => Ok(identity.clone()),
            IdentitySource::Request => context
                .extensions
                .get::<http::request::Parts>()
                .and_then(|parts| parts.extensions.get::<Identity>())
                .cloned()
                .ok_or_else(|| ErrorData::invalid_request("unauthenticated request", None)),
        }
    }
}

impl ServerHandler for KiwiServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new("kiwi", env!("CARGO_PKG_VERSION")))
        .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tools().into_iter().find(|tool| tool.name == name)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let identity = self.identity(&context)?;
        let args = request
            .arguments
            .map(serde_json::Value::Object)
            .unwrap_or(serde_json::Value::Null);
        let service = Arc::clone(&self.service);
        let name = request.name.to_string();
        let outcome = tokio::task::spawn_blocking(move || service.call(&identity, &name, args))
            .await
            .map_err(|e| internal(&e.to_string()))?;
        Ok(match outcome {
            Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]).into(),
            Err(ToolError::Invalid(message)) => {
                CallToolResult::error(vec![ContentBlock::text(message)]).into()
            }
            Err(ToolError::Internal(message)) => return Err(internal(&message)),
            Err(ToolError::Failed(reference)) => return Err(failed(&reference)),
        })
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        let template = ResourceTemplate::new("kiwi://atom/{id}@{rev}", "atom")
            .with_title("Atom at a snapshot")
            .with_description("The full text of one atom at a pinned snapshot, for stable citations.");
        Ok(ListResourceTemplatesResult::with_all_items(vec![template]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let identity = self.identity(&context)?;
        let service = Arc::clone(&self.service);
        let uri = request.uri.clone();
        let outcome = tokio::task::spawn_blocking(move || service.read_resource(&identity, &uri))
            .await
            .map_err(|e| internal(&e.to_string()))?;
        match outcome {
            Ok(text) => Ok(ReadResourceResult::new(vec![ResourceContents::text(text, request.uri)]).into()),
            Err(ToolError::Invalid(message)) => Err(ErrorData::resource_not_found(message, None)),
            Err(ToolError::Internal(message)) => Err(internal(&message)),
            Err(ToolError::Failed(reference)) => Err(failed(&reference)),
        }
    }
}
