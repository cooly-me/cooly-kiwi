//! Embedders: `fastembed` (a local ONNX model, cargo feature `fastembed`), `openai`
//! (any server with the OpenAI embeddings API, cargo feature `openai`) and a
//! feature-hashing embedder that needs no model (for tests, CI and synthetic
//! benchmarks).

use std::sync::Arc;

use kiwi_core::plugin::Capabilities;
use kiwi_core::{KiwiError, text};

use crate::DIMS;

/// Turns text into vectors. Implementations must be deterministic.
pub trait Embedder: Send + Sync {
    /// A stable name including the model; a segment built with one embedder is only
    /// queried with the same one.
    fn name(&self) -> String;
    /// What the embedder reads and reaches.
    fn capabilities(&self) -> Capabilities;
    /// Embeds document passages.
    fn embed_passages(&self, texts: &[String]) -> kiwi_core::Result<Vec<Vec<f32>>>;
    /// Embeds a query.
    fn embed_query(&self, text: &str) -> kiwi_core::Result<Vec<f32>>;

    /// Texts per call of [`Embedder::embed_passages`] the indexer sends.
    fn batch_size(&self) -> usize {
        64
    }

    /// Embeds several queries; override to batch them in one model call.
    fn embed_queries(&self, texts: &[String]) -> kiwi_core::Result<Vec<Vec<f32>>> {
        texts.iter().map(|t| self.embed_query(t)).collect()
    }
}

/// Feature hashing of folded words and word pairs into 384 signed buckets. Captures
/// lexical overlap only; it exists so the vector path can run without a model.
#[derive(Debug, Default, Clone, Copy)]
pub struct HashEmbedder;

impl HashEmbedder {
    fn embed(text_in: &str) -> Vec<f32> {
        let folded = text::fold(text_in);
        let words: Vec<String> = text::words(&folded)
            .into_iter()
            .map(|w| w.trim_end_matches('s').to_owned())
            .filter(|w| w.len() > 2)
            .collect();
        let mut vector = vec![0.0f32; DIMS];
        let mut add = |feature: &str, weight: f32| {
            let hash = blake3::hash(feature.as_bytes());
            let bytes = hash.as_bytes();
            let bucket = (u16::from_le_bytes([bytes[0], bytes[1]]) as usize) % DIMS;
            let sign = if bytes[2] & 1 == 0 { 1.0 } else { -1.0 };
            vector[bucket] += sign * weight;
        };
        for word in &words {
            add(word, 1.0);
        }
        for pair in words.windows(2) {
            add(&format!("{} {}", pair[0], pair[1]), 0.5);
        }
        vector
    }
}

impl Embedder for HashEmbedder {
    fn name(&self) -> String {
        "hash-v1".into()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            content: true,
            labels: false,
            network: Vec::new(),
        }
    }

    fn embed_passages(&self, texts: &[String]) -> kiwi_core::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| Self::embed(t)).collect())
    }

    fn embed_query(&self, text: &str) -> kiwi_core::Result<Vec<f32>> {
        Ok(Self::embed(text))
    }
}

#[cfg(feature = "fastembed")]
mod local {
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};

    use fastembed::{AutoDevicePolicy, EmbeddingModel, TextEmbedding, TextInitOptions};
    use kiwi_core::KiwiError;
    use kiwi_core::plugin::Capabilities;

    use super::Embedder;

    /// A local ONNX embedding model run by fastembed on the CPU. The model is
    /// downloaded once into the cache directory; atom text never leaves the machine.
    pub struct FastEmbedder {
        model: OnceLock<Mutex<TextEmbedding>>,
        kind: EmbeddingModel,
        cache_dir: PathBuf,
        name: String,
        prefixes: (&'static str, &'static str),
    }

    impl std::fmt::Debug for FastEmbedder {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("FastEmbedder").field("name", &self.name).finish()
        }
    }

    fn err(e: impl std::fmt::Display) -> KiwiError {
        KiwiError::plugin("vec", e)
    }

    impl FastEmbedder {
        /// A 384-dimension model: `multilingual-e5-small`, `bge-small-en-v1.5` or
        /// `all-minilm-l6-v2`. The model is loaded (and downloaded once into
        /// `cache_dir`) on first use, so opening a pack stays fast.
        pub fn new(model: &str, cache_dir: PathBuf) -> kiwi_core::Result<Self> {
            let (kind, prefixes) = match model {
                "multilingual-e5-small" => (EmbeddingModel::MultilingualE5Small, ("query: ", "passage: ")),
                "bge-small-en-v1.5" => (EmbeddingModel::BGESmallENV15, ("", "")),
                "all-minilm-l6-v2" => (EmbeddingModel::AllMiniLML6V2, ("", "")),
                other => return Err(err(format!("unsupported model `{other}`"))),
            };
            Ok(Self {
                model: OnceLock::new(),
                kind,
                cache_dir,
                name: format!("fastembed:{model}"),
                prefixes,
            })
        }

        fn model(&self) -> kiwi_core::Result<&Mutex<TextEmbedding>> {
            if let Some(model) = self.model.get() {
                return Ok(model);
            }
            std::fs::create_dir_all(&self.cache_dir).map_err(err)?;
            // On the CPU only: ONNX Runtime's default on macOS hands parts of the graph
            // to CoreML, which compiles again for every new input shape and makes each
            // call many times slower.
            let options = TextInitOptions::new(self.kind.clone())
                .with_cache_dir(self.cache_dir.clone())
                .with_auto_device(AutoDevicePolicy::PreferCPU)
                .with_show_download_progress(false);
            let loaded = TextEmbedding::try_new(options).map_err(err)?;
            Ok(self.model.get_or_init(|| Mutex::new(loaded)))
        }

        fn run(&self, texts: Vec<String>) -> kiwi_core::Result<Vec<Vec<f32>>> {
            let mut model = self.model()?.lock().map_err(|_| err("model lock poisoned"))?;
            model.embed(texts, Some(16)).map_err(err)
        }
    }

    impl Embedder for FastEmbedder {
        fn name(&self) -> String {
            self.name.clone()
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                content: true,
                labels: false,
                network: Vec::new(),
            }
        }

        fn embed_passages(&self, texts: &[String]) -> kiwi_core::Result<Vec<Vec<f32>>> {
            self.run(texts.iter().map(|t| format!("{}{t}", self.prefixes.1)).collect())
        }

        fn embed_query(&self, text: &str) -> kiwi_core::Result<Vec<f32>> {
            self.run(vec![format!("{}{text}", self.prefixes.0)])?
                .pop()
                .ok_or_else(|| err("no embedding returned"))
        }

        fn embed_queries(&self, texts: &[String]) -> kiwi_core::Result<Vec<Vec<f32>>> {
            self.run(texts.iter().map(|t| format!("{}{t}", self.prefixes.0)).collect())
        }
    }
}

#[cfg(feature = "fastembed")]
pub use local::FastEmbedder;

/// Which embedder to build, from the configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbedderConfig {
    /// Feature hashing, no model.
    Hash,
    /// A local fastembed model by name.
    FastEmbed {
        /// The model name.
        model: String,
    },
    /// An OpenAI-compatible embeddings API.
    #[cfg(feature = "openai")]
    OpenAi(crate::openai::OpenAiSettings),
}

impl EmbedderConfig {
    /// The vector space this configuration produces (the name a segment records).
    pub fn space(&self) -> String {
        match self {
            EmbedderConfig::Hash => "hash-v1".into(),
            EmbedderConfig::FastEmbed { model } => format!("fastembed:{model}"),
            #[cfg(feature = "openai")]
            EmbedderConfig::OpenAi(settings) => settings.space(),
        }
    }
}

/// Builds the configured embedder. Local models are cached under `cache_dir`.
pub fn embedder_from_config(
    config: &EmbedderConfig,
    cache_dir: std::path::PathBuf,
) -> kiwi_core::Result<Arc<dyn Embedder>> {
    match config {
        EmbedderConfig::Hash => Ok(Arc::new(HashEmbedder)),
        #[cfg(feature = "fastembed")]
        EmbedderConfig::FastEmbed { model } => Ok(Arc::new(FastEmbedder::new(model, cache_dir)?)),
        #[cfg(not(feature = "fastembed"))]
        EmbedderConfig::FastEmbed { model } => {
            let _ = cache_dir;
            Err(KiwiError::plugin(
                "vec",
                format!(
                    "the local model `{model}` needs a kiwi built with the `fastembed` feature; use embedder = \"openai\" or \"hash\""
                ),
            ))
        }
        #[cfg(feature = "openai")]
        EmbedderConfig::OpenAi(settings) => {
            Ok(Arc::new(crate::openai::OpenAiEmbedder::new(settings.clone())?))
        }
    }
}

/// The embedder for the vector space a segment header names (`hash-v1`,
/// `fastembed:<model>`, `openai:<model>:<dimensions>`). An `openai` space is served by
/// the configured client, which must produce that same space.
pub fn embedder_by_name(
    name: &str,
    configured: Option<&EmbedderConfig>,
    cache_dir: std::path::PathBuf,
) -> kiwi_core::Result<Arc<dyn Embedder>> {
    if name == "hash-v1" {
        return Ok(Arc::new(HashEmbedder));
    }
    if let Some(model) = name.strip_prefix("fastembed:") {
        return embedder_from_config(
            &EmbedderConfig::FastEmbed {
                model: model.to_owned(),
            },
            cache_dir,
        );
    }
    if name.starts_with("openai:") {
        return match configured {
            Some(config) if config.space() == name => embedder_from_config(config, cache_dir),
            Some(config) => Err(KiwiError::plugin(
                "vec",
                format!(
                    "the pack's vectors are `{name}` but kiwi.toml configures `{}`; rebuild the pack",
                    config.space()
                ),
            )),
            None => Err(KiwiError::plugin(
                "vec",
                format!(
                    "the pack's vectors are `{name}`; configure that embedder in kiwi.toml to query them"
                ),
            )),
        };
    }
    Err(KiwiError::plugin("vec", format!("unknown embedder `{name}`")))
}
