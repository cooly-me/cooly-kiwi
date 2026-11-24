//! `kiwi serve --follow`: serve each pack another process makes current.
//!
//! A builder (`kiwi build`, on a timer or in CI) writes a new pack into the store and
//! then replaces the store's `CURRENT` pointer atomically. A following server notices,
//! opens that pack, reads the policy, principals and configuration of the pack's own
//! snapshot and swaps all of them in at once ([`Corpus::replace`]). A call takes what
//! the corpus serves once, when it starts, so calls in flight finish on the pack they
//! started with and every later call uses the new one; no call fails or waits.
//!
//! Three triggers lead to the same idempotent check of every corpus: a change to a
//! store's `CURRENT` (file system notifications), `SIGHUP`, and a re-read every
//! [`POLL`] in case a notification is lost. A check that finds `CURRENT` naming the
//! pack already served does nothing. Anything that fails (a pack that does not open, a
//! snapshot whose policy does not load) is reported once and leaves the served state as
//! it was; that pack is tried again when `CURRENT` moves or on `SIGHUP`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use kiwi_compiler::{SourceMode, source};
use kiwi_core::{Pack, PackStore};
use kiwi_mcp::{Corpus, KiwiService};

use crate::{SourceArgs, load_governance, plugins};

/// How often every store's `CURRENT` is re-read without a notification.
pub const POLL: Duration = Duration::from_secs(30);

/// How long a burst of notifications is left to settle before the check.
const SETTLE: Duration = Duration::from_millis(100);

/// What the follower thread acts on.
enum Trigger {
    /// A file in a watched store changed.
    Store,
    /// The process received `SIGHUP`.
    Signal,
}

/// Starts following the pack stores of `sources` (by corpus index, as served by
/// `service`): a thread that swaps in each new current pack, the file system watcher
/// that wakes it, and on Unix a `SIGHUP` handler on `runtime`.
pub fn spawn(
    sources: &[SourceArgs],
    service: Arc<KiwiService>,
    runtime: &tokio::runtime::Runtime,
) -> Result<()> {
    use notify::{RecursiveMode, Watcher};
    let stores: Vec<PathBuf> = sources.iter().map(SourceArgs::packs).collect();
    let (tx, rx) = mpsc::channel::<Trigger>();
    let notify_tx = tx.clone();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let current = event.is_ok_and(|e| {
            e.paths
                .iter()
                .any(|p| p.file_name().is_some_and(|name| name == "CURRENT"))
        });
        if current {
            let _ = notify_tx.send(Trigger::Store);
        }
    })?;
    for store in &stores {
        watcher.watch(store, RecursiveMode::NonRecursive)?;
    }
    #[cfg(unix)]
    spawn_signal_handler(runtime, tx)?;
    #[cfg(not(unix))]
    let _ = (runtime, tx);
    let sources = sources.to_vec();
    std::thread::Builder::new()
        .name("kiwi-follow".into())
        .spawn(move || {
            let _watcher = watcher;
            // By corpus: the current pack that last failed to load, not retried until
            // `CURRENT` moves or a SIGHUP asks.
            let mut failed: Vec<Option<String>> = vec![None; sources.len()];
            loop {
                let signalled = match rx.recv_timeout(POLL) {
                    Ok(Trigger::Store) => {
                        std::thread::sleep(SETTLE);
                        while rx.try_recv().is_ok() {}
                        false
                    }
                    Ok(Trigger::Signal) => true,
                    Err(RecvTimeoutError::Timeout) => false,
                    Err(RecvTimeoutError::Disconnected) => return,
                };
                for ((corpus, source), failed) in service.corpora().iter().zip(&sources).zip(&mut failed) {
                    let current = current_hash(source);
                    if !signalled && failed.is_some() && *failed == current {
                        continue;
                    }
                    match follow(corpus, source) {
                        Ok(swapped) => {
                            *failed = None;
                            if let Some(swapped) = swapped {
                                eprintln!("kiwi: {} now serves {swapped}", corpus.namespace());
                            }
                        }
                        Err(error) => {
                            *failed = current;
                            eprintln!("kiwi: {} keeps serving its pack: {error:#}", corpus.namespace());
                        }
                    }
                }
            }
        })?;
    Ok(())
}

/// Forwards every `SIGHUP` to the follower.
#[cfg(unix)]
fn spawn_signal_handler(runtime: &tokio::runtime::Runtime, tx: Sender<Trigger>) -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut hangups = {
        let _guard = runtime.enter();
        signal(SignalKind::hangup())?
    };
    runtime.spawn(async move {
        while hangups.recv().await.is_some() {
            if tx.send(Trigger::Signal).is_err() {
                return;
            }
        }
    });
    Ok(())
}

/// Serves the pack `CURRENT` names in `source`'s store, if it is not served already,
/// with the governance of its snapshot. Returns what is served now when it changed.
pub fn follow(corpus: &Corpus, source: &SourceArgs) -> Result<Option<String>> {
    let store = PackStore::new(source.packs());
    let Some(path) = store.current()? else {
        return Ok(None);
    };
    if pack_hash(&path).is_some_and(|hash| hash == corpus.engine().pack().id()) {
        return Ok(None);
    }
    let pack = Pack::open(&path)?;
    let snapshot = pack.snapshot().to_owned();
    let of_commit = !snapshot.starts_with("dir:");
    if of_commit != source::uses_git(&source.corpus, &source.mode()) {
        bail!(
            "the current pack is of {snapshot}, and this server serves {}; build {} instead",
            if of_commit { "the working files" } else { "commits" },
            if of_commit { "with --worktree" } else { "a commit" },
        );
    }
    let mode = if of_commit {
        SourceMode::Git(snapshot.clone())
    } else {
        source.mode()
    };
    let governance = load_governance(&source.corpus, &mode)?;
    if governance.snapshot.id != snapshot {
        bail!("the files changed after the build of {snapshot}; the next build is served once it is current");
    }
    let factories = plugins::query_factories(&governance.config);
    corpus
        .replace(&path, governance.gate, governance.config, factories)
        .map_err(|e| anyhow!(e))?;
    let served = corpus.engine();
    Ok(Some(format!(
        "pack {} of {}",
        &served.pack().id()[..12.min(served.pack().id().len())],
        served.pack().snapshot()
    )))
}

/// The hash `CURRENT` names in `source`'s store, if it names one.
fn current_hash(source: &SourceArgs) -> Option<String> {
    let path = PackStore::new(source.packs()).current().ok()??;
    pack_hash(&path).map(str::to_owned)
}

/// The content hash a pack file is named by (`<hash>.kpack`).
fn pack_hash(path: &Path) -> Option<&str> {
    path.file_name()?.to_str()?.strip_suffix(".kpack")
}
