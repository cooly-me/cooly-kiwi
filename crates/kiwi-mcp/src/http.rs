//! Streamable HTTP with OAuth 2.1 bearer tokens, as the MCP authorization spec asks of
//! a resource server: tokens are validated (signature, expiry, not-before, issuer,
//! audience, a `kiwi:` scope), the protected-resource metadata is published, and a
//! missing or bad token gets `401` (a token without a `kiwi:` scope `403`) with a
//! `WWW-Authenticate` header pointing at it.
//!
//! Claims map to an [`Identity`]: `sub` is the user, `client_id` (or `azp`) the agent,
//! and `kiwi:label:<label>` scopes, when present, limit the session to those labels. A
//! token must carry `kiwi:read` or at least one `kiwi:label:` scope; a token that names
//! no client acts through [`UNIDENTIFIED_AGENT`], which only the directory's
//! `default_agent` grant covers.
//!
//! Signing keys: an HS256 secret, or public keys from a file: a PEM bundle (any number
//! of RSA or EC public keys) or a JWKS document (keys chosen by `kid`). The file is read
//! again when it changes, so a new key is added before the authorization server signs
//! with it and the old one removed after its tokens expire, without a restart.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use jsonwebtoken::jwk::{AlgorithmParameters, EllipticCurve, PublicKeyUse};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use kiwi_policy::{Identity, UNIDENTIFIED_AGENT};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde::Deserialize;

use crate::server::{IdentitySource, KiwiServer};
use crate::service::KiwiService;

/// The scope that grants reading without a label limit.
pub const READ_SCOPE: &str = "kiwi:read";

/// The channel of a caller over unauthenticated HTTP (`--insecure-principal`): never
/// `http`, so no policy written for authenticated HTTP callers applies to it.
pub const INSECURE_CHANNEL: &str = "insecure";

/// The path of the protected-resource metadata (RFC 9728); a resource with a path is
/// described at this path followed by its own.
const METADATA_PATH: &str = "/.well-known/oauth-protected-resource";

/// How often a key file is checked for changes, at most.
const KEY_FILE_CHECK: Duration = Duration::from_secs(5);

/// How HTTP callers authenticate.
#[derive(Clone)]
pub enum HttpAuth {
    /// OAuth 2.1 access tokens (JWT).
    Jwt(Arc<JwtAuth>),
    /// No authentication: every request acts as one identity, on the
    /// [`INSECURE_CHANNEL`]. For local development only; the server binds to loopback
    /// in this mode. Build it with [`HttpAuth::insecure`].
    Insecure(Identity),
}

impl HttpAuth {
    /// Unauthenticated HTTP acting as `principal` through `agent`, on the
    /// [`INSECURE_CHANNEL`].
    pub fn insecure(principal: impl Into<String>, agent: impl Into<String>) -> Self {
        HttpAuth::Insecure(Identity::new(principal, agent, INSECURE_CHANNEL))
    }
}

/// The format of a key file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyFormat {
    /// One or more PEM public keys (RSA for RS256, EC P-256 / P-384 for ES256 / ES384).
    Pem,
    /// A JSON Web Key Set.
    Jwks,
}

/// One key tokens may be signed with, and the algorithms it verifies.
struct VerifyKey {
    kid: Option<String>,
    key: DecodingKey,
    algorithms: Vec<Algorithm>,
}

/// A key file and the BLAKE3 hash of the content its keys were last taken from.
struct KeyFile {
    path: PathBuf,
    format: KeyFormat,
    hash: [u8; 32],
    checked: Instant,
    every: Duration,
}

/// Why a request is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// No token, or one that does not validate: `401`.
    InvalidToken(String),
    /// A valid token without a `kiwi:` scope: `403`.
    InsufficientScope,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::InvalidToken(message) => f.write_str(message),
            AuthError::InsufficientScope => {
                write!(f, "the token carries no kiwi: scope (ask for {READ_SCOPE})")
            }
        }
    }
}

/// JWT validation settings.
pub struct JwtAuth {
    keys: RwLock<Vec<VerifyKey>>,
    file: Option<Mutex<KeyFile>>,
    issuer: String,
    audience: String,
    /// The canonical URL of this resource server (the token audience).
    pub resource: String,
    /// Authorization servers to advertise in the metadata.
    pub authorization_servers: Vec<String>,
}

impl std::fmt::Debug for JwtAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwtAuth")
            .field("resource", &self.resource)
            .finish()
    }
}

/// The PEM blocks of a bundle.
fn pem_blocks(text: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(begin) = rest.find("-----BEGIN ") {
        let after = &rest[begin..];
        let Some(end) = after.find("-----END ") else {
            break;
        };
        let close = after[end + 9..]
            .find("-----")
            .map_or(after.len(), |i| end + 9 + i + 5);
        blocks.push(&after[..close]);
        rest = &after[close..];
    }
    blocks
}

/// Reports a problem with the token keys to the operator: on standard error, where it
/// is seen without a log configuration, and through `tracing`.
fn key_error(message: &str) {
    tracing::error!(%message, "token keys");
    eprintln!("kiwi: error: {message}");
}

/// The keys of a key file, each accepted or refused on its own.
struct KeySet {
    /// The keys that parse.
    keys: Vec<VerifyKey>,
    /// Why each other key was refused.
    refused: Vec<String>,
}

impl KeySet {
    /// The keys to use, or why the file is broken: a file whose every key is refused
    /// is broken, while one that lists no key at all is a valid empty set.
    fn usable(self) -> Result<(Vec<VerifyKey>, Vec<String>), String> {
        if self.keys.is_empty() && !self.refused.is_empty() {
            return Err(self.refused.join("; "));
        }
        Ok((self.keys, self.refused))
    }
}

fn pem_keys(bytes: &[u8]) -> Result<KeySet, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "the key file is not UTF-8".to_owned())?;
    let blocks = pem_blocks(text);
    if blocks.is_empty() && !text.trim().is_empty() {
        return Err("the key file holds text but no PEM block".into());
    }
    let mut set = KeySet {
        keys: Vec::new(),
        refused: Vec::new(),
    };
    for (index, block) in blocks.into_iter().enumerate() {
        let (key, algorithms) = if let Ok(key) = DecodingKey::from_rsa_pem(block.as_bytes()) {
            (key, vec![Algorithm::RS256])
        } else if let Ok(key) = DecodingKey::from_ec_pem(block.as_bytes()) {
            // P-256 or P-384: the curve decides which of the two verifies.
            (key, vec![Algorithm::ES256, Algorithm::ES384])
        } else {
            set.refused
                .push(format!("PEM block {} is not an RSA or EC public key", index + 1));
            continue;
        };
        set.keys.push(VerifyKey {
            kid: None,
            key,
            algorithms,
        });
    }
    Ok(set)
}

fn jwk_key(jwk: &jsonwebtoken::jwk::Jwk) -> Result<VerifyKey, String> {
    let by_type = match &jwk.algorithm {
        AlgorithmParameters::RSA(_) => Algorithm::RS256,
        AlgorithmParameters::EllipticCurve(params) => match params.curve {
            EllipticCurve::P384 => Algorithm::ES384,
            _ => Algorithm::ES256,
        },
        AlgorithmParameters::OctetKeyPair(_) => Algorithm::EdDSA,
        _ => return Err("only public RSA, EC or OKP keys are accepted".into()),
    };
    let algorithm = match jwk.common.key_algorithm {
        Some(alg) => Algorithm::try_from(alg).map_err(|e| e.to_string())?,
        None => by_type,
    };
    let key = DecodingKey::from_jwk(jwk).map_err(|e| e.to_string())?;
    Ok(VerifyKey {
        kid: jwk.common.key_id.clone(),
        key,
        algorithms: vec![algorithm],
    })
}

fn jwks_keys(bytes: &[u8]) -> Result<KeySet, String> {
    // The document must be a key set; each key in it is then judged on its own, so one
    // key of an algorithm Kiwi does not take never drops the others.
    #[derive(Deserialize)]
    struct RawSet {
        keys: Vec<serde_json::Value>,
    }
    let raw: RawSet = serde_json::from_slice(bytes).map_err(|e| format!("not a JWKS document: {e}"))?;
    let mut set = KeySet {
        keys: Vec::new(),
        refused: Vec::new(),
    };
    for (index, value) in raw.keys.into_iter().enumerate() {
        let parsed = serde_json::from_value::<jsonwebtoken::jwk::Jwk>(value)
            .map_err(|e| e.to_string())
            .and_then(|jwk| {
                if matches!(jwk.common.public_key_use, Some(PublicKeyUse::Encryption)) {
                    Ok(None)
                } else {
                    jwk_key(&jwk).map(Some)
                }
            });
        match parsed {
            Ok(Some(key)) => set.keys.push(key),
            Ok(None) => {}
            Err(error) => set.refused.push(format!("JWKS key {}: {error}", index + 1)),
        }
    }
    Ok(set)
}

/// The keys of the key file content `bytes` (read from `path`). Keys that do not parse
/// are dropped one by one and reported in the second list; content whose every key is
/// refused, or that is not a key file at all, is an error.
fn parse_keys(path: &Path, bytes: &[u8], format: KeyFormat) -> Result<(Vec<VerifyKey>, Vec<String>), String> {
    let (keys, refused) = match format {
        KeyFormat::Pem => pem_keys(bytes),
        KeyFormat::Jwks => jwks_keys(bytes),
    }
    .and_then(KeySet::usable)
    .map_err(|e| format!("{}: {e}", path.display()))?;
    let refused = refused
        .into_iter()
        .map(|reason| format!("{}: {reason}", path.display()))
        .collect();
    Ok((keys, refused))
}

impl JwtAuth {
    /// Tokens signed with a shared HMAC secret (HS256).
    pub fn hs256(secret: &[u8], issuer: &str, resource: &str) -> Result<Self, String> {
        let key = DecodingKey::from_secret(secret).map_err(|e| e.to_string())?;
        Ok(Self::with_keys(
            vec![VerifyKey {
                kid: None,
                key,
                algorithms: vec![Algorithm::HS256],
            }],
            None,
            issuer,
            resource,
        ))
    }

    /// Tokens signed by any of the public keys of a PEM bundle (RSA keys for RS256, EC
    /// keys for ES256 / ES384).
    pub fn rs256_pem(pem: &[u8], issuer: &str, resource: &str) -> Result<Self, String> {
        let (keys, refused) = pem_keys(pem)?.usable()?;
        if let Some(reason) = refused.first() {
            return Err(reason.clone());
        }
        if keys.is_empty() {
            return Err("no PEM public key".into());
        }
        Ok(Self::with_keys(keys, None, issuer, resource))
    }

    /// Tokens signed by the keys of a file (a PEM bundle or a JWKS document), read
    /// again when the file changes. Each key is accepted on its own: one that does not
    /// parse is logged and skipped. At start the file must hold at least one usable
    /// key; on a reload, a valid file with no key revokes every key, and a broken one
    /// keeps the previous keys and logs an error.
    pub fn key_file(path: &Path, format: KeyFormat, issuer: &str, resource: &str) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let (keys, refused) = parse_keys(path, &bytes, format)?;
        if keys.is_empty() {
            return Err(format!("{}: no usable signing key", path.display()));
        }
        for reason in &refused {
            key_error(&format!("token key refused, the other keys are used: {reason}"));
        }
        let file = KeyFile {
            path: path.to_owned(),
            format,
            hash: *blake3::hash(&bytes).as_bytes(),
            checked: Instant::now(),
            every: KEY_FILE_CHECK,
        };
        Ok(Self::with_keys(keys, Some(file), issuer, resource))
    }

    fn with_keys(keys: Vec<VerifyKey>, file: Option<KeyFile>, issuer: &str, resource: &str) -> Self {
        Self {
            keys: RwLock::new(keys),
            file: file.map(Mutex::new),
            issuer: issuer.to_owned(),
            audience: resource.to_owned(),
            resource: resource.trim_end_matches('/').to_owned(),
            authorization_servers: vec![issuer.to_owned()],
        }
    }

    /// Where the metadata of this resource is served (RFC 9728): the well-known path
    /// at the resource's origin, followed by the resource's own path
    /// (`https://h/mcp` → `https://h/.well-known/oauth-protected-resource/mcp`).
    pub fn metadata_url(&self) -> String {
        metadata_url(&self.resource)
    }

    fn validation(&self, algorithm: Algorithm) -> Validation {
        let mut validation = Validation::new(algorithm);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.audience]);
        validation.set_required_spec_claims(&["exp", "sub", "iss", "aud"]);
        validation.validate_nbf = true;
        validation.leeway = 30;
        validation
    }

    /// Reads the key file again (at most every few seconds) and takes its keys when its
    /// content changed (by hash, whatever its modification time says). Its usable keys
    /// replace the current ones, none at all when it lists none; a file that is missing
    /// or broken (unreadable, not a key file, or every key refused) leaves the keys as
    /// they were and is reported as an error at every check until it is fixed.
    fn refresh(&self) {
        let Some(file) = &self.file else {
            return;
        };
        let Ok(mut file) = file.lock() else {
            return;
        };
        if file.checked.elapsed() < file.every {
            return;
        }
        file.checked = Instant::now();
        let bytes = match std::fs::read(&file.path) {
            Ok(bytes) => bytes,
            Err(error) => {
                key_error(&format!(
                    "the token key file {} cannot be read, the previous keys stay: {error}",
                    file.path.display()
                ));
                return;
            }
        };
        let hash = *blake3::hash(&bytes).as_bytes();
        if hash == file.hash {
            return;
        }
        match parse_keys(&file.path, &bytes, file.format) {
            Ok((keys, refused)) => {
                for reason in &refused {
                    key_error(&format!("token key refused, the other keys are used: {reason}"));
                }
                let count = keys.len();
                if let Ok(mut current) = self.keys.write() {
                    *current = keys;
                }
                file.hash = hash;
                if count == 0 {
                    key_error(&format!(
                        "{} lists no key: every token is refused",
                        file.path.display()
                    ));
                } else {
                    tracing::info!(path = %file.path.display(), keys = count, "token keys reloaded");
                }
            }
            // The hash is left as it was, so the broken file is read (and reported)
            // again at the next check.
            Err(error) => key_error(&format!(
                "the token key file is broken, the previous keys stay: {error}"
            )),
        }
    }

    /// Validates a token and maps its claims to an identity.
    pub fn identity(&self, token: &str) -> Result<Identity, AuthError> {
        self.refresh();
        let invalid = |e: &dyn std::fmt::Display| AuthError::InvalidToken(e.to_string());
        let header = decode_header(token).map_err(|e| invalid(&e))?;
        let keys = self
            .keys
            .read()
            .map_err(|_| AuthError::InvalidToken("key store unavailable".into()))?;
        // A token naming a key is checked against that key (and keys without an id); one
        // naming none against every key of its algorithm.
        let candidates = keys.iter().filter(|key| {
            key.algorithms.contains(&header.alg)
                && match (&header.kid, &key.kid) {
                    (Some(wanted), Some(kid)) => wanted == kid,
                    _ => true,
                }
        });
        let mut last = AuthError::InvalidToken("no key for this token's algorithm or kid".into());
        let mut claims = None;
        for key in candidates {
            match decode::<Claims>(token, &key.key, &self.validation(header.alg)) {
                Ok(data) => {
                    claims = Some(data.claims);
                    break;
                }
                Err(error) => last = invalid(&error),
            }
        }
        drop(keys);
        let claims = claims.ok_or(last)?;
        let scopes = claims.scopes();
        let labels: Vec<String> = scopes
            .iter()
            .filter_map(|s| s.strip_prefix("kiwi:label:"))
            .filter(|label| !label.is_empty())
            .map(str::to_owned)
            .collect();
        if !scopes.iter().any(|s| s == READ_SCOPE) && labels.is_empty() {
            return Err(AuthError::InsufficientScope);
        }
        Ok(Identity {
            principal: claims.sub,
            agent: claims
                .client_id
                .or(claims.azp)
                .filter(|agent| !agent.trim().is_empty())
                .unwrap_or_else(|| UNIDENTIFIED_AGENT.into()),
            channel: "http".into(),
            session_labels: (!labels.is_empty()).then_some(labels),
        })
    }
}

/// `scp` as some authorization servers write it: a list, or one string.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ScopeList {
    List(Vec<String>),
    One(String),
}

#[derive(Debug, Deserialize)]
struct Claims {
    sub: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    scp: Option<ScopeList>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    azp: Option<String>,
}

impl Claims {
    fn scopes(&self) -> Vec<String> {
        let mut scopes: Vec<String> = self
            .scope
            .as_deref()
            .unwrap_or("")
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        match &self.scp {
            Some(ScopeList::List(list)) => scopes.extend(list.iter().cloned()),
            Some(ScopeList::One(text)) => scopes.extend(text.split_whitespace().map(str::to_owned)),
            None => {}
        }
        scopes
    }
}

/// The RFC 9728 metadata URL of `resource`.
fn metadata_url(resource: &str) -> String {
    let (origin, path) = match resource.find("://") {
        Some(scheme) => {
            let rest = &resource[scheme + 3..];
            match rest.find('/') {
                Some(slash) => resource.split_at(scheme + 3 + slash),
                None => (resource, ""),
            }
        }
        None => (resource, ""),
    };
    format!("{origin}{METADATA_PATH}{}", path.trim_end_matches('/'))
}

fn challenge(auth: &JwtAuth, error: &AuthError) -> Response {
    let (status, value) = match error {
        AuthError::InvalidToken(message) => (
            StatusCode::UNAUTHORIZED,
            format!(
                "Bearer resource_metadata=\"{}\", error=\"invalid_token\", error_description=\"{}\"",
                auth.metadata_url(),
                message.replace(['"', '\\'], "'")
            ),
        ),
        AuthError::InsufficientScope => (
            StatusCode::FORBIDDEN,
            format!(
                "Bearer resource_metadata=\"{}\", error=\"insufficient_scope\", scope=\"{READ_SCOPE}\"",
                auth.metadata_url()
            ),
        ),
    };
    let body = if status == StatusCode::FORBIDDEN {
        "forbidden"
    } else {
        "unauthorized"
    };
    let mut response = (status, body).into_response();
    if let Ok(value) = HeaderValue::from_str(&value) {
        response.headers_mut().insert(header::WWW_AUTHENTICATE, value);
    }
    response
}

/// The token of an `Authorization: Bearer <token>` header (the scheme in any case).
fn bearer(value: &str) -> Option<&str> {
    let (scheme, token) = value.trim().split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

async fn authenticate(State(auth): State<HttpAuth>, mut request: Request, next: Next) -> Response {
    let identity = match &auth {
        HttpAuth::Insecure(identity) => identity.clone(),
        HttpAuth::Jwt(jwt) => {
            let token = request
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(bearer);
            let Some(token) = token else {
                return challenge(jwt, &AuthError::InvalidToken("missing bearer token".into()));
            };
            match jwt.identity(token) {
                Ok(identity) => identity,
                Err(error) => return challenge(jwt, &error),
            }
        }
    };
    request.extensions_mut().insert(identity);
    next.run(request).await
}

async fn resource_metadata(State(auth): State<HttpAuth>) -> Response {
    match auth {
        HttpAuth::Jwt(jwt) => axum::Json(serde_json::json!({
            "resource": jwt.resource,
            "authorization_servers": jwt.authorization_servers,
            "bearer_methods_supported": ["header"],
            "scopes_supported": [READ_SCOPE, "kiwi:label:public", "kiwi:label:internal", "kiwi:label:confidential", "kiwi:label:restricted"],
        }))
        .into_response(),
        HttpAuth::Insecure(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Sessions held at once; a new one beyond it is refused (`503`) until an old one
/// closes or idles out (five minutes without a request).
pub const MAX_SESSIONS: usize = 1024;

/// The axum router: `/mcp` behind authentication, the metadata document, `/healthz`.
pub fn router(service: Arc<KiwiService>, auth: HttpAuth, allowed_hosts: Vec<String>) -> Router {
    router_with_sessions(service, auth, allowed_hosts, MAX_SESSIONS)
}

/// [`router`] with another cap on the sessions held at once.
pub fn router_with_sessions(
    service: Arc<KiwiService>,
    auth: HttpAuth,
    allowed_hosts: Vec<String>,
    max_sessions: usize,
) -> Router {
    let mut config = StreamableHttpServerConfig::default();
    if allowed_hosts.is_empty() {
        config = config.disable_allowed_hosts();
    } else {
        config = config.with_allowed_hosts(allowed_hosts);
    }
    let sessions = Arc::new(LocalSessionManager::default());
    let mcp = StreamableHttpService::new(
        move || Ok(KiwiServer::new(Arc::clone(&service), IdentitySource::Request)),
        Arc::clone(&sessions),
        config,
    );
    let limit = SessionLimit {
        sessions,
        max: max_sessions,
    };
    Router::new()
        .route_service("/mcp", mcp)
        .route_layer(middleware::from_fn_with_state(limit, limit_sessions))
        .route_layer(middleware::from_fn_with_state(auth.clone(), authenticate))
        // The metadata at the well-known path and at it followed by any resource path,
        // so the URL a challenge names exists whatever path the resource has.
        .route(METADATA_PATH, get(resource_metadata))
        .route(&format!("{METADATA_PATH}/{{*resource}}"), get(resource_metadata))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(auth)
}

#[derive(Clone)]
struct SessionLimit {
    sessions: Arc<LocalSessionManager>,
    max: usize,
}

/// Refuses a request that would open a session (a POST without `Mcp-Session-Id`) while
/// the server holds as many as it may.
async fn limit_sessions(State(limit): State<SessionLimit>, request: Request, next: Next) -> Response {
    let opens =
        request.method() == axum::http::Method::POST && !request.headers().contains_key("mcp-session-id");
    if opens && limit.sessions.sessions.read().await.len() >= limit.max {
        tracing::warn!(
            max = limit.max,
            "session limit reached; a new session was refused"
        );
        let mut response = (StatusCode::SERVICE_UNAVAILABLE, "too many sessions").into_response();
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
        return response;
    }
    next.run(request).await
}

/// Resolves when the process is asked to stop: Ctrl-C, or SIGTERM (what service
/// managers send).
async fn stopped() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(error) => {
                tracing::warn!(%error, "cannot watch SIGTERM; stopping on Ctrl-C only");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Serves until the process is stopped.
pub async fn serve(router: Router, addr: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "kiwi listening on http");
    axum::serve(listener, router)
        .with_graceful_shutdown(stopped())
        .await
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use jsonwebtoken::jwk::JwkSet;
    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde_json::json;
    use std::time::SystemTime;

    const SECRET: &[u8] = b"a-test-secret-long-enough-for-hs256";
    const ISSUER: &str = "https://issuer.test";
    const RESOURCE: &str = "https://kiwi.test/mcp";

    fn now() -> u64 {
        jsonwebtoken::get_current_timestamp()
    }

    fn token(claims: serde_json::Value) -> String {
        encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(SECRET).unwrap(),
        )
        .unwrap()
    }

    fn claims(scope: &str) -> serde_json::Value {
        json!({"sub": "fiona", "iss": ISSUER, "aud": RESOURCE, "exp": now() + 600, "scope": scope, "client_id": "claude-code"})
    }

    #[test]
    fn tokens_need_a_kiwi_scope_and_a_reached_nbf() {
        let auth = JwtAuth::hs256(SECRET, ISSUER, RESOURCE).unwrap();
        let ok = auth.identity(&token(claims("kiwi:read"))).unwrap();
        assert_eq!(
            (ok.principal.as_str(), ok.agent.as_str()),
            ("fiona", "claude-code")
        );
        assert_eq!(ok.session_labels, None);
        let narrow = auth.identity(&token(claims("openid kiwi:label:public"))).unwrap();
        assert_eq!(narrow.session_labels, Some(vec!["public".to_owned()]));
        for scope in ["", "openid profile", "kiwi:label:"] {
            assert_eq!(
                auth.identity(&token(claims(scope))),
                Err(AuthError::InsufficientScope),
                "{scope:?}"
            );
        }
        let mut list = claims("");
        list["scp"] = json!(["kiwi:read"]);
        assert!(auth.identity(&token(list)).is_ok(), "scp as a list");
        let mut early = claims("kiwi:read");
        early["nbf"] = json!(now() + 3600);
        assert!(matches!(
            auth.identity(&token(early)),
            Err(AuthError::InvalidToken(_))
        ));
        // A token naming no client acts through the unidentified agent.
        let mut anonymous = claims("kiwi:read");
        anonymous.as_object_mut().unwrap().remove("client_id");
        assert_eq!(
            auth.identity(&token(anonymous)).unwrap().agent,
            UNIDENTIFIED_AGENT
        );
    }

    #[test]
    fn the_bearer_scheme_is_case_insensitive() {
        assert_eq!(bearer("Bearer abc"), Some("abc"));
        assert_eq!(bearer("bearer abc"), Some("abc"));
        assert_eq!(bearer("BEARER  abc "), Some("abc"));
        assert_eq!(bearer("Basic abc"), None);
        assert_eq!(bearer("Bearer"), None);
    }

    // Throwaway P-256 key pairs, for these tests only.
    const KEY_A: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgv1qrmN43/mY4pZO8
wmgn+sZzk4jCLbmBC+y2kLyxppOhRANCAARpHrG4DkVLLpdV3f4abqQFgnsFS7EO
qxOCfzSxQ/2zf7AMQCctK3f+XL6UlGUBMsf1OoZn9hr6Et1+vrWGbTJT
-----END PRIVATE KEY-----
";
    const PUB_A: &str = "-----BEGIN PUBLIC KEY-----
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEaR6xuA5FSy6XVd3+Gm6kBYJ7BUux
DqsTgn80sUP9s3+wDEAnLSt3/ly+lJRlATLH9TqGZ/Ya+hLdfr61hm0yUw==
-----END PUBLIC KEY-----
";
    const KEY_B: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg+ybhEmBNQHcxUXai
UeUG2rohjdq5eBagcufP3Ot4Q7mhRANCAARfHxL1F2y03GBVBybiF4THmZZHH7L8
XUlqzspB1LWyfqfAp+wDRN6ZowoZkO6FPP9usIlm6dI2KJvcfZ2Vg+ub
-----END PRIVATE KEY-----
";
    const PUB_B: &str = "-----BEGIN PUBLIC KEY-----
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEXx8S9RdstNxgVQcm4heEx5mWRx+y
/F1Jas7KQdS1sn6nwKfsA0TemaMKGZDuhTz/brCJZunSNiib3H2dlYPrmw==
-----END PUBLIC KEY-----
";

    fn signed(private: &str, kid: Option<&str>) -> String {
        let mut header = Header::new(Algorithm::ES256);
        header.kid = kid.map(str::to_owned);
        let key = EncodingKey::from_ec_pem(private.as_bytes()).unwrap();
        encode(&header, &claims("kiwi:read"), &key).unwrap()
    }

    #[test]
    fn keys_rotate_through_a_bundle_or_a_jwks_file() {
        // Both keys in one bundle: tokens of either validate during the rotation.
        let bundle = JwtAuth::rs256_pem(format!("{PUB_A}{PUB_B}").as_bytes(), ISSUER, RESOURCE).unwrap();
        assert!(bundle.identity(&signed(KEY_A, None)).is_ok());
        assert!(bundle.identity(&signed(KEY_B, None)).is_ok());
        let only_a = JwtAuth::rs256_pem(PUB_A.as_bytes(), ISSUER, RESOURCE).unwrap();
        assert!(only_a.identity(&signed(KEY_B, None)).is_err());

        // A JWKS file chooses by kid, and is read again when it changes.
        let jwk = |private: &str, kid: &str| {
            let key = EncodingKey::from_ec_pem(private.as_bytes()).unwrap();
            let mut jwk = jsonwebtoken::jwk::Jwk::from_encoding_key(&key, Algorithm::ES256).unwrap();
            jwk.common.key_id = Some(kid.to_owned());
            jwk
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jwks.json");
        let write = |keys: Vec<jsonwebtoken::jwk::Jwk>| {
            std::fs::write(&path, serde_json::to_vec(&JwkSet { keys }).unwrap()).unwrap();
        };
        write(vec![jwk(KEY_A, "a")]);
        let auth = JwtAuth::key_file(&path, KeyFormat::Jwks, ISSUER, RESOURCE).unwrap();
        if let Some(file) = &auth.file {
            file.lock().unwrap().every = Duration::ZERO;
        }
        assert!(auth.identity(&signed(KEY_A, Some("a"))).is_ok());
        assert!(auth.identity(&signed(KEY_B, Some("b"))).is_err());
        assert!(
            auth.identity(&signed(KEY_B, Some("a"))).is_err(),
            "a kid picks its key"
        );
        // The new key is published alongside the old one...
        std::thread::sleep(Duration::from_millis(20));
        write(vec![jwk(KEY_A, "a"), jwk(KEY_B, "b")]);
        bump(&path);
        assert!(auth.identity(&signed(KEY_B, Some("b"))).is_ok());
        assert!(auth.identity(&signed(KEY_A, Some("a"))).is_ok());
        // ...then the old one is retired; a broken file keeps the last good keys.
        write(vec![jwk(KEY_B, "b")]);
        bump(&path);
        assert!(auth.identity(&signed(KEY_A, Some("a"))).is_err());
        std::fs::write(&path, b"{not json").unwrap();
        bump(&path);
        assert!(auth.identity(&signed(KEY_B, Some("b"))).is_ok());
    }

    /// Moves a file's modification time forward, so a change is seen whatever the
    /// file system's time resolution.
    fn bump(path: &Path) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let step = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(SystemTime::now() + Duration::from_secs(step * 10))
            .unwrap();
    }

    #[test]
    fn key_files_take_each_key_on_its_own() {
        let jwk = |private: &str, kid: &str| {
            let key = EncodingKey::from_ec_pem(private.as_bytes()).unwrap();
            let mut jwk = jsonwebtoken::jwk::Jwk::from_encoding_key(&key, Algorithm::ES256).unwrap();
            jwk.common.key_id = Some(kid.to_owned());
            serde_json::to_value(jwk).unwrap()
        };
        let secret = json!({"kty": "oct", "k": "c2VjcmV0", "kid": "s"});
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jwks.json");
        let write = |keys: Vec<serde_json::Value>| {
            std::fs::write(&path, serde_json::to_vec(&json!({ "keys": keys })).unwrap()).unwrap();
        };
        // A key Kiwi does not take is dropped; the good one beside it is used.
        write(vec![secret.clone(), jwk(KEY_A, "a")]);
        let auth = JwtAuth::key_file(&path, KeyFormat::Jwks, ISSUER, RESOURCE).unwrap();
        if let Some(file) = &auth.file {
            file.lock().unwrap().every = Duration::ZERO;
        }
        assert!(auth.identity(&signed(KEY_A, Some("a"))).is_ok());
        // The same in a PEM bundle: a block that is not a public key is skipped.
        let pem = format!("-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----\n{PUB_B}");
        let bundle = dir.path().join("keys.pem");
        std::fs::write(&bundle, pem).unwrap();
        let from_pem = JwtAuth::key_file(&bundle, KeyFormat::Pem, ISSUER, RESOURCE).unwrap();
        assert!(from_pem.identity(&signed(KEY_B, None)).is_ok());
        // A file whose every key is refused is broken: the previous keys stay.
        std::thread::sleep(Duration::from_millis(20));
        write(vec![secret]);
        bump(&path);
        assert!(auth.identity(&signed(KEY_A, Some("a"))).is_ok());
        // A valid empty set revokes every key.
        write(Vec::new());
        bump(&path);
        assert!(auth.identity(&signed(KEY_A, Some("a"))).is_err());
        // At start, a file needs at least one usable key.
        assert!(JwtAuth::key_file(&path, KeyFormat::Jwks, ISSUER, RESOURCE).is_err());
        // A change is seen by content, whatever the modification time says, and a
        // missing file keeps the keys.
        write(vec![jwk(KEY_A, "a")]);
        let auth = JwtAuth::key_file(&path, KeyFormat::Jwks, ISSUER, RESOURCE).unwrap();
        if let Some(file) = &auth.file {
            file.lock().unwrap().every = Duration::ZERO;
        }
        let stamp = std::fs::metadata(&path).unwrap().modified().unwrap();
        write(vec![jwk(KEY_B, "b")]);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(stamp)
            .unwrap();
        assert!(auth.identity(&signed(KEY_B, Some("b"))).is_ok());
        assert!(auth.identity(&signed(KEY_A, Some("a"))).is_err());
        std::fs::remove_file(&path).unwrap();
        assert!(
            auth.identity(&signed(KEY_B, Some("b"))).is_ok(),
            "a missing file keeps the keys"
        );
    }

    #[test]
    fn challenges_name_the_metadata_route_that_exists() {
        assert_eq!(
            metadata_url("https://kiwi.test/mcp"),
            "https://kiwi.test/.well-known/oauth-protected-resource/mcp"
        );
        assert_eq!(
            metadata_url("https://kiwi.test/kb/mcp/"),
            "https://kiwi.test/.well-known/oauth-protected-resource/kb/mcp"
        );
        assert_eq!(
            metadata_url("https://kiwi.test"),
            "https://kiwi.test/.well-known/oauth-protected-resource"
        );
    }

    #[test]
    fn unauthenticated_http_is_its_own_channel() {
        let HttpAuth::Insecure(identity) = HttpAuth::insecure("fiona", "claude-code") else {
            panic!("insecure");
        };
        assert_eq!(identity.channel, INSECURE_CHANNEL);
        assert_ne!(identity.channel, "http");
    }

    #[test]
    fn pem_bundles_split_into_blocks() {
        let bundle = "junk\n-----BEGIN PUBLIC KEY-----\nAAA\n-----END PUBLIC KEY-----\n\n-----BEGIN PUBLIC KEY-----\nBBB\n-----END PUBLIC KEY-----\n";
        let blocks = pem_blocks(bundle);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[1].contains("BBB") && blocks[1].ends_with("-----END PUBLIC KEY-----"));
        let usable = |set: Result<KeySet, String>| set.and_then(KeySet::usable).map(|(keys, _)| keys.len());
        assert!(usable(pem_keys(bundle.as_bytes())).is_err(), "not real keys");
        assert!(usable(pem_keys(b"no keys here")).is_err());
        assert_eq!(usable(pem_keys(b"\n")), Ok(0), "an empty file is an empty set");
        assert!(
            usable(jwks_keys(br#"{"keys": [{"kty": "oct", "k": "c2VjcmV0"}]}"#)).is_err(),
            "no shared secrets"
        );
        assert_eq!(usable(jwks_keys(br#"{"keys": []}"#)), Ok(0));
        assert!(usable(jwks_keys(b"{not json")).is_err());
    }
}
