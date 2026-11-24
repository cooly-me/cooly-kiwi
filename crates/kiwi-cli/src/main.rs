//! `kiwi`: build packs, serve them over MCP, test policies, lint, benchmark and verify
//! the audit log.

mod bench;
mod identity;
mod plugins;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use kiwi_compiler::build::{BuildPlugins, PluginGate, load_config};
use kiwi_compiler::lint::BuiltinLint;
use kiwi_compiler::{BuildOptions, KiwiConfig, SourceMode, source};
use kiwi_core::plugin::{Lint, Severity};
use kiwi_core::{Pack, PackStore};
use kiwi_mcp::{Corpus, KiwiService, ServiceOptions};
use kiwi_policy::{Directory, Identity, PolicyGate, audit, policytest};

#[derive(Parser)]
#[command(
    name = "kiwi",
    version,
    about = "Kiwi: a governed knowledge engine for agents, over MCP"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Clone, Debug)]
struct SourceArgs {
    /// The corpus directory (Markdown in a Git repository, or a plain directory).
    corpus: PathBuf,
    /// Read the working files instead of the committed tree.
    #[arg(long, conflicts_with = "rev")]
    worktree: bool,
    /// Read this Git revision (default HEAD when the corpus is in a repository).
    #[arg(long)]
    rev: Option<String>,
    /// Pack directory (default <corpus>/.kiwi/packs).
    #[arg(long)]
    packs: Option<PathBuf>,
}

impl SourceArgs {
    fn mode(&self) -> SourceMode {
        match (&self.rev, self.worktree) {
            (Some(rev), _) => SourceMode::Git(rev.clone()),
            (None, true) => SourceMode::Worktree,
            (None, false) => SourceMode::Auto,
        }
    }

    fn packs(&self) -> PathBuf {
        self.packs
            .clone()
            .unwrap_or_else(|| self.corpus.join(kiwi_compiler::build::PACK_DIR))
    }
}

#[derive(Args, Clone, Debug, Default)]
struct PluginArgs {
    /// Override the configured embedder (`hash` needs no model).
    #[arg(long)]
    embedder: Option<String>,
    /// Do not build the BM25 segment.
    #[arg(long)]
    no_bm25: bool,
    /// Do not build the vector segment.
    #[arg(long)]
    no_vec: bool,
}

impl PluginArgs {
    fn overrides(&self) -> plugins::PluginOverrides {
        plugins::PluginOverrides {
            embedder: self.embedder.clone(),
            no_bm25: self.no_bm25,
            no_vec: self.no_vec,
        }
    }
}

// Parsed once per process; boxing the larger variants would buy nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum Command {
    /// Compile the corpus into a Kiwi Pack.
    Build {
        #[command(flatten)]
        source: SourceArgs,
        #[command(flatten)]
        plugins: PluginArgs,
        /// Override the snapshot date (YYYY-MM-DD) review deadlines are checked against.
        #[arg(long)]
        as_of: Option<String>,
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Serve over MCP (stdio by default) the pack of the snapshot the policy is read from.
    Serve(ServeArgs),
    /// Run one tool locally and print its JSON answer.
    Query {
        #[command(flatten)]
        source: SourceArgs,
        /// Tool name, e.g. kiwi_search.
        tool: String,
        /// Tool arguments as JSON.
        #[arg(default_value = "{}")]
        args: String,
        /// Act as this user.
        #[arg(long, default_value = "anonymous")]
        principal: String,
        /// Act through this agent.
        #[arg(long, default_value = "cli")]
        agent: String,
    },
    /// Policy commands.
    Policy {
        #[command(subcommand)]
        command: PolicyCommand,
    },
    /// Lint the corpus (exit code 1 on errors).
    Lint {
        #[command(flatten)]
        source: SourceArgs,
        /// Print findings as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Run Kiwi-Bench: recall@5, tokens and latency per mode, and leak counts.
    Bench(bench::BenchArgs),
    /// Audit log commands.
    Audit {
        #[command(subcommand)]
        command: AuditCommand,
    },
    /// List knowledge gaps reported by agents, as a writer may see them.
    Gaps {
        #[command(flatten)]
        source: SourceArgs,
        /// State directory (default <corpus>/.kiwi/state).
        #[arg(long)]
        state: Option<PathBuf>,
        /// The writer: only queries from callers whose view when they asked this user
        /// may read entirely now are shown; the others are counted.
        #[arg(long, default_value = "anonymous")]
        principal: String,
        /// Show at most this many gaps.
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Remove old packs, keeping the current one and the newest (`packs.keep` in
    /// .kiwi/kiwi.toml, 10 by default). Builds do this after every build.
    Gc {
        #[command(flatten)]
        source: SourceArgs,
        /// Packs to keep, the current one included (default: packs.keep).
        #[arg(long)]
        keep: Option<usize>,
    },
    /// Export what a principal may see as llms.txt or JSONL.
    Export {
        #[command(flatten)]
        source: SourceArgs,
        /// `llms-txt` or `jsonl`.
        #[arg(long, default_value = "llms-txt")]
        format: String,
        /// Export what this user may see.
        #[arg(long, default_value = "anonymous")]
        principal: String,
        /// Through this agent (its grant in principals.yaml applies).
        #[arg(long, default_value = "cli")]
        agent: String,
    },
    /// Show build information.
    Info,
}

#[derive(Subcommand)]
enum PolicyCommand {
    /// Run the policy unit tests (`.kiwi/policy.tests.yaml`).
    Test {
        #[command(flatten)]
        source: SourceArgs,
    },
}

#[derive(Subcommand)]
enum AuditCommand {
    /// Verify the hash chain of an audit log (records of every Kiwi version).
    Verify {
        /// The log file.
        file: PathBuf,
        /// Expected hash of the last record (catches truncation).
        #[arg(long)]
        anchor: Option<String>,
    },
    /// Print the records after a sequence number, as sinks receive them (notes
    /// replaced by their keyed MACs), once the whole chain verifies: for shipping the
    /// log elsewhere.
    Export {
        /// The log file.
        file: PathBuf,
        /// Print the records whose sequence number is above this one (0: all).
        #[arg(long, default_value_t = 0)]
        since: u64,
        /// The output format: one JSON record per line.
        #[arg(long, value_enum, default_value_t = AuditFormat::Jsonl)]
        format: AuditFormat,
        /// Expected hash of the last record (catches truncation).
        #[arg(long)]
        anchor: Option<String>,
    },
}

/// Formats of `kiwi audit export`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum AuditFormat {
    /// One JSON record per line.
    Jsonl,
}

#[derive(Args)]
struct ServeArgs {
    /// The corpora to serve. Several are federated by the `namespace` of each one's
    /// .kiwi/kiwi.toml: ids become `namespace:id`, and each corpus keeps its own policy,
    /// principals and audit log.
    #[arg(required = true, num_args = 1..)]
    corpora: Vec<PathBuf>,
    /// Read the working files instead of the committed tree (with --watch).
    #[arg(long, conflicts_with = "rev")]
    worktree: bool,
    /// Serve the pack of this Git revision (one corpus only).
    #[arg(long)]
    rev: Option<String>,
    /// Pack directory (one corpus only; default <corpus>/.kiwi/packs).
    #[arg(long)]
    packs: Option<PathBuf>,
    #[command(flatten)]
    plugins: PluginArgs,
    /// Serve Streamable HTTP on this address instead of stdio.
    #[arg(long)]
    http: Option<SocketAddr>,
    /// The user (stdio). Default: the principal whose e-mail in the directory is the
    /// repository's `git config user.email`.
    #[arg(long, env = "KIWI_PRINCIPAL")]
    principal: Option<String>,
    /// The agent id (stdio).
    #[arg(long, env = "KIWI_AGENT", default_value = "local-agent")]
    agent: String,
    /// State directory for the audit log, feedback and proposals (default
    /// <corpus>/.kiwi/state; with several corpora, <state>/<namespace>).
    #[arg(long)]
    state: Option<PathBuf>,
    /// Do not write the audit log.
    #[arg(long)]
    no_audit: bool,
    /// Rebuild and reload when a file under the corpus changes (the working files, so
    /// not with --rev).
    #[arg(long, conflicts_with = "rev")]
    watch: bool,
    /// HTTP: name of the environment variable holding the HS256 token secret.
    #[arg(long)]
    auth_hs256_env: Option<String>,
    /// HTTP: PEM file with the authorization server's public keys (one or more RSA
    /// keys for RS256, or EC keys for ES256), read again when it changes.
    #[arg(long)]
    auth_rs256_pem: Option<PathBuf>,
    /// HTTP: JWKS file with the authorization server's keys (chosen by `kid`), read
    /// again when it changes.
    #[arg(long)]
    auth_jwks: Option<PathBuf>,
    /// HTTP: sessions held at once; a new one beyond it is refused until one closes or
    /// idles out.
    #[arg(long, default_value_t = 1024)]
    max_sessions: usize,
    /// HTTP: the expected token issuer (the authorization server).
    #[arg(long)]
    issuer: Option<String>,
    /// HTTP: this server's canonical URL, the expected token audience.
    #[arg(long)]
    resource: Option<String>,
    /// HTTP: accepted Host headers (default: localhost only).
    #[arg(long = "allowed-host")]
    allowed_hosts: Vec<String>,
    /// HTTP without authentication, acting as this user through --agent (loopback
    /// only, development).
    #[arg(long)]
    insecure_principal: Option<String>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(2);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Build {
            source,
            plugins,
            as_of,
            json,
        } => cmd_build(&source, &plugins, as_of, json),
        Command::Serve(args) => cmd_serve(args),
        Command::Query {
            source,
            tool,
            args,
            principal,
            agent,
        } => cmd_query(&source, &tool, &args, &principal, &agent),
        Command::Policy {
            command: PolicyCommand::Test { source },
        } => cmd_policy_test(&source),
        Command::Lint { source, json } => cmd_lint(&source, json),
        Command::Bench(args) => bench::run(&args),
        Command::Audit {
            command: AuditCommand::Verify { file, anchor },
        } => cmd_audit_verify(&file, anchor.as_deref()),
        Command::Audit {
            command:
                AuditCommand::Export {
                    file,
                    since,
                    format: AuditFormat::Jsonl,
                    anchor,
                },
        } => cmd_audit_export(&file, since, anchor.as_deref()),
        Command::Gaps {
            source,
            state,
            principal,
            limit,
        } => cmd_gaps(&source, state, &principal, limit),
        Command::Gc { source, keep } => cmd_gc(&source, keep),
        Command::Export {
            source,
            format,
            principal,
            agent,
        } => cmd_export(&source, &format, &principal, &agent),
        Command::Info => {
            println!("kiwi {}", env!("CARGO_PKG_VERSION"));
            println!("plugins: {}", plugins::compiled_in().join(", "));
            Ok(())
        }
    }
}

/// The governance of one corpus snapshot: its policy gate and configuration, and the
/// snapshot they were read from (its id names the pack they govern).
struct Governance {
    gate: PolicyGate,
    config: KiwiConfig,
    /// The governance files (for a directory, every file), with the snapshot id.
    snapshot: kiwi_compiler::source::Snapshot,
}

/// Reads the configuration, policy and principals of one snapshot of the corpus: one
/// commit (a moving revision is resolved once), or for a directory every file at once,
/// so its id is that of the pack a build of the same files makes.
fn load_governance(corpus: &Path, mode: &SourceMode) -> Result<Governance> {
    if !source::uses_git(corpus, mode) {
        let snapshot = source::read_dir(corpus)?;
        let config = load_config(&snapshot)?;
        let gate = gate_from(&snapshot, &config)?;
        return Ok(Governance {
            gate,
            config,
            snapshot,
        });
    }
    let head = source::read_paths(corpus, mode, &[kiwi_compiler::config::CONFIG_PATH])?;
    let config = load_config(&head)?;
    let snapshot = source::read_paths(
        corpus,
        &SourceMode::Git(head.id.clone()),
        &[
            &config.governance.policy,
            &config.governance.principals,
            &config.governance.policy_tests,
        ],
    )?;
    let gate = gate_from(&snapshot, &config)?;
    Ok(Governance {
        gate,
        config,
        snapshot,
    })
}

/// The pack of the snapshot `governance` was read from, never another: a pack served
/// under another revision's policy could show what that policy hides.
fn governed_pack(source: &SourceArgs, governance: &Governance) -> Result<Pack> {
    let id = &governance.snapshot.id;
    let path = PackStore::new(source.packs()).find_snapshot(id)?.ok_or_else(|| {
        anyhow!("no pack of snapshot {id}, the one the policy was read from; build it with `kiwi build` and the same source options")
    })?;
    let pack = Pack::open(path)?;
    if pack.snapshot() != id {
        bail!(
            "the pack is of snapshot {}, not {id} the policy was read from",
            pack.snapshot()
        );
    }
    Ok(pack)
}

/// The policy gate of `snapshot` under `config`.
fn gate_from(snapshot: &kiwi_compiler::source::Snapshot, config: &KiwiConfig) -> Result<PolicyGate> {
    let policy = snapshot.file(&config.governance.policy).ok_or_else(|| {
        anyhow!(
            "{} is missing: without a policy nothing is visible",
            config.governance.policy
        )
    })?;
    let policy = std::str::from_utf8(policy).context("policy is not UTF-8")?;
    let directory = match snapshot.file(&config.governance.principals) {
        Some(bytes) => Directory::parse(std::str::from_utf8(bytes)?)
            .with_context(|| format!("parsing {}", config.governance.principals))?,
        None => Directory::default(),
    };
    Ok(PolicyGate::new(policy, directory)?)
}

/// A finished build, with the governance read from the snapshot it was built from.
struct Built {
    report: kiwi_compiler::BuildReport,
    /// The policy gate of that snapshot, when it has a policy.
    gate: Option<PolicyGate>,
    config: KiwiConfig,
}

fn build_with(source: &SourceArgs, plugin_args: &PluginArgs, as_of: Option<String>) -> Result<Built> {
    let options = BuildOptions {
        root: source.corpus.clone(),
        mode: source.mode(),
        out: Some(source.packs()),
        as_of,
    };
    // The configuration, the policy and the plugins all come from the snapshot the
    // session holds, read under the store's lock.
    let session = kiwi_compiler::BuildSession::begin(&options)?;
    let config = load_config(session.snapshot())?;
    let loaded = plugins::load(&config, &plugin_args.overrides())?;
    // Plugins that reach the network receive only what the policy lets them read, so
    // their build needs the policy; local plugins index everything without it.
    let gate = match gate_from(session.snapshot(), &config) {
        Ok(gate) => Some(gate),
        Err(error) => {
            let network = loaded.network_indexers();
            if !network.is_empty() {
                return Err(error.context(format!(
                    "the {} plugin sends atom text over the network and needs the corpus policy",
                    network.join(", ")
                )));
            }
            None
        }
    };
    let gate_fn = gate.as_ref().map(|gate| {
        move |name: &str, caps: &kiwi_core::plugin::Capabilities, chain: &[kiwi_core::model::AccessDecl]| {
            gate.plugin_allows(name, caps, chain)
        }
    });
    let gate_ref: Option<&PluginGate<'_>> = gate_fn.as_ref().map(|f| f as &PluginGate<'_>);
    #[cfg(feature = "wasm")]
    let wasm = plugins::load_wasm(&config, &source.corpus, None)?;
    #[cfg(feature = "wasm")]
    let enrichers: Vec<&dyn kiwi_core::plugin::Enricher> = wasm
        .iter()
        .map(|p| p as &dyn kiwi_core::plugin::Enricher)
        .collect();
    #[cfg(not(feature = "wasm"))]
    let enrichers: Vec<&dyn kiwi_core::plugin::Enricher> = Vec::new();
    let build_plugins = BuildPlugins {
        indexers: loaded.indexers.iter().map(|i| i.as_ref()).collect(),
        enrichers,
        gate: gate_ref,
    };
    let report = session.finish(&build_plugins)?;
    // Old packs go once the new one is current.
    let pruned = PackStore::new(source.packs()).gc(config.packs.keep as usize)?;
    if !pruned.removed.is_empty() {
        eprintln!(
            "kiwi: removed {} old pack(s), {} bytes (packs.keep = {})",
            pruned.removed.len(),
            pruned.bytes,
            config.packs.keep
        );
    }
    Ok(Built { report, gate, config })
}

fn cmd_build(source: &SourceArgs, plugin_args: &PluginArgs, as_of: Option<String>, json: bool) -> Result<()> {
    let report = build_with(source, plugin_args, as_of)?.report;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "built {} ({}) in {} ms: {} docs, {} atoms ({} compiled, {} reused), {} classes, segments [{}]",
            report.snapshot,
            report.source,
            report.millis,
            report.docs,
            report.atoms,
            report.atoms_rebuilt,
            report.atoms_reused,
            report.classes,
            report.segments.join(", ")
        );
        println!(
            "pack {} {}",
            report.pack.display(),
            if report.written {
                "(written)"
            } else {
                "(unchanged, not rewritten)"
            }
        );
    }
    Ok(())
}

/// Opens one corpus for serving or querying.
fn open_corpus(source: &SourceArgs, state: PathBuf, audit: bool, stdout_is_protocol: bool) -> Result<Corpus> {
    let Governance {
        gate,
        config,
        snapshot,
    } = load_governance(&source.corpus, &source.mode())?;
    let loaded = plugins::load_for_queries(&config)?;
    let options = ServiceOptions {
        corpus: source.corpus.clone(),
        packs: source.packs(),
        state_dir: state,
        config,
        audit,
        proposals: true,
        stdout_is_protocol,
        snapshot: Some(snapshot.id),
    };
    Corpus::open(options, gate, loaded.corpus_plugins())
        .with_context(|| format!("opening {}", source.corpus.display()))
}

fn open_service(source: &SourceArgs, state: Option<PathBuf>, audit: bool) -> Result<Arc<KiwiService>> {
    let state = state.unwrap_or_else(|| source.corpus.join(".kiwi/state"));
    let corpus = open_corpus(source, state, audit, false)?;
    Ok(Arc::new(KiwiService::federate(vec![corpus])?))
}

fn cmd_query(source: &SourceArgs, tool: &str, args: &str, principal: &str, agent: &str) -> Result<()> {
    let service = open_service(source, None, false)?;
    let args: serde_json::Value = serde_json::from_str(args).context("arguments must be JSON")?;
    let identity = Identity::new(principal, agent, "stdio");
    let text = service.call(&identity, tool, args)?;
    println!("{text}");
    Ok(())
}

impl ServeArgs {
    fn sources(&self) -> Result<Vec<SourceArgs>> {
        if self.corpora.len() > 1 && (self.rev.is_some() || self.packs.is_some()) {
            bail!("--rev and --packs apply to one corpus; serve several from their current packs");
        }
        Ok(self
            .corpora
            .iter()
            .map(|corpus| SourceArgs {
                corpus: corpus.clone(),
                worktree: self.worktree,
                rev: self.rev.clone(),
                packs: self.packs.clone(),
            })
            .collect())
    }
}

fn cmd_serve(args: ServeArgs) -> Result<()> {
    // The token-signing secret is the server's own: it is reserved before any corpus
    // configuration is read, so no audit sink or plugin of any corpus (opened now or
    // reloaded later) may name it.
    if let Some(var) = &args.auth_hs256_env {
        kiwi_http::reserve_secret(var);
    }
    let sources = args.sources()?;
    let stdio = args.http.is_none();
    let mut corpora = Vec::with_capacity(sources.len());
    for source in &sources {
        let state = match (&args.state, sources.len()) {
            (Some(dir), 1) => dir.clone(),
            (Some(dir), _) => {
                let config = load_config(&source::read_paths(
                    &source.corpus,
                    &source.mode(),
                    &[kiwi_compiler::config::CONFIG_PATH],
                )?)?;
                dir.join(&config.namespace)
            }
            (None, _) => source.corpus.join(".kiwi/state"),
        };
        corpora.push(open_corpus(source, state, !args.no_audit, stdio)?);
    }
    let service = Arc::new(KiwiService::federate(corpora)?);
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    if args.watch {
        for (index, source) in sources.iter().enumerate() {
            spawn_watcher(source, &args.plugins, Arc::clone(&service), index)?;
        }
    }
    // Watchers keep their own handles on the service, so its sinks are stopped here
    // rather than when the last handle drops.
    let served = Arc::clone(&service);
    let outcome = match args.http {
        None => {
            let (identity, how) =
                resolve_identity(&service, &sources, args.principal.as_deref(), &args.agent);
            eprintln!(
                "kiwi: serving {} over stdio as {} ({how})",
                service.describe(),
                identity.principal
            );
            runtime
                .block_on(kiwi_mcp::serve_stdio(service, identity))
                .map_err(|e| anyhow!(e))
        }
        Some(addr) => serve_http(&args, service, addr, &runtime),
    };
    served.shutdown();
    outcome
}

/// The stdio caller: an explicit principal, else the first corpus whose directory knows
/// the repository's Git e-mail (a principal id names one person in every corpus).
fn resolve_identity(
    service: &KiwiService,
    sources: &[SourceArgs],
    principal: Option<&str>,
    agent: &str,
) -> (Identity, String) {
    let mut first = None;
    for (corpus, source) in service.corpora().iter().zip(sources) {
        let (identity, how) = identity::resolve(corpus.gate().directory(), &source.corpus, principal, agent);
        if principal.is_some() || identity.principal != "anonymous" {
            return (identity, how);
        }
        first.get_or_insert((identity, how));
    }
    first.unwrap_or_else(|| (Identity::new("anonymous", agent, "stdio"), "no corpus".into()))
}

#[cfg(feature = "http")]
fn serve_http(
    args: &ServeArgs,
    service: Arc<KiwiService>,
    addr: SocketAddr,
    runtime: &tokio::runtime::Runtime,
) -> Result<()> {
    use kiwi_mcp::http::{HttpAuth, JwtAuth, KeyFormat, router_with_sessions, serve};
    let auth = if let Some(principal) = &args.insecure_principal {
        if !addr.ip().is_loopback() {
            bail!("--insecure-principal only binds to a loopback address");
        }
        HttpAuth::insecure(principal.clone(), args.agent.clone())
    } else {
        let issuer = args
            .issuer
            .as_deref()
            .ok_or_else(|| anyhow!("--issuer is required for HTTP"))?;
        let resource = args
            .resource
            .clone()
            .unwrap_or_else(|| format!("http://{addr}/mcp"));
        let jwt = match (&args.auth_hs256_env, &args.auth_rs256_pem, &args.auth_jwks) {
            (Some(var), None, None) => {
                // Reserved in `cmd_serve`, before any corpus opened.
                let secret = std::env::var(var).with_context(|| format!("${var} is not set"))?;
                JwtAuth::hs256(secret.as_bytes(), issuer, &resource).map_err(|e| anyhow!(e))?
            }
            (None, Some(pem), None) => {
                JwtAuth::key_file(pem, KeyFormat::Pem, issuer, &resource).map_err(|e| anyhow!(e))?
            }
            (None, None, Some(jwks)) => {
                JwtAuth::key_file(jwks, KeyFormat::Jwks, issuer, &resource).map_err(|e| anyhow!(e))?
            }
            _ => bail!(
                "HTTP needs exactly one of --auth-hs256-env, --auth-rs256-pem or --auth-jwks (or --insecure-principal for local development)"
            ),
        };
        HttpAuth::Jwt(Arc::new(jwt))
    };
    let mut hosts = args.allowed_hosts.clone();
    if hosts.is_empty() {
        hosts = vec![
            "localhost".into(),
            "127.0.0.1".into(),
            "::1".into(),
            addr.to_string(),
            format!("localhost:{}", addr.port()),
        ];
    }
    eprintln!("kiwi: serving {} on http://{addr}/mcp", service.describe());
    runtime.block_on(serve(
        router_with_sessions(service, auth, hosts, args.max_sessions.max(1)),
        addr,
    ))?;
    Ok(())
}

#[cfg(not(feature = "http"))]
fn serve_http(
    _args: &ServeArgs,
    _service: Arc<KiwiService>,
    _addr: SocketAddr,
    _runtime: &tokio::runtime::Runtime,
) -> Result<()> {
    bail!("this kiwi was built without the `http` feature")
}

fn spawn_watcher(
    source: &SourceArgs,
    plugin_args: &PluginArgs,
    service: Arc<KiwiService>,
    index: usize,
) -> Result<()> {
    use notify::{RecursiveMode, Watcher};
    // The source served stays the one asked for: the working files with --worktree,
    // else the commit at HEAD, rebuilt when HEAD or a branch moves.
    let source = source.clone();
    // A linked work tree keeps HEAD in its own Git directory and the branches in the
    // common one: both are watched.
    let git_dirs = if source::uses_git(&source.corpus, &source.mode()) {
        source::git_dirs(&source.corpus)?
    } else {
        Vec::new()
    };
    let plugin_args = plugin_args.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = notify::recommended_watcher(tx)?;
    if git_dirs.is_empty() {
        watcher.watch(&source.corpus, RecursiveMode::Recursive)?;
    }
    for dir in &git_dirs {
        watcher.watch(dir, RecursiveMode::Recursive)?;
    }
    let follows_commits = !git_dirs.is_empty();
    std::thread::spawn(move || {
        let _watcher = watcher;
        while let Ok(event) = rx.recv() {
            let relevant = event.is_ok_and(|e| {
                e.paths.iter().any(|p| {
                    let s = p.to_string_lossy();
                    if follows_commits {
                        !s.ends_with(".lock")
                            && (s.ends_with("/HEAD") || s.contains("/refs/") || s.ends_with("/packed-refs"))
                    } else {
                        !s.contains("/.kiwi/packs") && !s.contains("/.kiwi/state") && !s.contains("/.git/")
                    }
                })
            });
            if !relevant {
                continue;
            }
            // Coalesce bursts of events from one save.
            std::thread::sleep(std::time::Duration::from_millis(150));
            while rx.try_recv().is_ok() {}
            let corpus = &service.corpora()[index];
            match build_with(&source, &plugin_args, None) {
                Ok(built) => match reload(corpus, built) {
                    Ok(report) => eprintln!(
                        "kiwi: {} rebuilt in {} ms ({} atoms compiled)",
                        corpus.namespace(),
                        report.millis,
                        report.atoms_rebuilt
                    ),
                    Err(error) => eprintln!("kiwi: {} reload failed: {error:#}", corpus.namespace()),
                },
                Err(error) => eprintln!("kiwi: {} rebuild failed: {error:#}", corpus.namespace()),
            }
        }
    });
    Ok(())
}

/// Serves a rebuilt pack together with the policy, principals, configuration and audit
/// sinks of the snapshot it was built from, all swapped at once. A snapshot without a
/// policy, or one that fails to load, leaves the previous state serving.
fn reload(corpus: &Corpus, built: Built) -> Result<kiwi_compiler::BuildReport> {
    let Built { report, gate, config } = built;
    let gate = gate.ok_or_else(|| {
        anyhow!(
            "{} is missing in the rebuilt snapshot; still serving the previous pack and policy",
            config.governance.policy
        )
    })?;
    let factories = plugins::query_factories(&config);
    corpus.replace(&report.pack, gate, config, factories)?;
    Ok(report)
}

fn cmd_policy_test(source: &SourceArgs) -> Result<()> {
    let Governance {
        gate,
        config,
        snapshot,
    } = load_governance(&source.corpus, &source.mode())?;
    let tests = snapshot
        .file(&config.governance.policy_tests)
        .ok_or_else(|| anyhow!("{} is missing", config.governance.policy_tests))?;
    let tests: policytest::PolicyTests = serde_yaml_ng_parse(tests)?;
    let results = policytest::run(&gate, &tests)?;
    let failed = results.iter().filter(|r| !r.passed).count();
    for result in &results {
        println!(
            "{} {} (expected {}, got {})",
            if result.passed { "ok  " } else { "FAIL" },
            result.name,
            result.expected,
            result.actual
        );
    }
    println!("{} passed, {failed} failed", results.len() - failed);
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn serde_yaml_ng_parse<T: for<'de> serde::Deserialize<'de>>(bytes: &[u8]) -> Result<T> {
    Ok(serde_yaml_ng::from_slice(bytes)?)
}

fn cmd_lint(source: &SourceArgs, json: bool) -> Result<()> {
    let snapshot = source::read(&source.corpus, &source.mode())?;
    // Lint keeps its own compile cache: it may read another tree than the last build
    // (the working copy), and pruning the build's cache would make the next build
    // compile everything again.
    let cache = source.packs().join("lint-cache");
    let (data, _, _) =
        kiwi_compiler::build::compile_snapshot(&snapshot, None, &cache, None, &BuildPlugins::default())?;
    let pack = Pack::from_bytes(&kiwi_core::pack::encode(&data)?)?;
    #[allow(unused_mut)]
    let mut findings = BuiltinLint.check(&pack)?;
    #[cfg(feature = "wasm")]
    {
        let config = kiwi_compiler::build::load_config(&snapshot)?;
        let gate: Option<kiwi_wasm::Gate> =
            load_governance(&source.corpus, &source.mode())
                .ok()
                .map(|governance| {
                    let gate = Arc::new(governance.gate);
                    Arc::new(
                        move |name: &str,
                              caps: &kiwi_core::plugin::Capabilities,
                              chain: &[kiwi_core::model::AccessDecl]| {
                            gate.plugin_allows(name, caps, chain)
                        },
                    ) as kiwi_wasm::Gate
                });
        for plugin in plugins::load_wasm(&config, &source.corpus, gate)? {
            findings.extend(plugin.check(&pack)?);
        }
        findings.sort();
    }
    let errors = findings.iter().filter(|f| f.severity == Severity::Error).count();
    if json {
        println!("{}", serde_json::to_string_pretty(&findings)?);
    } else {
        for f in &findings {
            let level = if f.severity == Severity::Error {
                "error"
            } else {
                "warning"
            };
            println!("{level}[{}] {}: {}", f.rule, f.id, f.message);
        }
        println!("{} findings, {errors} errors", findings.len());
    }
    if errors > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn cmd_audit_verify(file: &Path, anchor: Option<&str>) -> Result<()> {
    match audit::verify(file, anchor) {
        Ok(report) => {
            println!(
                "ok: {} records, chain intact, last hash {}",
                report.records, report.last
            );
            for found in &report.breaks {
                println!(
                    "break at line {}: a torn line of {} bytes left by a crash was moved to {} (blake3 {})",
                    found.line, found.quarantine.bytes, found.quarantine.file, found.quarantine.blake3
                );
            }
            if report.torn > 0 {
                println!(
                    "torn tail: {} bytes after record {} were left by an interrupted write; the next append moves them aside",
                    report.torn, report.records
                );
            }
            Ok(())
        }
        Err(error) => {
            println!("TAMPERED: {error}");
            std::process::exit(1);
        }
    }
}

/// Prints the verified records after `since`; on a broken chain, prints nothing to
/// standard output and fails.
fn cmd_audit_export(file: &Path, since: u64, anchor: Option<&str>) -> Result<()> {
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    match audit::export(file, since, anchor, &mut out) {
        Ok(_) => Ok(()),
        Err(error @ audit::AuditError::Broken { .. }) => {
            eprintln!("TAMPERED: {error}");
            std::process::exit(1);
        }
        Err(error) => Err(error.into()),
    }
}

fn cmd_gaps(source: &SourceArgs, state: Option<PathBuf>, principal: &str, limit: usize) -> Result<()> {
    let dir = state.unwrap_or_else(|| source.corpus.join(".kiwi/state"));
    let governance = load_governance(&source.corpus, &source.mode())?;
    let pack = governed_pack(source, &governance)?;
    let gate = governance.gate;
    let writer = Identity::new(principal, "cli", "stdio");
    // A record is judged by the view its asker had when asking: the writer must be
    // allowed every access chain the asker could read then. A record that kept no view
    // is shown only to a writer who may read the whole current pack.
    let everything = gate.classes(&pack, &writer)?.len() == pack.class_count() as usize;
    let mut allowed: std::collections::HashMap<Vec<kiwi_core::model::AccessDecl>, bool> =
        std::collections::HashMap::new();
    let mut failure = None;
    let log = kiwi_mcp::feedback::FeedbackLog::new(dir.join("feedback.jsonl"));
    let shown = log.gaps(|record| {
        let Some(view) = &record.view else {
            return everything;
        };
        view.iter().all(|chain| {
            if let Some(&known) = allowed.get(chain) {
                return known;
            }
            let verdict = match gate.allows_chain(&writer, chain) {
                Ok(verdict) => verdict,
                Err(error) => {
                    failure.get_or_insert(error);
                    false
                }
            };
            allowed.insert(chain.clone(), verdict);
            verdict
        })
    })?;
    if let Some(error) = failure {
        return Err(error.into());
    }
    if shown.gaps.is_empty() {
        println!("no knowledge gaps recorded");
    }
    for gap in shown.gaps.iter().take(limit) {
        println!("{:>4}  {}  (last {})", gap.count, gap.query, gap.last_seen);
    }
    if shown.gaps.len() > limit {
        println!("… {} more (raise --limit)", shown.gaps.len() - limit);
    }
    if shown.withheld > 0 {
        println!(
            "{} report(s) withheld: asked by someone who can see more than {principal}",
            shown.withheld
        );
    }
    Ok(())
}

fn cmd_gc(source: &SourceArgs, keep: Option<usize>) -> Result<()> {
    let keep = match keep {
        Some(keep) => keep,
        None => {
            let config = load_config(&source::read_paths(
                &source.corpus,
                &source.mode(),
                &[kiwi_compiler::config::CONFIG_PATH],
            )?)?;
            config.packs.keep as usize
        }
    };
    let report = PackStore::new(source.packs()).gc(keep)?;
    println!(
        "kept {} pack(s), removed {} ({} bytes)",
        report.kept,
        report.removed.len(),
        report.bytes
    );
    Ok(())
}

fn cmd_export(source: &SourceArgs, format: &str, principal: &str, agent: &str) -> Result<()> {
    let governance = load_governance(&source.corpus, &source.mode())?;
    let pack = governed_pack(source, &governance)?;
    let gate = governance.gate;
    let view = gate.view(&pack, &Identity::new(principal, agent, "stdio"))?;
    let mut out = std::io::stdout().lock();
    match format {
        "llms-txt" => kiwi_core::export::llms_txt(&pack, view.visible(), &mut out)?,
        "jsonl" => kiwi_core::export::jsonl(&pack, view.visible(), &mut out)?,
        other => bail!("unknown format `{other}` (llms-txt or jsonl)"),
    }
    Ok(())
}
