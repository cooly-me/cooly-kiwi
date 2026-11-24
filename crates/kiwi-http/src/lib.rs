//! Kiwi's outbound HTTP: the one client the embedding, rerank and audit-sink plugins
//! share.
//!
//! - Endpoints must be `https`, or `http` on a loopback address (a local model server):
//!   corpus text never crosses a network in clear text.
//! - Secrets come from environment variables named in the configuration, never from the
//!   configuration itself, and never appear in errors or logs. A corpus configuration
//!   may name only variables the operator lists (comma-separated) in `KIWI_SECRET_ENV`:
//!   a configuration change cannot ship the server's other secrets (cloud credentials,
//!   a token-signing secret) to an endpoint it chooses. Variables the server reserves
//!   for itself ([`reserve_secret`]) are refused even when listed.
//! - URLs carry no credentials and no query string (keys in a query string would end up
//!   in error messages), and errors name the endpoint by its URL.
//! - Requests have a global timeout, follow no redirects (a redirect could carry the
//!   bearer token elsewhere) and honour `HTTPS_PROXY` / `NO_PROXY`, except that a
//!   loopback endpoint is always reached directly: plain `http` never goes through a
//!   proxy.
//! - Transient failures (transport errors, 408, 429, 5xx) are retried with exponential
//!   backoff and jitter, honouring `Retry-After`; other statuses fail at once.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use http::Uri;

/// Errors of an outbound request.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    /// The configuration is not usable (bad URL, missing secret).
    #[error("{0}")]
    Config(String),
    /// The server answered with a status that is not retried, or kept failing.
    #[error("{url} answered {status}: {excerpt}")]
    Status {
        /// The endpoint (without credentials).
        url: String,
        /// The HTTP status.
        status: u16,
        /// The start of the response body.
        excerpt: String,
    },
    /// The request could not be sent or the answer not read (including timeouts).
    #[error("{url}: {message}")]
    Transport {
        /// The endpoint.
        url: String,
        /// What failed.
        message: String,
    },
    /// The overall deadline passed before an attempt succeeded.
    #[error("{url}: no answer within {millis} ms")]
    Deadline {
        /// The endpoint.
        url: String,
        /// The deadline.
        millis: u128,
    },
}

/// A validated endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    url: String,
    host: String,
    loopback: bool,
}

fn is_loopback(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    bare == "localhost" || bare.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

impl Endpoint {
    /// Parses and checks a URL: `https://…`, or `http://…` on a loopback host. User
    /// information in the URL is refused (credentials belong in an environment variable).
    pub fn parse(url: &str) -> Result<Self, HttpError> {
        let uri: Uri = url
            .trim()
            .parse()
            .map_err(|e| HttpError::Config(format!("`{url}` is not a URL: {e}")))?;
        let authority = uri
            .authority()
            .ok_or_else(|| HttpError::Config(format!("`{url}` has no host")))?;
        if uri.query().is_some() || url.contains('#') {
            return Err(HttpError::Config(format!(
                "`{}` has a query string or fragment; put keys in an environment variable",
                uri.host().unwrap_or_default()
            )));
        }
        if authority.as_str().contains('@') {
            return Err(HttpError::Config(format!(
                "`{}` carries credentials; put secrets in an environment variable instead",
                uri.host().unwrap_or_default()
            )));
        }
        let host = authority.host().to_owned();
        match uri.scheme_str() {
            Some("https") => {}
            Some("http") if is_loopback(&host) => {}
            Some("http") => {
                return Err(HttpError::Config(format!(
                    "`{url}`: plain http is only allowed to a loopback address; use https"
                )));
            }
            _ => return Err(HttpError::Config(format!("`{url}` must be an http(s) URL"))),
        }
        let loopback = is_loopback(&host);
        Ok(Self {
            url: url.trim().to_owned(),
            host,
            loopback,
        })
    }

    /// The URL.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The host, as declared in a plugin's capability manifest.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// This endpoint with `path` appended (`https://h/v1` + `embeddings`).
    pub fn join(&self, path: &str) -> Result<Self, HttpError> {
        Self::parse(&format!(
            "{}/{}",
            self.url.trim_end_matches('/'),
            path.trim_start_matches('/')
        ))
    }
}

/// The operator's list (comma-separated) of the variables a corpus configuration may
/// name for a secret; no other variable is ever read for one.
pub const SECRET_ENV_ALLOW: &str = "KIWI_SECRET_ENV";

static RESERVED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// Keeps the variable `name` (a secret of the server itself, such as the token-signing
/// secret) from ever being read for a plugin, whatever the configuration and
/// `KIWI_SECRET_ENV` say. Call it before any corpus configuration is opened.
pub fn reserve_secret(name: &str) {
    if let Ok(mut reserved) = RESERVED.lock() {
        reserved.insert(name.to_owned());
    }
}

/// Whether the server reserved the variable `name` for itself.
pub fn secret_reserved(name: &str) -> bool {
    RESERVED.lock().map(|r| r.contains(name)).unwrap_or(true)
}

/// Whether a corpus configuration may name the variable `name` for a secret: one the
/// operator listed in `KIWI_SECRET_ENV` (`allow_list`), never one the server reserved,
/// and never `KIWI_SECRET_ENV` itself.
pub fn secret_env_allowed(name: &str, allow_list: Option<&str>) -> bool {
    if secret_reserved(name) || name == SECRET_ENV_ALLOW {
        return false;
    }
    allow_list.is_some_and(|list| list.split(',').any(|allowed| allowed.trim() == name))
}

/// Reads a secret from the environment variable `name`, which the configuration names;
/// see [`secret_env_allowed`] for the variables it may name.
pub fn secret_from_env(name: &str) -> Result<String, HttpError> {
    if secret_reserved(name) {
        return Err(HttpError::Config(format!(
            "the configuration may not read ${name}: it holds a secret of the server itself"
        )));
    }
    let allow_list = std::env::var(SECRET_ENV_ALLOW).ok();
    if !secret_env_allowed(name, allow_list.as_deref()) {
        return Err(HttpError::Config(format!(
            "the configuration may not read ${name}: the operator lists the variables it may read in ${SECRET_ENV_ALLOW}"
        )));
    }
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => Ok(value.trim().to_owned()),
        Ok(_) => Err(HttpError::Config(format!("environment variable {name} is empty"))),
        Err(_) => Err(HttpError::Config(format!(
            "environment variable {name} is not set"
        ))),
    }
}

/// Timeouts and retries of a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// Limit for one attempt (connect, send and read).
    pub timeout: Duration,
    /// Attempts after the first one.
    pub retries: u32,
    /// The first backoff; it doubles per retry, up to [`MAX_BACKOFF`].
    pub backoff: Duration,
    /// Limit for all attempts together, including backoff.
    pub deadline: Duration,
}

/// The longest wait between two attempts.
pub const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// The largest response body read.
const MAX_BODY: u64 = 64 * 1024 * 1024;

/// A blocking HTTP client with a retry policy.
#[derive(Debug, Clone)]
pub struct Client {
    /// Through the environment's proxy, if any.
    agent: ureq::Agent,
    /// Without a proxy, for loopback endpoints.
    direct: ureq::Agent,
    policy: Policy,
}

/// A successful answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// The status (2xx).
    pub status: u16,
    /// The body.
    pub body: Vec<u8>,
}

static JITTER: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);

fn jitter(range: Duration) -> Duration {
    // xorshift over a process-wide state: enough to spread retries of many clients.
    let mut x = JITTER.load(Ordering::Relaxed);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    JITTER.store(x, Ordering::Relaxed);
    let millis = range.as_millis().max(1) as u64;
    Duration::from_millis(x % millis)
}

fn retry_after(response: &http::Response<ureq::Body>) -> Option<Duration> {
    let value = response.headers().get("retry-after")?.to_str().ok()?;
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

impl Client {
    /// A client with `policy`.
    pub fn new(policy: Policy) -> Self {
        let agent = |proxy: Option<ureq::Proxy>| {
            let config = ureq::Agent::config_builder()
                .timeout_global(Some(policy.timeout))
                .http_status_as_error(false)
                .max_redirects(0)
                .proxy(proxy)
                .user_agent(concat!("kiwi/", env!("CARGO_PKG_VERSION")))
                .build();
            ureq::Agent::new_with_config(config)
        };
        Self {
            agent: agent(ureq::Proxy::try_from_env()),
            direct: agent(None),
            policy,
        }
    }

    /// POSTs `body` with `content_type`, with an optional bearer token, retrying
    /// transient failures within the policy.
    pub fn post(
        &self,
        endpoint: &Endpoint,
        content_type: &str,
        body: &[u8],
        bearer: Option<&str>,
    ) -> Result<Answer, HttpError> {
        let started = Instant::now();
        let mut attempt = 0u32;
        loop {
            // No attempt runs past the deadline.
            let left = self.policy.deadline.saturating_sub(started.elapsed());
            if left.is_zero() {
                return Err(HttpError::Deadline {
                    url: endpoint.url.clone(),
                    millis: self.policy.deadline.as_millis(),
                });
            }
            let outcome = self.attempt(
                endpoint,
                content_type,
                body,
                bearer,
                self.policy.timeout.min(left),
            );
            let wait = match outcome {
                Attempt::Done(answer) => return Ok(answer),
                Attempt::Fatal(error) => return Err(error),
                Attempt::Retry { error, after } => {
                    if attempt >= self.policy.retries {
                        return Err(error);
                    }
                    let exponential = self
                        .policy
                        .backoff
                        .saturating_mul(2u32.saturating_pow(attempt))
                        .min(MAX_BACKOFF);
                    let wait = after.unwrap_or(exponential).min(MAX_BACKOFF) + jitter(self.policy.backoff);
                    if started.elapsed() + wait >= self.policy.deadline {
                        return Err(HttpError::Deadline {
                            url: endpoint.url.clone(),
                            millis: self.policy.deadline.as_millis(),
                        });
                    }
                    wait
                }
            };
            std::thread::sleep(wait);
            attempt += 1;
        }
    }

    fn attempt(
        &self,
        endpoint: &Endpoint,
        content_type: &str,
        body: &[u8],
        bearer: Option<&str>,
        timeout: Duration,
    ) -> Attempt {
        let transport = |message: String| HttpError::Transport {
            url: endpoint.url.clone(),
            message,
        };
        let agent = if endpoint.loopback {
            &self.direct
        } else {
            &self.agent
        };
        let mut request = agent
            .post(endpoint.url.as_str())
            .config()
            .timeout_global(Some(timeout))
            .build()
            .header("content-type", content_type)
            .header("accept", "application/json");
        if let Some(token) = bearer {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let mut response = match request.send(body) {
            Ok(response) => response,
            Err(error) => {
                return Attempt::Retry {
                    error: transport(error.to_string()),
                    after: None,
                };
            }
        };
        let status = response.status().as_u16();
        let after = retry_after(&response);
        let bytes = response.body_mut().with_config().limit(MAX_BODY).read_to_vec();
        let bytes = match bytes {
            Ok(bytes) => bytes,
            // The server accepted the request: sending it again could apply it twice
            // (an audit batch shipped twice), so a broken 2xx answer is not retried.
            Err(error) if (200..300).contains(&status) => {
                return Attempt::Fatal(transport(format!("reading the answer: {error}")));
            }
            Err(error) => {
                return Attempt::Retry {
                    error: transport(format!("reading the answer: {error}")),
                    after: None,
                };
            }
        };
        if (200..300).contains(&status) {
            return Attempt::Done(Answer { status, body: bytes });
        }
        let excerpt: String = String::from_utf8_lossy(&bytes).chars().take(200).collect();
        let error = HttpError::Status {
            url: endpoint.url.clone(),
            status,
            excerpt: excerpt.trim().to_owned(),
        };
        if status == 408 || status == 429 || status >= 500 {
            Attempt::Retry { error, after }
        } else {
            Attempt::Fatal(error)
        }
    }
}

enum Attempt {
    Done(Answer),
    Retry {
        error: HttpError,
        after: Option<Duration>,
    },
    Fatal(HttpError),
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn endpoints_need_tls_off_loopback() {
        assert!(Endpoint::parse("https://api.example.com/v1").is_ok());
        assert!(Endpoint::parse("http://127.0.0.1:8080/rerank").is_ok());
        assert!(Endpoint::parse("http://localhost:8080").is_ok());
        assert!(Endpoint::parse("http://[::1]:9/x").is_ok());
        assert!(Endpoint::parse("http://api.example.com/v1").is_err());
        assert!(Endpoint::parse("ftp://example.com").is_err());
        assert!(Endpoint::parse("https://user:secret@example.com").is_err());
        assert!(Endpoint::parse("not a url").is_err());
        assert!(Endpoint::parse("https://api.example.com/v1?api-key=secret").is_err());
        assert!(Endpoint::parse("https://api.example.com/v1#x").is_err());
        let base = Endpoint::parse("https://api.example.com/v1/").unwrap();
        assert_eq!(
            base.join("/embeddings").unwrap().url(),
            "https://api.example.com/v1/embeddings"
        );
        assert_eq!(base.host(), "api.example.com");
    }

    #[test]
    fn secrets_come_from_the_environment() {
        let err = secret_from_env("KIWI_TEST_SURELY_UNSET_VARIABLE").unwrap_err();
        assert!(err.to_string().contains(SECRET_ENV_ALLOW), "{err}");
        // SAFETY: the other test of this module that reads the environment reads only
        // variables this list never names.
        unsafe { std::env::set_var(SECRET_ENV_ALLOW, "KIWI_TEST_SURELY_UNSET_VARIABLE") };
        let err = secret_from_env("KIWI_TEST_SURELY_UNSET_VARIABLE").unwrap_err();
        assert!(err.to_string().contains("is not set"), "{err}");
    }

    #[test]
    fn a_configuration_names_only_allowed_variables() {
        assert!(
            !secret_env_allowed("KIWI_OPENAI_KEY", None),
            "no blanket KIWI_* allowance"
        );
        assert!(!secret_env_allowed("KIWI_TOKEN_SECRET", Some("OPENAI_API_KEY")));
        assert!(secret_env_allowed("KIWI_OPENAI_KEY", Some("KIWI_OPENAI_KEY")));
        assert!(!secret_env_allowed("AWS_SECRET_ACCESS_KEY", None));
        assert!(!secret_env_allowed(
            "OPENAI_API_KEY",
            Some("SIEM_TOKEN, RERANK_KEY")
        ));
        assert!(secret_env_allowed(
            "OPENAI_API_KEY",
            Some("SIEM_TOKEN, OPENAI_API_KEY")
        ));
        assert!(!secret_env_allowed(SECRET_ENV_ALLOW, Some(SECRET_ENV_ALLOW)));
        reserve_secret("KIWI_TEST_TOKEN_SIGNING_SECRET");
        assert!(!secret_env_allowed(
            "KIWI_TEST_TOKEN_SIGNING_SECRET",
            Some("KIWI_TEST_TOKEN_SIGNING_SECRET")
        ));
        let err = secret_from_env("KIWI_TEST_TOKEN_SIGNING_SECRET").unwrap_err();
        assert!(err.to_string().contains("secret of the server"), "{err}");
        // HOME is surely set, yet never read for a plugin.
        let err = secret_from_env("HOME").unwrap_err();
        assert!(err.to_string().contains("may not read"), "{err}");
    }

    /// Serves canned statuses in order, recording each request's head and body.
    fn server(statuses: Vec<(u16, &'static str)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            for (status, body) in statuses {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                let mut reader = BufReader::new(stream);
                let mut head = String::new();
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap();
                    }
                    if line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                let mut payload = vec![0; length];
                reader.read_exact(&mut payload).unwrap();
                log.lock()
                    .unwrap()
                    .push(format!("{head}\n{}", String::from_utf8_lossy(&payload)));
                let mut stream = reader.into_inner();
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nretry-after: 0\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (format!("http://{addr}/api"), seen)
    }

    fn policy(retries: u32) -> Policy {
        Policy {
            timeout: Duration::from_secs(5),
            retries,
            backoff: Duration::from_millis(5),
            deadline: Duration::from_secs(10),
        }
    }

    #[test]
    fn retries_transient_failures() {
        let (url, seen) = server(vec![(503, "busy"), (429, "slow down"), (200, "{\"ok\":1}")]);
        let client = Client::new(policy(3));
        let answer = client
            .post(
                &Endpoint::parse(&url).unwrap(),
                "application/json",
                b"{}",
                Some("t0ken"),
            )
            .unwrap();
        assert_eq!(answer.body, b"{\"ok\":1}");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert!(
            seen[0]
                .to_ascii_lowercase()
                .contains("authorization: bearer t0ken")
        );
    }

    #[test]
    fn client_errors_are_not_retried_and_hide_the_token() {
        let (url, seen) = server(vec![(400, "bad input"), (200, "{}")]);
        let client = Client::new(policy(3));
        let error = client
            .post(
                &Endpoint::parse(&url).unwrap(),
                "application/json",
                b"{}",
                Some("s3cret"),
            )
            .unwrap_err();
        assert!(matches!(error, HttpError::Status { status: 400, .. }));
        assert!(!error.to_string().contains("s3cret"));
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn gives_up_after_the_retries() {
        let (url, _) = server(vec![(500, "a"), (500, "b")]);
        let error = Client::new(policy(1))
            .post(&Endpoint::parse(&url).unwrap(), "application/json", b"{}", None)
            .unwrap_err();
        assert!(matches!(error, HttpError::Status { status: 500, .. }));
    }

    #[test]
    fn unreachable_servers_fail_within_the_deadline() {
        // Bind and drop: nothing listens on the port any more.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let started = Instant::now();
        let error = Client::new(Policy {
            timeout: Duration::from_millis(500),
            retries: 10,
            backoff: Duration::from_millis(50),
            deadline: Duration::from_millis(300),
        })
        .post(
            &Endpoint::parse(&format!("http://127.0.0.1:{port}/x")).unwrap(),
            "application/json",
            b"{}",
            None,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            HttpError::Deadline { .. } | HttpError::Transport { .. }
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
