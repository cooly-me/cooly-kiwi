//! Kiwi rerank plugin: a cross-encoder that reorders the top of a fused ranking.
//!
//! Two backends:
//! - `http`: any server with the common `/rerank` API, in the TEI shape
//!   (`{"query", "texts"}` → `[{"index", "score"}]`) or the Cohere shape used by Jina,
//!   Cohere, Voyage, vLLM and Infinity (`{"model", "query", "documents"}` →
//!   `{"results": [{"index", "relevance_score"}]}`);
//! - `local` (cargo feature `fastembed`): an ONNX cross-encoder run on the CPU.
//!
//! Every call is bounded by a timeout. A reranker that fails or runs out of time
//! returns an error, and the planner keeps the fused order.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::sync::Arc;
use std::time::Duration;

use kiwi_core::KiwiError;
use kiwi_core::plugin::{Capabilities, Reranker};
use kiwi_http::{Client, Endpoint, Policy, secret_from_env};
use serde::Deserialize;

/// The plugin name (and Cedar principal `plugin:rerank`).
pub const NAME: &str = "rerank";

fn err(message: impl std::fmt::Display) -> KiwiError {
    KiwiError::plugin(NAME, message)
}

/// The request and response shape of an HTTP reranker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiFormat {
    /// Hugging Face Text Embeddings Inference.
    Tei,
    /// Cohere, Jina, Voyage, vLLM, Infinity.
    Cohere,
}

impl ApiFormat {
    /// Parses `tei` or `cohere`.
    pub fn parse(name: &str) -> kiwi_core::Result<Self> {
        match name {
            "tei" => Ok(Self::Tei),
            "cohere" => Ok(Self::Cohere),
            other => Err(err(format!("format `{other}` is not tei or cohere"))),
        }
    }
}

/// Settings of an HTTP reranker.
#[derive(Debug, Clone)]
pub struct HttpSettings {
    /// The full endpoint URL, e.g. `http://127.0.0.1:8080/rerank`.
    pub url: String,
    /// The API shape.
    pub format: ApiFormat,
    /// Model name sent with the Cohere shape.
    pub model: String,
    /// Environment variable holding a bearer token, if the server needs one.
    pub api_key_env: Option<String>,
    /// Limit for the whole call.
    pub timeout: Duration,
}

/// A reranker behind an HTTP API.
pub struct HttpReranker {
    endpoint: Endpoint,
    client: Client,
    format: ApiFormat,
    model: String,
    token: Option<String>,
}

impl std::fmt::Debug for HttpReranker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpReranker")
            .field("url", &self.endpoint.url())
            .field("format", &self.format)
            .finish()
    }
}

#[derive(Deserialize)]
struct Scored {
    index: usize,
    #[serde(alias = "relevance_score")]
    score: f32,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RerankAnswer {
    Tei(Vec<Scored>),
    Cohere { results: Vec<Scored> },
}

impl HttpReranker {
    /// A client for `settings`; reads the token from its environment variable now, so a
    /// missing secret fails at startup rather than on the first query.
    pub fn new(settings: &HttpSettings) -> kiwi_core::Result<Self> {
        let endpoint = Endpoint::parse(&settings.url).map_err(err)?;
        let token = match &settings.api_key_env {
            Some(var) => Some(secret_from_env(var).map_err(err)?),
            None => None,
        };
        let client = Client::new(Policy {
            timeout: settings.timeout,
            retries: 0,
            backoff: Duration::from_millis(50),
            deadline: settings.timeout,
        });
        Ok(Self {
            endpoint,
            client,
            format: settings.format,
            model: settings.model.clone(),
            token,
        })
    }
}

impl Reranker for HttpReranker {
    fn name(&self) -> &str {
        NAME
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            content: true,
            labels: false,
            network: vec![self.endpoint.host().to_owned()],
        }
    }

    fn score(&self, query: &str, passages: &[String]) -> kiwi_core::Result<Vec<f32>> {
        let body = match self.format {
            ApiFormat::Tei => serde_json::json!({ "query": query, "texts": passages, "truncate": true }),
            ApiFormat::Cohere => serde_json::json!({
                "model": self.model,
                "query": query,
                "documents": passages,
                "top_n": passages.len(),
            }),
        };
        let bytes = serde_json::to_vec(&body).map_err(err)?;
        let answer = self
            .client
            .post(&self.endpoint, "application/json", &bytes, self.token.as_deref())
            .map_err(err)?;
        let parsed: RerankAnswer = serde_json::from_slice(&answer.body)
            .map_err(|e| err(format!("unexpected answer from {}: {e}", self.endpoint.url())))?;
        let scored = match parsed {
            RerankAnswer::Tei(scored) | RerankAnswer::Cohere { results: scored } => scored,
        };
        let mut scores = vec![f32::NAN; passages.len()];
        for item in scored {
            let slot = scores.get_mut(item.index).ok_or_else(|| {
                err(format!(
                    "answer names passage {} of {}",
                    item.index,
                    passages.len()
                ))
            })?;
            *slot = item.score;
        }
        if scores.iter().any(|s| !s.is_finite()) {
            return Err(err("the answer does not score every passage"));
        }
        Ok(scores)
    }
}

#[cfg(feature = "fastembed")]
mod local {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;

    use fastembed::{AutoDevicePolicy, RerankInitOptions, RerankerModel, TextRerank};
    use kiwi_core::plugin::{Capabilities, Reranker};

    use super::{NAME, err};

    /// Reranks running or queued for the model at once: one running and one waiting.
    const MAX_IN_FLIGHT: usize = 2;
    /// Tokens of query and passage the cross-encoder reads.
    const MAX_TOKENS: usize = 192;

    /// Counts a rerank as finished when its thread ends, however it ends.
    struct Release(Arc<AtomicUsize>);

    impl Drop for Release {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// A local ONNX cross-encoder run by fastembed. Passages never leave the machine.
    pub struct LocalReranker {
        model: Arc<OnceLock<Mutex<TextRerank>>>,
        /// Reranks running or waiting for the model, including ones past their timeout.
        in_flight: Arc<AtomicUsize>,
        kind: RerankerModel,
        cache_dir: PathBuf,
        timeout: Duration,
        name: String,
    }

    impl std::fmt::Debug for LocalReranker {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("LocalReranker")
                .field("model", &self.name)
                .finish()
        }
    }

    /// The local models: `bge-reranker-base` (English, Chinese), `bge-reranker-v2-m3`
    /// and `jina-reranker-v2-base-multilingual` (multilingual), `jina-reranker-v1-turbo-en`
    /// (English, small).
    pub fn model_kind(name: &str) -> kiwi_core::Result<RerankerModel> {
        match name {
            "bge-reranker-base" => Ok(RerankerModel::BGERerankerBase),
            "bge-reranker-v2-m3" => Ok(RerankerModel::BGERerankerV2M3),
            "jina-reranker-v1-turbo-en" => Ok(RerankerModel::JINARerankerV1TurboEn),
            "jina-reranker-v2-base-multilingual" => Ok(RerankerModel::JINARerankerV2BaseMultilingual),
            other => Err(err(format!("unknown local rerank model `{other}`"))),
        }
    }

    impl LocalReranker {
        /// A reranker loading `model` (downloaded once into `cache_dir`) on first use.
        pub fn new(model: &str, cache_dir: PathBuf, timeout: Duration) -> kiwi_core::Result<Self> {
            Ok(Self {
                model: Arc::new(OnceLock::new()),
                in_flight: Arc::new(AtomicUsize::new(0)),
                kind: model_kind(model)?,
                cache_dir,
                timeout,
                name: model.to_owned(),
            })
        }

        /// Loads the model now (startup or benchmarks), rather than on the first query.
        pub fn warm(&self) -> kiwi_core::Result<()> {
            load(&self.model, &self.kind, &self.cache_dir).map(|_| ())
        }
    }

    fn load<'a>(
        cell: &'a OnceLock<Mutex<TextRerank>>,
        kind: &RerankerModel,
        cache_dir: &PathBuf,
    ) -> kiwi_core::Result<&'a Mutex<TextRerank>> {
        if let Some(model) = cell.get() {
            return Ok(model);
        }
        std::fs::create_dir_all(cache_dir).map_err(err)?;
        // On the CPU only (ONNX Runtime's default on macOS hands parts of the graph to
        // CoreML, which compiles again for every new input shape), and at most
        // MAX_TOKENS tokens of query and passage: the head of an atom (its title,
        // abstract and first paragraphs) is what decides its rank.
        let options = RerankInitOptions::new(kind.clone())
            .with_cache_dir(cache_dir.clone())
            .with_auto_device(AutoDevicePolicy::PreferCPU)
            .with_max_length(MAX_TOKENS)
            .with_show_download_progress(false);
        let loaded = TextRerank::try_new(options).map_err(err)?;
        Ok(cell.get_or_init(|| Mutex::new(loaded)))
    }

    impl Reranker for LocalReranker {
        fn name(&self) -> &str {
            NAME
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                content: true,
                labels: false,
                network: Vec::new(),
            }
        }

        fn score(&self, query: &str, passages: &[String]) -> kiwi_core::Result<Vec<f32>> {
            // The model runs on its own thread so the caller can stop waiting at the
            // timeout; a late answer is dropped. The model serves one rerank at a time,
            // so under load the work is bounded: past MAX_IN_FLIGHT the search does not
            // wait and keeps its fused order.
            if self.in_flight.fetch_add(1, Ordering::SeqCst) >= MAX_IN_FLIGHT {
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                return Err(err("the local model is busy"));
            }
            let in_flight = Arc::clone(&self.in_flight);
            let (tx, rx) = mpsc::channel();
            let cell = Arc::clone(&self.model);
            let kind = self.kind.clone();
            let cache_dir = self.cache_dir.clone();
            let query = query.to_owned();
            let passages = passages.to_vec();
            std::thread::Builder::new()
                .name("kiwi-rerank".into())
                .spawn(move || {
                    let _done = Release(in_flight);
                    let outcome = load(&cell, &kind, &cache_dir).and_then(|model| {
                        let mut model = model.lock().map_err(|_| err("model lock poisoned"))?;
                        let refs: Vec<&str> = passages.iter().map(String::as_str).collect();
                        // One batch: every passage of this search in one model call.
                        let batch = refs.len().max(1);
                        let ranked = model
                            .rerank(query.as_str(), refs, false, Some(batch))
                            .map_err(err)?;
                        let mut scores = vec![f32::NAN; passages.len()];
                        for item in ranked {
                            if let Some(slot) = scores.get_mut(item.index) {
                                *slot = item.score;
                            }
                        }
                        Ok(scores)
                    });
                    let _ = tx.send(outcome);
                })
                .map_err(|e| {
                    self.in_flight.fetch_sub(1, Ordering::SeqCst);
                    err(e)
                })?;
            match rx.recv_timeout(self.timeout) {
                Ok(outcome) => outcome,
                Err(_) => Err(err(format!("no answer within {} ms", self.timeout.as_millis()))),
            }
        }
    }
}

#[cfg(feature = "fastembed")]
pub use local::{LocalReranker, model_kind};

/// The rerank configuration, as `kiwi.toml` gives it.
#[derive(Debug, Clone)]
pub enum Backend {
    /// A local ONNX model by name.
    Local {
        /// The model name.
        model: String,
    },
    /// An HTTP server.
    Http(HttpSettings),
}

/// Builds the configured reranker. Local models are cached under `cache_dir`.
pub fn reranker(
    backend: &Backend,
    timeout: Duration,
    cache_dir: std::path::PathBuf,
) -> kiwi_core::Result<Arc<dyn Reranker>> {
    match backend {
        Backend::Http(settings) => Ok(Arc::new(HttpReranker::new(settings)?)),
        #[cfg(feature = "fastembed")]
        Backend::Local { model } => Ok(Arc::new(LocalReranker::new(model, cache_dir, timeout)?)),
        #[cfg(not(feature = "fastembed"))]
        Backend::Local { model } => {
            let _ = (timeout, cache_dir);
            Err(err(format!(
                "the local backend (model `{model}`) needs a kiwi built with the `fastembed` feature; use backend = \"http\""
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    use super::*;

    fn serve(answer: &'static str, delay: Duration) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream);
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap();
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                std::thread::sleep(delay);
                let mut stream = reader.into_inner();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{answer}",
                    answer.len()
                );
            }
        });
        format!("http://{addr}/rerank")
    }

    fn settings(url: String, format: ApiFormat, timeout: Duration) -> HttpSettings {
        HttpSettings {
            url,
            format,
            model: "m".into(),
            api_key_env: None,
            timeout,
        }
    }

    #[test]
    fn reads_both_answer_shapes() {
        let passages = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let tei = serve(
            r#"[{"index":2,"score":0.9},{"index":0,"score":0.5},{"index":1,"score":0.1}]"#,
            Duration::ZERO,
        );
        let reranker = HttpReranker::new(&settings(tei, ApiFormat::Tei, Duration::from_secs(5))).unwrap();
        assert_eq!(reranker.score("q", &passages).unwrap(), vec![0.5, 0.1, 0.9]);
        let cohere = serve(
            r#"{"results":[{"index":1,"relevance_score":0.7},{"index":0,"relevance_score":0.2},{"index":2,"relevance_score":0.3}]}"#,
            Duration::ZERO,
        );
        let reranker =
            HttpReranker::new(&settings(cohere, ApiFormat::Cohere, Duration::from_secs(5))).unwrap();
        assert_eq!(reranker.score("q", &passages).unwrap(), vec![0.2, 0.7, 0.3]);
        assert_eq!(reranker.capabilities().network, vec!["127.0.0.1".to_owned()]);
    }

    #[test]
    fn incomplete_or_slow_answers_fail() {
        let passages = vec!["a".to_owned(), "b".to_owned()];
        let partial = serve(r#"[{"index":0,"score":0.9}]"#, Duration::ZERO);
        let reranker = HttpReranker::new(&settings(partial, ApiFormat::Tei, Duration::from_secs(5))).unwrap();
        assert!(reranker.score("q", &passages).is_err());
        let slow = serve(r#"[]"#, Duration::from_millis(800));
        let reranker =
            HttpReranker::new(&settings(slow, ApiFormat::Tei, Duration::from_millis(200))).unwrap();
        let started = std::time::Instant::now();
        assert!(reranker.score("q", &passages).is_err());
        assert!(started.elapsed() < Duration::from_millis(700));
    }

    #[test]
    fn refuses_bad_settings() {
        let remote = settings(
            "http://rerank.example.com/rerank".into(),
            ApiFormat::Tei,
            Duration::from_secs(1),
        );
        assert!(HttpReranker::new(&remote).is_err());
        let mut keyed = settings(
            "https://rerank.example.com/rerank".into(),
            ApiFormat::Tei,
            Duration::from_secs(1),
        );
        keyed.api_key_env = Some("KIWI_TEST_RERANK_KEY_THAT_IS_NOT_SET".into());
        let message = HttpReranker::new(&keyed).unwrap_err().to_string();
        assert!(
            message.contains("KIWI_TEST_RERANK_KEY_THAT_IS_NOT_SET"),
            "{message}"
        );
        assert!(ApiFormat::parse("other").is_err());
    }
}
