//! The compiled-in plugins, chosen by cargo features and the corpus configuration.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use kiwi_compiler::KiwiConfig;
use kiwi_core::plugin::{Indexer, Reranker, RetrieverFactory};

/// The plugins of one run.
#[derive(Default)]
pub struct Plugins {
    /// Build-time indexers.
    pub indexers: Vec<Box<dyn Indexer>>,
    /// Query-time retrievers.
    pub factories: Vec<Arc<dyn RetrieverFactory>>,
    /// The reranker and how many results from the top it reorders.
    pub reranker: Option<(Arc<dyn Reranker>, usize)>,
}

impl Plugins {
    /// Indexers that send atom text over the network.
    pub fn network_indexers(&self) -> Vec<String> {
        self.indexers
            .iter()
            .filter(|i| !i.capabilities().network.is_empty())
            .map(|i| i.name().to_owned())
            .collect()
    }

    /// The query-time plugins, for the MCP service.
    pub fn corpus_plugins(&self) -> kiwi_mcp::CorpusPlugins {
        kiwi_mcp::CorpusPlugins {
            factories: self.factories.clone(),
            reranker: self.reranker.clone(),
        }
    }
}

/// Overrides from the command line.
#[derive(Debug, Clone, Default)]
pub struct PluginOverrides {
    /// Replace the configured embedder (`hash`, `fastembed` or `openai`).
    #[cfg_attr(not(feature = "vec"), allow(dead_code))]
    pub embedder: Option<String>,
    /// Build without the BM25 segment.
    #[cfg_attr(not(feature = "bm25"), allow(dead_code))]
    pub no_bm25: bool,
    /// Build without the vector segment.
    pub no_vec: bool,
}

/// Where downloaded models are cached: `$KIWI_CACHE`, else `~/.cache/kiwi`.
#[cfg_attr(not(any(feature = "vec", feature = "rerank")), allow(dead_code))]
pub fn cache_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("KIWI_CACHE") {
        return PathBuf::from(dir);
    }
    match std::env::var("HOME") {
        Ok(home) => PathBuf::from(home).join(".cache").join("kiwi"),
        Err(_) => std::env::temp_dir().join("kiwi-cache"),
    }
}

/// The embedder the configuration (or an override) names.
#[cfg(feature = "vec")]
pub fn embedder_config(
    vec: &kiwi_compiler::config::VecConfig,
    kind: Option<&str>,
) -> Result<kiwi_vec::EmbedderConfig> {
    use kiwi_vec::EmbedderConfig;
    let kind = kind.unwrap_or(&vec.embedder);
    Ok(match kind {
        "hash" => EmbedderConfig::Hash,
        "fastembed" => EmbedderConfig::FastEmbed {
            model: if vec.embedder == "fastembed" {
                vec.model().to_owned()
            } else {
                kiwi_compiler::config::DEFAULT_EMBED_MODEL.to_owned()
            },
        },
        #[cfg(feature = "openai")]
        "openai" => {
            if vec.embedder != "openai" {
                anyhow::bail!(
                    "--embedder openai needs [plugins.vec] embedder = \"openai\" with its url and model"
                );
            }
            EmbedderConfig::OpenAi(kiwi_vec::OpenAiSettings {
                url: vec.url.clone().unwrap_or_default(),
                model: vec.model().to_owned(),
                dimensions: vec.dimensions,
                api_key_env: vec.api_key_env.clone(),
                batch_size: vec.batch_size as usize,
                timeout: std::time::Duration::from_millis(vec.timeout_ms),
                retries: vec.retries,
                query_prefix: vec.query_prefix.clone(),
                passage_prefix: vec.passage_prefix.clone(),
            })
        }
        #[cfg(not(feature = "openai"))]
        "openai" => anyhow::bail!("this kiwi was built without the `openai` feature"),
        other => anyhow::bail!("embedder `{other}` is not fastembed, openai or hash"),
    })
}

/// Instantiates the plugins a build runs (and the retrievers, for measuring the result),
/// as the binary's features and the configuration ask.
pub fn load(config: &KiwiConfig, overrides: &PluginOverrides) -> Result<Plugins> {
    load_with(config, overrides, true)
}

/// Instantiates the query-time plugins only: retrievers and the reranker. Serving needs
/// no indexer, so an embedder that this binary cannot build with does not stop it.
pub fn load_for_queries(config: &KiwiConfig) -> Result<Plugins> {
    load_with(config, &PluginOverrides::default(), false)
}

/// The retrievers the configuration asks for (no model is loaded until a query needs
/// one), for serving and for a reload of a watched corpus.
pub fn query_factories(config: &KiwiConfig) -> Vec<Arc<dyn RetrieverFactory>> {
    #[allow(unused_mut)]
    let mut factories: Vec<Arc<dyn RetrieverFactory>> = Vec::new();
    #[cfg(feature = "bm25")]
    {
        use kiwi_compiler::config::StatsMode;
        factories.push(Arc::new(kiwi_bm25::Bm25Factory::new(
            config.governance.bm25_stats == StatsMode::Strict,
        )));
    }
    #[cfg(feature = "vec")]
    {
        let mut factory = kiwi_vec::VecFactory::new(cache_dir().join("models"));
        // The configured embedder serves queries on segments built in its space (an
        // `openai` segment can only be queried through its configuration).
        if let Ok(configured) = embedder_config(&config.plugins.vec, None) {
            factory = factory.with_configured(configured);
        }
        factories.push(Arc::new(factory));
    }
    #[cfg(not(any(feature = "bm25", feature = "vec")))]
    let _ = config;
    factories
}

fn load_with(config: &KiwiConfig, overrides: &PluginOverrides, build: bool) -> Result<Plugins> {
    #[allow(unused_mut)]
    let mut plugins = Plugins {
        factories: query_factories(config),
        ..Plugins::default()
    };
    #[cfg(feature = "bm25")]
    if build && config.plugins.bm25.enabled && !overrides.no_bm25 {
        plugins.indexers.push(Box::new(kiwi_bm25::Bm25Indexer));
    }
    #[cfg(feature = "vec")]
    {
        let models = cache_dir().join("models");
        if build && config.plugins.vec.enabled && !overrides.no_vec {
            let chosen = embedder_config(&config.plugins.vec, overrides.embedder.as_deref())?;
            let embedder = kiwi_vec::embedder_from_config(&chosen, models)?;
            plugins
                .indexers
                .push(Box::new(kiwi_vec::VecIndexer::new(embedder)));
        }
    }
    // A plugin the configuration enables but this binary leaves out is skipped, loudly:
    // the pack is built without its segment and searches without that tier.
    #[cfg(not(feature = "bm25"))]
    if build && config.plugins.bm25.enabled && !overrides.no_bm25 {
        eprintln!(
            "kiwi: plugins.bm25 is enabled but this kiwi was built without `bm25`; building without it"
        );
    }
    #[cfg(not(feature = "vec"))]
    if build && config.plugins.vec.enabled && !overrides.no_vec {
        eprintln!("kiwi: plugins.vec is enabled but this kiwi was built without `vec`; building without it");
    }
    let rerank = &config.plugins.rerank;
    if !build && rerank.enabled {
        plugins.reranker = Some((load_reranker(rerank)?, rerank.top_k() as usize));
    }
    Ok(plugins)
}

#[cfg(feature = "rerank")]
fn load_reranker(rerank: &kiwi_compiler::config::RerankConfig) -> Result<Arc<dyn Reranker>> {
    let timeout = std::time::Duration::from_millis(rerank.timeout_ms);
    let backend = match rerank.backend.as_str() {
        "http" => kiwi_rerank::Backend::Http(kiwi_rerank::HttpSettings {
            url: rerank.url.clone().unwrap_or_default(),
            format: kiwi_rerank::ApiFormat::parse(&rerank.format)?,
            model: rerank.model().to_owned(),
            api_key_env: rerank.api_key_env.clone(),
            timeout,
        }),
        _ => kiwi_rerank::Backend::Local {
            model: rerank.model().to_owned(),
        },
    };
    Ok(kiwi_rerank::reranker(
        &backend,
        timeout,
        cache_dir().join("models"),
    )?)
}

#[cfg(not(feature = "rerank"))]
fn load_reranker(_rerank: &kiwi_compiler::config::RerankConfig) -> Result<Arc<dyn Reranker>> {
    anyhow::bail!("plugins.rerank is enabled but this kiwi was built without the `rerank` feature")
}

/// Names of the compiled-in plugins and backends.
pub fn compiled_in() -> Vec<&'static str> {
    [
        (cfg!(feature = "bm25"), "bm25"),
        (cfg!(feature = "vec"), "vec"),
        (cfg!(feature = "openai"), "openai"),
        (cfg!(feature = "fastembed"), "fastembed"),
        (cfg!(feature = "rerank"), "rerank"),
        (cfg!(feature = "wasm"), "wasm"),
        (cfg!(feature = "http"), "http"),
    ]
    .into_iter()
    .filter_map(|(on, name)| on.then_some(name))
    .collect()
}

/// Loads the WebAssembly components the configuration lists (paths relative to the
/// corpus root), each under the name the configuration gives it and governed by
/// `gate` as `plugin:<name>`.
#[cfg(feature = "wasm")]
pub fn load_wasm(
    config: &KiwiConfig,
    corpus: &std::path::Path,
    gate: Option<kiwi_wasm::Gate>,
) -> Result<Vec<kiwi_wasm::WasmPlugin>> {
    let mut out = Vec::new();
    for component in &config.plugins.wasm.components {
        out.push(kiwi_wasm::WasmPlugin::load(
            &component.name,
            &corpus.join(&component.path),
            gate.clone(),
        )?);
    }
    Ok(out)
}
