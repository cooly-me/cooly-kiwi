//! The corpus configuration, `.kiwi/kiwi.toml`. Every key is optional.
//!
//! ```toml
//! namespace = "acme"
//!
//! [atoms]
//! min_tokens = 50
//! max_tokens = 800
//!
//! [governance]
//! hidden = "invisible"      # or "stub"
//! bm25_stats = "strict"     # or "global"
//!
//! [search]
//! budget = 800
//!
//! [plugins.bm25]
//! enabled = true
//!
//! [plugins.vec]
//! enabled = true
//! embedder = "fastembed"    # or "openai", "hash"
//!
//! [plugins.rerank]
//! enabled = true
//! backend = "http"
//! url = "http://127.0.0.1:8080/rerank"
//!
//! [[audit.sinks]]
//! kind = "stderr"           # or "stdout" (not with stdio serving), "http", "syslog"
//! ```

use kiwi_core::HiddenMode;
use kiwi_policy::sinks::SinkConfig;
use serde::{Deserialize, Serialize};

use crate::error::{CompileError, Result};

/// The path of the configuration file inside the corpus.
pub const CONFIG_PATH: &str = ".kiwi/kiwi.toml";

/// Atom sizing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AtomConfig {
    /// Sections below this many tokens merge into their parent (same access only).
    pub min_tokens: u32,
    /// Sections above this many tokens split at paragraph boundaries.
    pub max_tokens: u32,
}

impl Default for AtomConfig {
    fn default() -> Self {
        Self {
            min_tokens: 50,
            max_tokens: 800,
        }
    }
}

/// How BM25 statistics are scoped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StatsMode {
    /// Statistics over the caller's visible atoms only: no hidden term can shift a score.
    #[default]
    Strict,
    /// Statistics over the whole corpus (faster, but rare terms in hidden sections can
    /// shift scores).
    Global,
}

/// Governance settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GovernanceConfig {
    /// How hidden atoms are shown.
    pub hidden: HiddenMode,
    /// BM25 statistics scope.
    pub bm25_stats: StatsMode,
    /// The Cedar policy file.
    pub policy: String,
    /// Cedar policy unit tests.
    pub policy_tests: String,
    /// The principal directory.
    pub principals: String,
}

impl Default for GovernanceConfig {
    fn default() -> Self {
        Self {
            hidden: HiddenMode::Invisible,
            bm25_stats: StatsMode::Strict,
            policy: ".kiwi/policy.cedar".into(),
            policy_tests: ".kiwi/policy.tests.yaml".into(),
            principals: ".kiwi/principals.yaml".into(),
        }
    }
}

/// Search defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchConfig {
    /// Default token budget.
    pub budget: u32,
    /// Default number of results considered before the budget cut.
    pub limit: u32,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            budget: 800,
            limit: 10,
        }
    }
}

/// The lexical plugin settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Bm25Config {
    /// Build and use the BM25 segment.
    pub enabled: bool,
}

impl Default for Bm25Config {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// The semantic plugin settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VecConfig {
    /// Build and use the vector segment.
    pub enabled: bool,
    /// `fastembed` (a local ONNX model), `openai` (an OpenAI-compatible embeddings API)
    /// or `hash` (feature hashing, no model).
    pub embedder: String,
    /// The model: a fastembed model name (default `multilingual-e5-small`), or the API's
    /// model name (required with `openai`).
    pub model: Option<String>,
    /// `openai`: the API's base URL, e.g. `https://api.openai.com/v1` (https, or http on
    /// a loopback address).
    pub url: Option<String>,
    /// `openai`: the environment variable holding the API key (never the key itself).
    pub api_key_env: Option<String>,
    /// `openai`: dimensions to ask the model for (vectors are cut to 384 either way).
    pub dimensions: Option<u32>,
    /// `openai`: texts per request.
    pub batch_size: u32,
    /// `openai`: limit for one request, in milliseconds.
    pub timeout_ms: u64,
    /// `openai`: retries of a failed request.
    pub retries: u32,
    /// `openai`: prefix added to queries (`query: ` for E5 models).
    pub query_prefix: String,
    /// `openai`: prefix added to passages (`passage: ` for E5 models).
    pub passage_prefix: String,
}

/// The default local embedding model.
pub const DEFAULT_EMBED_MODEL: &str = "multilingual-e5-small";
/// The local embedding models.
pub const LOCAL_EMBED_MODELS: [&str; 3] = ["multilingual-e5-small", "bge-small-en-v1.5", "all-minilm-l6-v2"];

impl Default for VecConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            embedder: "fastembed".into(),
            model: None,
            url: None,
            api_key_env: None,
            dimensions: None,
            batch_size: 64,
            timeout_ms: 30_000,
            retries: 4,
            query_prefix: String::new(),
            passage_prefix: String::new(),
        }
    }
}

impl VecConfig {
    /// The model in effect.
    pub fn model(&self) -> &str {
        match (&self.model, self.embedder.as_str()) {
            (Some(model), _) => model,
            (None, "fastembed") => DEFAULT_EMBED_MODEL,
            (None, _) => "",
        }
    }

    fn validate(&self) -> Result<()> {
        let bad = |message: String| Err(CompileError::Config(format!("plugins.vec: {message}")));
        let http_only = self.url.is_some()
            || self.api_key_env.is_some()
            || self.dimensions.is_some()
            || !self.query_prefix.is_empty()
            || !self.passage_prefix.is_empty();
        match self.embedder.as_str() {
            "hash" => {
                if http_only || self.model.is_some() {
                    return bad("the hash embedder takes no model, url or key".into());
                }
            }
            "fastembed" => {
                if http_only {
                    return bad(
                        "url, api_key_env, dimensions and prefixes apply to embedder = \"openai\"".into(),
                    );
                }
                if !LOCAL_EMBED_MODELS.contains(&self.model()) {
                    return bad(format!(
                        "model `{}` is not a local model ({})",
                        self.model(),
                        LOCAL_EMBED_MODELS.join(", ")
                    ));
                }
            }
            "openai" => {
                if self.url.as_deref().is_none_or(|u| u.trim().is_empty()) {
                    return bad("embedder = \"openai\" needs url (the API's base URL)".into());
                }
                if self.model.as_deref().is_none_or(|m| m.trim().is_empty()) {
                    return bad("embedder = \"openai\" needs model".into());
                }
                if !(1..=2048).contains(&self.batch_size) {
                    return bad("batch_size must be between 1 and 2048".into());
                }
                if !(100..=600_000).contains(&self.timeout_ms) {
                    return bad("timeout_ms must be between 100 and 600000".into());
                }
                if self.retries > 10 {
                    return bad("retries must be at most 10".into());
                }
                if self.dimensions.is_some_and(|d| !(1..=16_384).contains(&d)) {
                    return bad("dimensions must be between 1 and 16384".into());
                }
                if let Some(var) = &self.api_key_env {
                    check_env_name("plugins.vec.api_key_env", var)?;
                }
            }
            other => return bad(format!("embedder `{other}` is not fastembed, openai or hash")),
        }
        Ok(())
    }
}

/// The rerank plugin settings (off by default).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RerankConfig {
    /// Rerank the top of every search.
    pub enabled: bool,
    /// `local` (an ONNX cross-encoder; a kiwi built with `fastembed`) or `http`.
    pub backend: String,
    /// `local`: the model; `http`: the model name sent with the `cohere` format.
    pub model: Option<String>,
    /// `http`: the endpoint, e.g. `http://127.0.0.1:8080/rerank`.
    pub url: Option<String>,
    /// `http`: `tei` (Text Embeddings Inference) or `cohere` (Cohere, Jina, Voyage,
    /// vLLM, Infinity).
    pub format: String,
    /// `http`: the environment variable holding a bearer token, if needed.
    pub api_key_env: Option<String>,
    /// How many results from the top are reranked: by default 10 with the local backend
    /// (a CPU cross-encoder costs about 0.1 s per passage) and 20 over HTTP.
    pub top_k: Option<u32>,
    /// Limit for one rerank, in milliseconds; past it the fused order stands.
    pub timeout_ms: u64,
}

/// Results reranked by default with the local backend.
pub const DEFAULT_LOCAL_TOP_K: u32 = 10;
/// Results reranked by default over HTTP.
pub const DEFAULT_HTTP_TOP_K: u32 = 20;

/// The default local rerank model.
pub const DEFAULT_RERANK_MODEL: &str = "jina-reranker-v2-base-multilingual";
/// The local rerank models.
pub const LOCAL_RERANK_MODELS: [&str; 4] = [
    "bge-reranker-base",
    "bge-reranker-v2-m3",
    "jina-reranker-v1-turbo-en",
    "jina-reranker-v2-base-multilingual",
];

impl Default for RerankConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: "local".into(),
            model: None,
            url: None,
            format: "cohere".into(),
            api_key_env: None,
            top_k: None,
            timeout_ms: 2_000,
        }
    }
}

impl RerankConfig {
    /// The model in effect.
    pub fn model(&self) -> &str {
        match (&self.model, self.backend.as_str()) {
            (Some(model), _) => model,
            (None, "local") => DEFAULT_RERANK_MODEL,
            (None, _) => "",
        }
    }

    /// How many results from the top are reranked.
    pub fn top_k(&self) -> u32 {
        self.top_k.unwrap_or(if self.backend == "local" {
            DEFAULT_LOCAL_TOP_K
        } else {
            DEFAULT_HTTP_TOP_K
        })
    }

    fn validate(&self) -> Result<()> {
        let bad = |message: String| Err(CompileError::Config(format!("plugins.rerank: {message}")));
        if !(2..=50).contains(&self.top_k()) {
            return bad("top_k must be between 2 and 50".into());
        }
        if !(50..=60_000).contains(&self.timeout_ms) {
            return bad("timeout_ms must be between 50 and 60000".into());
        }
        match self.backend.as_str() {
            "local" => {
                if self.url.is_some() || self.api_key_env.is_some() {
                    return bad("url and api_key_env apply to backend = \"http\"".into());
                }
                if !LOCAL_RERANK_MODELS.contains(&self.model()) {
                    return bad(format!(
                        "model `{}` is not a local model ({})",
                        self.model(),
                        LOCAL_RERANK_MODELS.join(", ")
                    ));
                }
            }
            "http" => {
                if self.url.as_deref().is_none_or(|u| u.trim().is_empty()) {
                    return bad("backend = \"http\" needs url".into());
                }
                if !matches!(self.format.as_str(), "tei" | "cohere") {
                    return bad(format!("format `{}` is not tei or cohere", self.format));
                }
                if let Some(var) = &self.api_key_env {
                    check_env_name("plugins.rerank.api_key_env", var)?;
                }
            }
            other => return bad(format!("backend `{other}` is not local or http")),
        }
        Ok(())
    }
}

/// Audit settings: where records go besides the local hash-chained log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct AuditConfig {
    /// Sinks, each fed every record through its own bounded queue.
    pub sinks: Vec<SinkConfig>,
}

fn check_env_name(key: &str, name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit());
    if valid {
        Ok(())
    } else {
        Err(CompileError::Config(format!(
            "{key} must name an environment variable (A-Z, 0-9, _), not hold a secret"
        )))
    }
}

/// One WebAssembly plugin: its name and its component file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WasmComponent {
    /// The plugin's name: the policy decides what it reads as `plugin:<name>`. It is
    /// set here, never taken from the component, so a plugin cannot choose the
    /// principal it is governed as.
    pub name: String,
    /// The component file, relative to the corpus root.
    pub path: String,
}

/// Third-party WebAssembly plugins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct WasmConfig {
    /// The components, `{ name = "…", path = "…" }` each.
    pub components: Vec<WasmComponent>,
}

/// Plugin names the compiled-in plugins use; a WebAssembly plugin may not take one.
const BUILTIN_PLUGINS: [&str; 7] = ["bm25", "vec", "openai", "fastembed", "hash", "rerank", "http"];

impl WasmConfig {
    fn validate(&self) -> Result<()> {
        let mut seen = std::collections::BTreeSet::new();
        for component in &self.components {
            let name = component.name.as_str();
            let valid = (1..=64).contains(&name.len())
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if !valid {
                return Err(CompileError::Config(format!(
                    "plugins.wasm: name `{name}` must be 1-64 letters, digits, - or _"
                )));
            }
            if BUILTIN_PLUGINS.contains(&name) || !seen.insert(name) {
                return Err(CompileError::Config(format!(
                    "plugins.wasm: name `{name}` is taken"
                )));
            }
            if component.path.trim().is_empty() {
                return Err(CompileError::Config(format!(
                    "plugins.wasm: `{name}` needs a path"
                )));
            }
        }
        Ok(())
    }
}

/// How many packs the store keeps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PacksConfig {
    /// Packs kept after each build, the current one included (pinned citations of
    /// older snapshots stop resolving once their pack goes).
    pub keep: u32,
}

impl Default for PacksConfig {
    fn default() -> Self {
        Self {
            keep: kiwi_core::DEFAULT_KEEP_PACKS as u32,
        }
    }
}

/// Plugin settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct PluginsConfig {
    /// BM25.
    pub bm25: Bm25Config,
    /// Vectors.
    pub vec: VecConfig,
    /// Rerank.
    pub rerank: RerankConfig,
    /// WebAssembly components.
    pub wasm: WasmConfig,
}

/// Where `kiwi_propose` sends a branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ForgeConfig {
    /// `local` (a branch and a proposal record in the repository) or `github` (push the
    /// branch and open a pull request with the `gh` CLI).
    pub kind: String,
    /// The remote to push to with `github`.
    pub remote: String,
    /// The base branch of proposals.
    pub base: String,
}

impl Default for ForgeConfig {
    fn default() -> Self {
        Self {
            kind: "local".into(),
            remote: "origin".into(),
            base: "main".into(),
        }
    }
}

/// Checks a namespace: lowercase letters, digits, `-` and `_`, starting with a letter or
/// digit, at most 63 characters; `code` is reserved (it prefixes code paths in traces).
pub fn check_namespace(namespace: &str) -> Result<()> {
    let valid = (1..=63).contains(&namespace.len())
        && namespace.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && namespace
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        && namespace != "code";
    if valid {
        Ok(())
    } else {
        Err(CompileError::Config(format!(
            "namespace `{namespace}` must be 1-63 lowercase letters, digits, - or _ (and not `code`)"
        )))
    }
}

/// The whole configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KiwiConfig {
    /// The corpus namespace; ids carry it when a server federates several corpora.
    pub namespace: String,
    /// Atom sizing.
    pub atoms: AtomConfig,
    /// Governance.
    pub governance: GovernanceConfig,
    /// Search defaults.
    pub search: SearchConfig,
    /// Plugins.
    pub plugins: PluginsConfig,
    /// Proposal forge.
    pub forge: ForgeConfig,
    /// Audit sinks.
    pub audit: AuditConfig,
    /// Pack retention.
    pub packs: PacksConfig,
}

impl Default for KiwiConfig {
    fn default() -> Self {
        Self {
            namespace: "default".into(),
            atoms: AtomConfig::default(),
            governance: GovernanceConfig::default(),
            search: SearchConfig::default(),
            plugins: PluginsConfig::default(),
            forge: ForgeConfig::default(),
            audit: AuditConfig::default(),
            packs: PacksConfig::default(),
        }
    }
}

impl KiwiConfig {
    /// Parses `kiwi.toml` content.
    pub fn parse(source: &str) -> Result<Self> {
        let config: Self = toml::from_str(source).map_err(|e| CompileError::Config(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        check_namespace(&self.namespace)?;
        self.plugins.vec.validate()?;
        self.plugins.rerank.validate()?;
        self.plugins.wasm.validate()?;
        if !(1..=10_000).contains(&self.packs.keep) {
            return Err(CompileError::Config(
                "packs.keep must be between 1 and 10000".into(),
            ));
        }
        for (index, sink) in self.audit.sinks.iter().enumerate() {
            sink.validate()
                .map_err(|e| CompileError::Config(format!("audit.sinks[{index}]: {e}")))?;
        }
        if self.atoms.min_tokens >= self.atoms.max_tokens {
            return Err(CompileError::Config(
                "atoms.min_tokens must be below atoms.max_tokens".into(),
            ));
        }
        if self.atoms.max_tokens < 64 {
            return Err(CompileError::Config(
                "atoms.max_tokens must be at least 64".into(),
            ));
        }
        if !matches!(self.forge.kind.as_str(), "local" | "github") {
            return Err(CompileError::Config(format!(
                "forge.kind `{}` is not local or github",
                self.forge.kind
            )));
        }
        Ok(())
    }

    /// BLAKE3 of the settings that change compiled output, so a change invalidates the
    /// incremental cache.
    pub fn build_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(env!("CARGO_PKG_VERSION").as_bytes());
        hasher.update(&self.atoms.min_tokens.to_le_bytes());
        hasher.update(&self.atoms.max_tokens.to_le_bytes());
        hasher.update(self.namespace.as_bytes());
        hasher.update(&[
            u8::from(self.plugins.bm25.enabled),
            u8::from(self.plugins.vec.enabled),
        ]);
        hasher.update(self.plugins.vec.embedder.as_bytes());
        hasher.update(self.plugins.vec.model().as_bytes());
        for component in &self.plugins.wasm.components {
            hasher.update(component.name.as_bytes());
            hasher.update(b"=");
            hasher.update(component.path.as_bytes());
            hasher.update(b"\n");
        }
        *hasher.finalize().as_bytes()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn defaults_and_overrides() {
        let config = KiwiConfig::parse("namespace = \"acme\"\n[governance]\nhidden = \"stub\"\n").unwrap();
        assert_eq!(config.namespace, "acme");
        assert_eq!(config.governance.hidden, HiddenMode::Stub);
        assert_eq!(config.atoms.max_tokens, 800);
        assert!(KiwiConfig::parse("[atoms]\nmin_tokens = 900\n").is_err());
        assert!(KiwiConfig::parse("unknown = 1\n").is_err());
    }

    #[test]
    fn plugin_settings_are_validated() {
        let openai = KiwiConfig::parse(
            "[plugins.vec]\nenabled = true\nembedder = \"openai\"\nmodel = \"text-embedding-3-small\"\nurl = \"https://api.openai.com/v1\"\napi_key_env = \"OPENAI_API_KEY\"\ndimensions = 384\n",
        )
        .unwrap();
        assert_eq!(openai.plugins.vec.model(), "text-embedding-3-small");
        for bad in [
            "[plugins.vec]\nembedder = \"openai\"\nmodel = \"m\"\n",
            "[plugins.vec]\nembedder = \"openai\"\nurl = \"https://x\"\n",
            "[plugins.vec]\nembedder = \"openai\"\nmodel = \"m\"\nurl = \"https://x\"\napi_key_env = \"sk-live-abc\"\n",
            "[plugins.vec]\nembedder = \"openai\"\nmodel = \"m\"\nurl = \"https://x\"\napi_key = \"sk\"\n",
            "[plugins.vec]\nembedder = \"fastembed\"\nurl = \"https://x\"\n",
            "[plugins.vec]\nembedder = \"fastembed\"\nmodel = \"nope\"\n",
            "[plugins.vec]\nembedder = \"other\"\n",
            "[plugins.rerank]\nenabled = true\nbackend = \"http\"\n",
            "[plugins.rerank]\nbackend = \"http\"\nurl = \"http://127.0.0.1:1/r\"\nformat = \"x\"\n",
            "[plugins.rerank]\ntop_k = 1\n",
            "[plugins.rerank]\nmodel = \"nope\"\n",
            "[[audit.sinks]]\nkind = \"http\"\n",
            "[[audit.sinks]]\nkind = \"carrier-pigeon\"\n",
            "namespace = \"Acme\"\n",
            "namespace = \"code\"\n",
            "[plugins.wasm]\ncomponents = [\"plugins/x.wasm\"]\n",
            "[plugins.wasm]\ncomponents = [{ name = \"bm25\", path = \"x.wasm\" }]\n",
            "[plugins.wasm]\ncomponents = [{ name = \"a b\", path = \"x.wasm\" }]\n",
            "[plugins.wasm]\ncomponents = [{ name = \"x\", path = \"a.wasm\" }, { name = \"x\", path = \"b.wasm\" }]\n",
            "[packs]\nkeep = 0\n",
        ] {
            assert!(KiwiConfig::parse(bad).is_err(), "{bad}");
        }
        let wasm = KiwiConfig::parse(
            "[plugins.wasm]\ncomponents = [{ name = \"example-lint\", path = \"plugins/example_lint.wasm\" }]\n[packs]\nkeep = 3\n",
        )
        .unwrap();
        assert_eq!(wasm.plugins.wasm.components[0].name, "example-lint");
        assert_eq!(wasm.packs.keep, 3);
        let rerank = KiwiConfig::parse("[plugins.rerank]\nenabled = true\n").unwrap();
        assert_eq!(rerank.plugins.rerank.model(), DEFAULT_RERANK_MODEL);
        assert_eq!(rerank.plugins.rerank.top_k(), DEFAULT_LOCAL_TOP_K);
        let http = KiwiConfig::parse(
            "[plugins.rerank]\nbackend = \"http\"\nurl = \"https://r.example.com/rerank\"\n",
        )
        .unwrap();
        assert_eq!(http.plugins.rerank.top_k(), DEFAULT_HTTP_TOP_K);
        let sinks = KiwiConfig::parse("[[audit.sinks]]\nkind = \"stdout\"\n[[audit.sinks]]\nkind = \"http\"\nurl = \"https://siem.example.com/kiwi\"\n").unwrap();
        assert_eq!(sinks.audit.sinks.len(), 2);
    }
}
