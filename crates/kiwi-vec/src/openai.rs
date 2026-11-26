//! An embedder for any server with the OpenAI embeddings API (`POST {url}/embeddings`):
//! OpenAI itself, vLLM, Text Embeddings Inference, Ollama,
//! LiteLLM, Infinity.
//!
//! Requests are batched, bounded by a timeout, and retried with backoff on transient
//! failures. The API key comes from an environment variable named in `kiwi.toml`. The
//! plugin declares the server's host as network access, so the build sends it only the
//! atoms the policy lets `plugin:vec` read over the network.

use std::time::Duration;

use kiwi_core::KiwiError;
use kiwi_core::plugin::Capabilities;
use kiwi_http::{Client, Endpoint, Policy, secret_from_env};
use serde::Deserialize;

use crate::embed::Embedder;

fn err(message: impl std::fmt::Display) -> KiwiError {
    KiwiError::plugin("vec", message)
}

/// Settings of the embedding client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiSettings {
    /// Base URL of the API, e.g. `https://api.openai.com/v1`.
    pub url: String,
    /// The model, e.g. `text-embedding-3-small`.
    pub model: String,
    /// Dimensions to ask for (models with Matryoshka training accept it); vectors are
    /// cut to 384 either way.
    pub dimensions: Option<u32>,
    /// Environment variable holding the API key (`None` for servers without auth).
    pub api_key_env: Option<String>,
    /// Texts per request.
    pub batch_size: usize,
    /// Limit for one request.
    pub timeout: Duration,
    /// Retries of a failed request.
    pub retries: u32,
    /// Prefix added to queries (`query: ` for E5 models).
    pub query_prefix: String,
    /// Prefix added to passages (`passage: ` for E5 models).
    pub passage_prefix: String,
}

impl OpenAiSettings {
    /// The name of the vector space these settings produce: the model, the requested
    /// dimensions and the prefixes. The server's URL is not part of it, so the same
    /// model behind another gateway queries the same segment.
    pub fn space(&self) -> String {
        let dims = self
            .dimensions
            .map_or_else(|| "native".to_owned(), |d| d.to_string());
        let mut name = format!("openai:{}:{dims}", self.model);
        if !self.query_prefix.is_empty() || !self.passage_prefix.is_empty() {
            let digest = blake3::hash(format!("{}\0{}", self.query_prefix, self.passage_prefix).as_bytes());
            name.push_str(&format!(":p{}", &digest.to_hex()[..8]));
        }
        name
    }
}

/// The embedding client.
pub struct OpenAiEmbedder {
    settings: OpenAiSettings,
    endpoint: Endpoint,
    /// Passages, at build time: the configured timeout and retries.
    client: Client,
    /// Queries, while an agent waits: at most one retry, within [`QUERY_DEADLINE`].
    queries: Client,
    token: Option<String>,
}

impl std::fmt::Debug for OpenAiEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiEmbedder")
            .field("url", &self.endpoint.url())
            .field("model", &self.settings.model)
            .finish()
    }
}

/// The longest a search waits for its query embeddings before it goes on without the
/// vector tier.
pub const QUERY_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
struct EmbeddingAnswer {
    data: Vec<EmbeddingItem>,
}

#[derive(Deserialize)]
struct EmbeddingItem {
    index: usize,
    embedding: Vec<f32>,
}

impl OpenAiEmbedder {
    /// A client for `settings`. The API key is read from its environment variable now,
    /// so a missing secret fails before any work.
    pub fn new(settings: OpenAiSettings) -> kiwi_core::Result<Self> {
        if settings.model.trim().is_empty() {
            return Err(err("plugins.vec.model must name the embedding model"));
        }
        if !(1..=2048).contains(&settings.batch_size) {
            return Err(err("plugins.vec.batch_size must be between 1 and 2048"));
        }
        let endpoint = Endpoint::parse(&settings.url)
            .and_then(|base| base.join("embeddings"))
            .map_err(err)?;
        let token = match &settings.api_key_env {
            Some(var) => Some(secret_from_env(var).map_err(err)?),
            None => None,
        };
        let client = Client::new(Policy {
            timeout: settings.timeout,
            retries: settings.retries,
            backoff: Duration::from_millis(500),
            deadline: settings.timeout.saturating_mul(settings.retries + 1) + Duration::from_secs(60),
        });
        let queries = Client::new(Policy {
            timeout: settings.timeout.min(QUERY_DEADLINE),
            retries: settings.retries.min(1),
            backoff: Duration::from_millis(200),
            deadline: QUERY_DEADLINE,
        });
        Ok(Self {
            settings,
            endpoint,
            client,
            queries,
            token,
        })
    }

    fn embed(&self, client: &Client, texts: Vec<String>) -> kiwi_core::Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for batch in texts.chunks(self.settings.batch_size) {
            let mut body = serde_json::json!({
                "model": self.settings.model,
                "input": batch,
                "encoding_format": "float",
            });
            if let Some(dimensions) = self.settings.dimensions {
                body["dimensions"] = serde_json::json!(dimensions);
            }
            let bytes = serde_json::to_vec(&body).map_err(err)?;
            let answer = client
                .post(&self.endpoint, "application/json", &bytes, self.token.as_deref())
                .map_err(err)?;
            let parsed: EmbeddingAnswer = serde_json::from_slice(&answer.body)
                .map_err(|e| err(format!("unexpected answer from {}: {e}", self.endpoint.url())))?;
            let mut vectors: Vec<Option<Vec<f32>>> = vec![None; batch.len()];
            for item in parsed.data {
                let slot = vectors
                    .get_mut(item.index)
                    .ok_or_else(|| err(format!("answer names input {} of {}", item.index, batch.len())))?;
                if item.embedding.is_empty() || item.embedding.iter().any(|v| !v.is_finite()) {
                    return Err(err(format!(
                        "input {} has an empty or invalid embedding",
                        item.index
                    )));
                }
                *slot = Some(item.embedding);
            }
            let width = vectors.first().and_then(|v| v.as_ref()).map_or(0, Vec::len);
            for (index, vector) in vectors.into_iter().enumerate() {
                let vector = vector.ok_or_else(|| err(format!("no embedding for input {index}")))?;
                if vector.len() != width {
                    return Err(err("embeddings of one batch differ in length"));
                }
                out.push(vector);
            }
        }
        Ok(out)
    }
}

impl Embedder for OpenAiEmbedder {
    fn name(&self) -> String {
        self.settings.space()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            content: true,
            labels: false,
            network: vec![self.endpoint.host().to_owned()],
        }
    }

    fn batch_size(&self) -> usize {
        self.settings.batch_size
    }

    fn embed_passages(&self, texts: &[String]) -> kiwi_core::Result<Vec<Vec<f32>>> {
        self.embed(
            &self.client,
            texts
                .iter()
                .map(|t| format!("{}{t}", self.settings.passage_prefix))
                .collect(),
        )
    }

    fn embed_query(&self, text: &str) -> kiwi_core::Result<Vec<f32>> {
        self.embed(
            &self.queries,
            vec![format!("{}{text}", self.settings.query_prefix)],
        )?
        .pop()
        .ok_or_else(|| err("no embedding returned"))
    }

    fn embed_queries(&self, texts: &[String]) -> kiwi_core::Result<Vec<Vec<f32>>> {
        self.embed(
            &self.queries,
            texts
                .iter()
                .map(|t| format!("{}{t}", self.settings.query_prefix))
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A tiny embeddings server: each input's vector is its length in dimension 0 and
    /// 1.0 elsewhere. The first `fail` requests get a 503.
    fn server(fail: usize) -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            let mut count = 0usize;
            while let Ok((stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream);
                let mut length = 0usize;
                let mut auth = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap();
                    }
                    if lower.starts_with("authorization:") {
                        auth = line.trim().to_owned();
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let mut request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                request["auth"] = serde_json::json!(auth);
                count += 1;
                let mut stream = reader.into_inner();
                if count <= fail {
                    let _ = write!(
                        stream,
                        "HTTP/1.1 503 Busy\r\ncontent-length: 4\r\nretry-after: 0\r\nconnection: close\r\n\r\nbusy"
                    );
                    continue;
                }
                let inputs: Vec<String> = serde_json::from_value(request["input"].clone()).unwrap();
                log.lock().unwrap().push(request);
                let data: Vec<serde_json::Value> = inputs
                    .iter()
                    .enumerate()
                    .rev()
                    .map(|(index, text)| {
                        let mut v = vec![1.0f32; 8];
                        v[0] = text.len() as f32;
                        serde_json::json!({"index": index, "embedding": v})
                    })
                    .collect();
                let answer = serde_json::json!({"data": data}).to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{answer}",
                    answer.len()
                );
            }
        });
        (format!("http://{addr}/v1"), seen)
    }

    fn settings(url: String) -> OpenAiSettings {
        OpenAiSettings {
            url,
            model: "text-embedding-3-small".into(),
            dimensions: Some(384),
            api_key_env: None,
            batch_size: 2,
            timeout: Duration::from_secs(5),
            retries: 3,
            query_prefix: "query: ".into(),
            passage_prefix: String::new(),
        }
    }

    #[test]
    fn batches_orders_and_retries() {
        let (url, seen) = server(1);
        let embedder = OpenAiEmbedder::new(settings(url)).unwrap();
        let texts: Vec<String> = ["a", "bb", "ccc"].iter().map(|s| (*s).to_owned()).collect();
        let vectors = embedder.embed_passages(&texts).unwrap();
        assert_eq!(
            vectors.iter().map(|v| v[0]).collect::<Vec<_>>(),
            vec![1.0, 2.0, 3.0]
        );
        let query = embedder.embed_query("x").unwrap();
        assert_eq!(query[0], "query: x".len() as f32);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3, "two passage batches and one query");
        assert_eq!(seen[0]["dimensions"], 384);
        assert_eq!(seen[0]["model"], "text-embedding-3-small");
        assert_eq!(seen[0]["auth"], "");
    }

    #[test]
    fn sends_the_key_from_the_environment() {
        let (url, seen) = server(0);
        // A variable only this test sets.
        // SAFETY: no other test reads or writes these variables' values in a way this
        // changes: the other test only names a variable that is never set.
        unsafe {
            std::env::set_var("KIWI_TEST_OPENAI_KEY", "sk-test-123");
            std::env::set_var("KIWI_SECRET_ENV", "KIWI_TEST_OPENAI_KEY");
        }
        let mut keyed = settings(url);
        keyed.api_key_env = Some("KIWI_TEST_OPENAI_KEY".into());
        let embedder = OpenAiEmbedder::new(keyed).unwrap();
        embedder.embed_query("x").unwrap();
        assert_eq!(
            seen.lock().unwrap()[0]["auth"],
            "authorization: Bearer sk-test-123"
        );
        assert!(!format!("{embedder:?}").contains("sk-test"));
    }

    #[test]
    fn names_its_vector_space() {
        let mut a = settings("https://api.openai.com/v1".into());
        a.query_prefix.clear();
        assert_eq!(a.space(), "openai:text-embedding-3-small:384");
        let mut b = a.clone();
        b.url = "https://gateway.example.com/v1".into();
        assert_eq!(a.space(), b.space(), "the URL is not part of the space");
        b.query_prefix = "query: ".into();
        assert_ne!(a.space(), b.space());
    }

    #[test]
    fn refuses_bad_settings() {
        assert!(OpenAiEmbedder::new(settings("http://api.example.com/v1".into())).is_err());
        let mut missing = settings("https://api.example.com/v1".into());
        missing.api_key_env = Some("KIWI_TEST_OPENAI_KEY_UNSET".into());
        assert!(
            OpenAiEmbedder::new(missing)
                .unwrap_err()
                .to_string()
                .contains("KIWI_TEST_OPENAI_KEY_UNSET")
        );
        let mut batch = settings("https://api.example.com/v1".into());
        batch.batch_size = 0;
        assert!(OpenAiEmbedder::new(batch).is_err());
    }

    #[test]
    fn a_dead_server_fails_the_call() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut dead = settings(format!("http://127.0.0.1:{port}/v1"));
        dead.retries = 1;
        dead.timeout = Duration::from_millis(300);
        let embedder = OpenAiEmbedder::new(dead).unwrap();
        assert!(embedder.embed_query("x").is_err());
    }
}
