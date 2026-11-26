//! The audit log: one JSON line per call, append-only and hash-chained.
//!
//! A record says who called which tool, how the call ended, which atoms it returned by
//! their stable ids and documents, the tokens of the body the caller received and the
//! budget applied, and the server time; what the caller asked for only when the corpus
//! records requests. Optional fields are left out when empty, so a record hashes the
//! same whichever of them it carries, and records without them verify as written.
//!
//! Every record carries the hash of the record before it, and its own hash covers its
//! content and that link. Editing, reordering or deleting any line breaks the chain at
//! that line, which `kiwi audit verify` reports. Truncating the tail can only be caught
//! against a saved hash (`--anchor`), which a sink can ship elsewhere as it is written.
//!
//! Several processes may append to one log (two servers on one corpus): each append
//! holds an exclusive lock on the file and links to the record at the end of the file
//! as it is under that lock, so the chain never forks. A line a crash left half
//! written (the file does not end with a newline) is moved aside to a quarantine file
//! next to the log, and the chain continues with a record that names that file, the
//! bytes moved and their hash; `kiwi audit verify` reports each such break. A complete
//! line that is not a valid record is tampering, and the log is not extended.
//!
//! Sinks receive every record as written, except its `note`: a note may name content
//! hidden from the caller (a refused proposal's cause, a conflict with a hidden
//! document), so it stays in the local log and a sink receives only a keyed BLAKE3 of
//! it (`note_mac`), under a key kept beside the local log (`audit.log.key`): enough to
//! match the record against the local log, and no way to test a guess of its text.
//! [`export`] prints verified records the same way, for shipping a log elsewhere.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use kiwi_core::date;
use kiwi_core::plugin::AuditSink;
use serde::{Deserialize, Serialize};

/// The `prev` of the first record.
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// The tool name of a record that marks a quarantined torn line.
pub const QUARANTINE_TOOL: &str = "audit-quarantine";

/// The longest record the tail reader accepts.
const MAX_RECORD_BYTES: u64 = 16 * 1024 * 1024;

/// Errors of the audit log.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    /// I/O on the log file.
    #[error("{path}: {source}")]
    Io {
        /// The log.
        path: PathBuf,
        /// The error.
        #[source]
        source: std::io::Error,
    },
    /// The record that ends the log, read to append after it, is not a record.
    #[error("the last record of the log is not a record ({0}); the log is not extended")]
    Tail(String),
    /// The chain is broken.
    #[error("line {line}: {reason}")]
    Broken {
        /// 1-based line.
        line: usize,
        /// What does not match.
        reason: String,
    },
}

/// How a call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuditStatus {
    /// Answered.
    Ok,
    /// Refused with a message to the caller (invalid arguments, an unknown id, a proposal
    /// the policy or the corpus does not allow).
    Refused,
    /// An internal error; the caller was given only its `reference`.
    Failed,
}

/// What one record says.
///
/// Every optional field is left out of the record when empty or unset, so records that
/// never had it hash and verify as written.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEvent {
    /// The user.
    pub principal: String,
    /// The agent acting for them.
    pub agent: String,
    /// `stdio` or `http`.
    pub channel: String,
    /// The tool called.
    pub tool: String,
    /// The snapshot answered from.
    pub snapshot: String,
    /// The corpus's namespace. Left out of the record when empty, so a record of a
    /// corpus served alone hashes without it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub namespace: String,
    /// Pack uids of `ids`. They name atoms of one pack only: `ids` is what stays
    /// meaningful once the corpus is rebuilt.
    pub atoms: Vec<u32>,
    /// Matches withheld by the policy gate.
    pub blocked: u32,
    /// How the call ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<AuditStatus>,
    /// The stable ids (`path#anchor`) of the atoms of this corpus the answer returned
    /// (or a feedback named, or a proposal changed), distinct, in answer order. Only
    /// atoms the caller can see.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ids: Vec<String>,
    /// The distinct document paths (without `.md`) `ids` belong to, in first-seen order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub docs: Vec<String>,
    /// Items the answer returned: search results, map nodes, read atoms, trace nodes, or
    /// one atom for a resource. Unset for feedback and proposals, and on a refusal or a
    /// failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub results: Option<u32>,
    /// Tokens of the body the caller received (the answer, or the refusal or failure
    /// message), counted with the budget's tokenizer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u32>,
    /// The token budget applied (search and map).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<u32>,
    /// Milliseconds the server spent on the call, until the record was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    /// What the caller asked for (search queries, read ids, a map scope, a trace id, a
    /// feedback query, the paths and sections a proposal names, a resource URI). Only
    /// when the corpus sets `audit.record_requests`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queries: Vec<String>,
    /// The reference of a failure, as the caller was given it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// What the caller was not told, for whoever reads the local log: why a proposal
    /// was refused or failed, or its conflicts with content hidden from the caller. Left
    /// out when empty, so records without one hash as before. Never sent to a sink,
    /// which receives `note_mac` instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Bytes a crash left half written, moved aside to a quarantine file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quarantine {
    /// The quarantine file's name, next to the log.
    pub file: String,
    /// How many bytes were moved.
    pub bytes: u64,
    /// BLAKE3 of those bytes.
    pub blake3: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Body {
    seq: u64,
    ts: String,
    #[serde(flatten)]
    event: AuditEvent,
    prev: String,
    /// Only on a quarantine record; absent elsewhere, so other records hash as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    quarantined: Option<Quarantine>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    #[serde(flatten)]
    body: Body,
    hash: String,
}

fn hash_body(body: &Body) -> Result<String, serde_json::Error> {
    let canonical = serde_json::to_vec(body)?;
    Ok(blake3::hash(&canonical).to_hex().to_string())
}

/// The end of the chain as last seen in the file.
struct Tail {
    file: File,
    /// The file's length after the last append seen; another process's append changes
    /// it, and the tail is then read again.
    len: u64,
    seq: u64,
    last: String,
    sinks: Vec<Box<dyn AuditSink>>,
}

/// An open audit log.
pub struct AuditLog {
    path: PathBuf,
    /// The key of the notes' MACs sinks receive.
    note_key: [u8; 32],
    tail: Mutex<Tail>,
}

impl std::fmt::Debug for AuditLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuditLog").field("path", &self.path).finish()
    }
}

/// Holds the exclusive lock of a log file until dropped.
struct FileLock<'a>(&'a File);

impl<'a> FileLock<'a> {
    fn take(file: &'a File) -> std::io::Result<Self> {
        file.lock()?;
        Ok(Self(file))
    }
}

impl Drop for FileLock<'_> {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

impl AuditLog {
    /// Opens (or creates) the log at `path`, verifying the existing chain first so new
    /// records never extend a tampered one. A torn last line is quarantined.
    pub fn open(path: impl Into<PathBuf>, sinks: Vec<Box<dyn AuditSink>>) -> Result<Self, AuditError> {
        let path = path.into();
        let io = |source: std::io::Error| AuditError::Io {
            path: path.clone(),
            source,
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| AuditError::Io {
                path: parent.to_owned(),
                source,
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)
            .map_err(io)?;
        let mut tail = Tail {
            file,
            len: 0,
            seq: 0,
            last: GENESIS.to_owned(),
            sinks,
        };
        let note_key = note_key(&path).map_err(|source| AuditError::Io {
            path: note_key_path(&path),
            source,
        })?;
        let quarantined = {
            let handle = tail.file.try_clone().map_err(io)?;
            let _lock = FileLock::take(&handle).map_err(io)?;
            let scan = scan(BufReader::new(&handle), &path)?;
            tail.seq = scan.records as u64;
            tail.last = scan.last;
            tail.len = scan.good_len;
            if scan.torn > 0 {
                Some(quarantine(&path, &mut tail, scan.good_len)?)
            } else {
                None
            }
        };
        if let Some(line) = quarantined {
            let shipped = for_sinks(line.trim_end(), &note_key).map_err(|e| io(std::io::Error::other(e)))?;
            for sink in &tail.sinks {
                if let Err(error) = sink.deliver(&shipped) {
                    tracing::warn!(sink = sink.name(), %error, "audit sink failed");
                }
            }
        }
        Ok(Self {
            path,
            note_key,
            tail: Mutex::new(tail),
        })
    }

    /// The log file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Ships what the sinks still hold and stops them (the local log is already
    /// complete). Records appended afterwards reach only the local log.
    pub fn close(&self) {
        if let Ok(tail) = self.tail.lock() {
            for sink in &tail.sinks {
                sink.close();
            }
        }
    }

    /// Replaces the sinks (a reloaded configuration). The old sinks ship what they hold
    /// and stop; records from now on go to the new ones.
    pub fn replace_sinks(&self, sinks: Vec<Box<dyn AuditSink>>) {
        let old = match self.tail.lock() {
            Ok(mut tail) => std::mem::replace(&mut tail.sinks, sinks),
            Err(_) => return,
        };
        for sink in &old {
            sink.close();
        }
    }

    /// Appends one record and hands it to the sinks. Returns the record's hash.
    pub fn record(&self, event: AuditEvent) -> Result<String, AuditError> {
        let io = |source: std::io::Error| AuditError::Io {
            path: self.path.clone(),
            source,
        };
        let mut guard = self
            .tail
            .lock()
            .map_err(|_| io(std::io::Error::other("audit lock poisoned")))?;
        let tail = &mut *guard;
        let file = tail.file.try_clone().map_err(io)?;
        let lock = FileLock::take(&file).map_err(io)?;
        let len = file.metadata().map_err(io)?.len();
        let mut lines = Vec::with_capacity(2);
        if len != tail.len {
            // Another process appended (or a crash left a torn line): link to the
            // record that ends the file now.
            let end = read_tail(&file, len, &self.path)?;
            tail.seq = end.seq;
            tail.last = end.last;
            tail.len = end.good_len;
            if end.good_len != len {
                lines.push(quarantine(&self.path, tail, end.good_len)?);
            }
        }
        lines.push(append(&self.path, tail, event, None)?);
        drop(lock);
        // Handed to the sinks under the in-process lock, so they receive this process's
        // records in `seq` order. Delivery only queues the line (it never blocks); a
        // failing sink must not lose the local record, which is already durable.
        for line in &lines {
            let shipped =
                for_sinks(line.trim_end(), &self.note_key).map_err(|e| io(std::io::Error::other(e)))?;
            for sink in &tail.sinks {
                if let Err(error) = sink.deliver(&shipped) {
                    tracing::warn!(sink = sink.name(), %error, "audit sink failed");
                }
            }
        }
        Ok(tail.last.clone())
    }
}

/// A record as a sink receives it: as written, with its `note` replaced by the note's
/// keyed BLAKE3 under `key` (`note_mac`), and a quarantine record's hash and length of
/// the torn bytes (which may hold a note) replaced by a keyed BLAKE3 of that hash
/// (`quarantined.mac`).
fn for_sinks(line: &str, key: &[u8; 32]) -> Result<String, serde_json::Error> {
    let mut record: serde_json::Map<String, serde_json::Value> = serde_json::from_str(line)?;
    let mut changed = false;
    if let Some(serde_json::Value::String(note)) = record.remove("note") {
        record.insert("note_mac".into(), serde_json::Value::String(note_mac(key, &note)));
        changed = true;
    }
    if let Some(serde_json::Value::Object(quarantined)) = record.get_mut("quarantined") {
        let hash = quarantined.remove("blake3");
        quarantined.remove("bytes");
        if let Some(serde_json::Value::String(hash)) = hash {
            quarantined.insert("mac".into(), serde_json::Value::String(note_mac(key, &hash)));
        }
        changed = true;
    }
    if changed {
        serde_json::to_string(&record)
    } else {
        Ok(line.to_owned())
    }
}

/// The MAC a sink receives for `note`.
pub fn note_mac(key: &[u8; 32], note: &str) -> String {
    blake3::keyed_hash(key, note.as_bytes()).to_hex().to_string()
}

/// The file beside the log `path` that keeps the key of its notes' MACs.
pub fn note_key_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".key");
    path.with_file_name(name)
}

/// The key of the notes' MACs of the log `path`, read from its key file. The key file
/// must be its owner's only.
fn read_note_key(path: &Path) -> std::io::Result<[u8; 32]> {
    let key_path = note_key_path(path);
    // The mode is checked on the handle the key is read from, so the file read is the
    // file checked.
    let mut file = File::open(&key_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = file.metadata()?.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(std::io::Error::other(format!(
                "{} is readable by others (mode {:o}); make it its owner's only (chmod 600)",
                key_path.display(),
                mode & 0o777
            )));
        }
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let bytes = hex::decode(text.trim()).map_err(std::io::Error::other)?;
    bytes
        .try_into()
        .map_err(|_| std::io::Error::other(format!("{} is not a 32-byte key", key_path.display())))
}

/// The key of the notes' MACs of the log `path`: read from its key file, or made from
/// the operating system's random source and written there (readable by its owner only)
/// on first use. Several processes on one log agree on it: the file is created once.
fn note_key(path: &Path) -> std::io::Result<[u8; 32]> {
    let key_path = note_key_path(path);
    match read_note_key(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        other => return other,
    }
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).map_err(|e| std::io::Error::other(e.to_string()))?;
    // Written whole under another name, then linked into place: a reader never sees a
    // half-written key, and of two processes the first link wins.
    let mut temp_name = key_path.file_name().unwrap_or_default().to_os_string();
    temp_name.push(format!(".tmp-{}-{}", std::process::id(), hex::encode(&key[..4])));
    let temp = key_path.with_file_name(temp_name);
    // Created exclusively (O_EXCL) with the owner's mode only.
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    file.write_all(hex::encode(key).as_bytes())?;
    file.sync_all()?;
    drop(file);
    let linked = std::fs::hard_link(&temp, &key_path);
    let _ = std::fs::remove_file(&temp);
    match linked {
        Ok(()) => Ok(key),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => read_note_key(path),
        Err(error) => Err(error),
    }
}

/// Appends one record after the tail, under the file lock. Returns the line written.
fn append(
    path: &Path,
    tail: &mut Tail,
    event: AuditEvent,
    quarantined: Option<Quarantine>,
) -> Result<String, AuditError> {
    let io = |source: std::io::Error| AuditError::Io {
        path: path.to_owned(),
        source,
    };
    let body = Body {
        seq: tail.seq + 1,
        ts: date::timestamp(date::now_unix()),
        event,
        prev: tail.last.clone(),
        quarantined,
    };
    let hash = hash_body(&body).map_err(|e| io(std::io::Error::other(e)))?;
    let record = Record {
        body,
        hash: hash.clone(),
    };
    let mut line = serde_json::to_string(&record).map_err(|e| io(std::io::Error::other(e)))?;
    line.push('\n');
    tail.file.write_all(line.as_bytes()).map_err(io)?;
    tail.file.flush().map_err(io)?;
    tail.seq += 1;
    tail.last = hash;
    tail.len = tail.file.metadata().map_err(io)?.len();
    Ok(line)
}

/// Moves the bytes after `good_len` (a line a crash left half written) to a quarantine
/// file, cuts them from the log and appends a record of the break. Runs under the file
/// lock.
fn quarantine(path: &Path, tail: &mut Tail, good_len: u64) -> Result<String, AuditError> {
    let io = |source: std::io::Error| AuditError::Io {
        path: path.to_owned(),
        source,
    };
    let mut torn = Vec::new();
    let mut reader = &tail.file;
    reader.seek(SeekFrom::Start(good_len)).map_err(io)?;
    reader.read_to_end(&mut torn).map_err(io)?;
    let name = format!(
        "{}.torn-{}-{}",
        path.file_name()
            .map_or("audit.log".into(), |n| n.to_string_lossy()),
        tail.seq + 1,
        date::now_unix()
    );
    let target = path.with_file_name(&name);
    let mut out = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&target)
        .map_err(|source| AuditError::Io {
            path: target.clone(),
            source,
        })?;
    out.write_all(&torn)
        .and_then(|()| out.sync_all())
        .map_err(|source| AuditError::Io {
            path: target.clone(),
            source,
        })?;
    tail.file.set_len(good_len).map_err(io)?;
    tracing::warn!(
        log = %path.display(),
        quarantine = %target.display(),
        bytes = torn.len(),
        "audit log: a torn last line was moved aside"
    );
    let event = AuditEvent {
        principal: "kiwi".into(),
        agent: "kiwi".into(),
        channel: "local".into(),
        tool: QUARANTINE_TOOL.into(),
        ..AuditEvent::default()
    };
    let record = Quarantine {
        file: name,
        bytes: torn.len() as u64,
        blake3: blake3::hash(&torn).to_hex().to_string(),
    };
    append(path, tail, event, Some(record))
}

/// The record that ends a log of `len` bytes, and where the complete lines end.
struct End {
    seq: u64,
    last: String,
    good_len: u64,
}

/// Reads the last complete record of the file, from its end.
fn read_tail(file: &File, len: u64, path: &Path) -> Result<End, AuditError> {
    let io = |source: std::io::Error| AuditError::Io {
        path: path.to_owned(),
        source,
    };
    let mut reader = file;
    let mut window = 4096u64;
    loop {
        let start = len.saturating_sub(window);
        let mut bytes = vec![0u8; (len - start) as usize];
        reader.seek(SeekFrom::Start(start)).map_err(io)?;
        reader.read_exact(&mut bytes).map_err(io)?;
        // Complete lines end at the last newline; anything after it is torn.
        let complete = bytes.iter().rposition(|&b| b == b'\n').map(|i| i + 1);
        let Some(complete) = complete else {
            if start == 0 {
                // No complete line at all: the whole file is torn.
                return Ok(End {
                    seq: 0,
                    last: GENESIS.to_owned(),
                    good_len: 0,
                });
            }
            window = grow(window, path)?;
            continue;
        };
        let lines = &bytes[..complete - 1];
        let begin = match lines.iter().rposition(|&b| b == b'\n') {
            Some(i) => i + 1,
            None if start == 0 => 0,
            None => {
                window = grow(window, path)?;
                continue;
            }
        };
        let record: Record =
            serde_json::from_slice(&lines[begin..]).map_err(|e| AuditError::Tail(e.to_string()))?;
        return Ok(End {
            seq: record.body.seq,
            last: record.hash,
            good_len: start + complete as u64,
        });
    }
}

fn grow(window: u64, path: &Path) -> Result<u64, AuditError> {
    if window >= MAX_RECORD_BYTES {
        return Err(AuditError::Io {
            path: path.to_owned(),
            source: std::io::Error::other("the last audit record is too long"),
        });
    }
    Ok(window * 4)
}

/// A break in the chain: a quarantined torn line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainBreak {
    /// The 1-based line of the quarantine record.
    pub line: usize,
    /// What was moved aside.
    pub quarantine: Quarantine,
}

/// The result of a successful verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// Records checked.
    pub records: usize,
    /// Hash of the last record (the anchor to keep).
    pub last: String,
    /// Torn lines a crash left and that were quarantined, in order.
    pub breaks: Vec<ChainBreak>,
    /// Bytes of a torn last line not yet quarantined (the next append moves them).
    pub torn: u64,
}

struct Scan {
    records: usize,
    last: String,
    good_len: u64,
    torn: u64,
    breaks: Vec<ChainBreak>,
}

fn scan(mut reader: impl BufRead, path: &Path) -> Result<Scan, AuditError> {
    let mut prev = GENESIS.to_owned();
    let mut records = 0usize;
    let mut good_len = 0u64;
    let mut breaks = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|source| AuditError::Io {
                path: path.to_owned(),
                source,
            })?;
        if read == 0 {
            break;
        }
        if line.last() != Some(&b'\n') {
            // A crash in the middle of a write: the line never ended.
            return Ok(Scan {
                records,
                last: prev,
                good_len,
                torn: read as u64,
                breaks,
            });
        }
        let number = records + 1;
        let text = std::str::from_utf8(&line[..line.len() - 1]).map_err(|_| AuditError::Broken {
            line: number,
            reason: "not UTF-8".into(),
        })?;
        if text.trim().is_empty() {
            return Err(AuditError::Broken {
                line: number,
                reason: "empty line".into(),
            });
        }
        let record: Record = serde_json::from_str(text).map_err(|e| AuditError::Broken {
            line: number,
            reason: format!("not a record: {e}"),
        })?;
        if record.body.seq != number as u64 {
            return Err(AuditError::Broken {
                line: number,
                reason: format!("sequence {} where {number} was expected", record.body.seq),
            });
        }
        if record.body.prev != prev {
            return Err(AuditError::Broken {
                line: number,
                reason: "link to the previous record does not match".into(),
            });
        }
        let expected = hash_body(&record.body).map_err(|e| AuditError::Broken {
            line: number,
            reason: e.to_string(),
        })?;
        if expected != record.hash {
            return Err(AuditError::Broken {
                line: number,
                reason: "content does not match its hash".into(),
            });
        }
        if let Some(quarantine) = record.body.quarantined {
            breaks.push(ChainBreak {
                line: number,
                quarantine,
            });
        }
        prev = record.hash;
        records += 1;
        good_len += read as u64;
    }
    Ok(Scan {
        records,
        last: prev,
        good_len,
        torn: 0,
        breaks,
    })
}

/// Verifies the chain of the log at `path`. With `anchor`, the last record's hash must
/// also equal it (this catches a truncated tail). Quarantined torn lines and a torn
/// last line are reported, not refused: they are what a crash leaves, and the chain
/// around them is intact.
pub fn verify(path: &Path, anchor: Option<&str>) -> Result<VerifyReport, AuditError> {
    let file = File::open(path).map_err(|source| AuditError::Io {
        path: path.to_owned(),
        source,
    })?;
    let scan = scan(BufReader::new(file), path)?;
    if let Some(anchor) = anchor
        && anchor != scan.last
    {
        return Err(AuditError::Broken {
            line: scan.records,
            reason: "last record does not match the anchor (the log was truncated?)".into(),
        });
    }
    Ok(VerifyReport {
        records: scan.records,
        last: scan.last,
        breaks: scan.breaks,
        torn: scan.torn,
    })
}

/// Holds a shared lock of a log file until dropped: appends, which take the exclusive
/// lock, wait until it is released.
struct SharedLock<'a>(&'a File);

impl<'a> SharedLock<'a> {
    fn take(file: &'a File) -> std::io::Result<Self> {
        file.lock_shared()?;
        Ok(Self(file))
    }
}

impl Drop for SharedLock<'_> {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Writes the records of the log at `path` whose sequence number is above `since` to
/// `out`, one JSON line each, exactly as a sink receives them (a `note` replaced by its
/// `note_mac`, a quarantine record's hash and length of the torn bytes by their MAC):
/// for shipping elsewhere. The whole chain is verified first, and against `anchor`
/// when given, under a shared lock that holds appends back until the export is
/// written; nothing is written from a log that does not verify. Returns how many
/// records were written.
pub fn export(path: &Path, since: u64, anchor: Option<&str>, out: &mut dyn Write) -> Result<u64, AuditError> {
    let io = |source: std::io::Error| AuditError::Io {
        path: path.to_owned(),
        source,
    };
    let file = File::open(path).map_err(io)?;
    let _lock = SharedLock::take(&file).map_err(io)?;
    let checked = scan(BufReader::new(&file), path)?;
    if let Some(anchor) = anchor
        && anchor != checked.last
    {
        return Err(AuditError::Broken {
            line: checked.records,
            reason: "last record does not match the anchor (the log was truncated?)".into(),
        });
    }
    let mut reader = BufReader::new(&file);
    reader.seek(SeekFrom::Start(0)).map_err(io)?;
    let mut key: Option<[u8; 32]> = None;
    let mut written = 0u64;
    let mut line = String::new();
    for number in 1..=checked.records {
        line.clear();
        reader.read_line(&mut line).map_err(io)?;
        let text = line.trim_end_matches('\n');
        let record: Record = serde_json::from_str(text).map_err(|e| AuditError::Broken {
            line: number,
            reason: format!("not a record: {e}"),
        })?;
        if record.body.seq <= since {
            continue;
        }
        let shipped = if record.body.event.note.is_some() || record.body.quarantined.is_some() {
            let key = match key {
                Some(key) => key,
                None => {
                    let read = read_note_key(path).map_err(|source| AuditError::Io {
                        path: note_key_path(path),
                        source,
                    })?;
                    *key.insert(read)
                }
            };
            for_sinks(text, &key).map_err(|e| io(std::io::Error::other(e)))?
        } else {
            text.to_owned()
        };
        out.write_all(shipped.as_bytes())
            .and_then(|()| out.write_all(b"\n"))
            .map_err(io)?;
        written += 1;
    }
    out.flush().map_err(io)?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn event(n: u32) -> AuditEvent {
        AuditEvent {
            principal: "alice".into(),
            agent: "claude-code".into(),
            channel: "stdio".into(),
            tool: "kiwi_search".into(),
            snapshot: "abc".into(),
            namespace: "acme".into(),
            atoms: vec![n, n + 1],
            ..AuditEvent::default()
        }
    }

    /// A sink that keeps what it receives.
    struct Kept(std::sync::Arc<Mutex<Vec<String>>>);

    impl AuditSink for Kept {
        fn name(&self) -> &str {
            "kept"
        }
        fn deliver(&self, line: &str) -> kiwi_core::Result<()> {
            self.0.lock().unwrap().push(line.to_owned());
            Ok(())
        }
    }

    #[test]
    fn sinks_never_receive_a_note() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let kept = std::sync::Arc::new(Mutex::new(Vec::new()));
        let log = AuditLog::open(&path, vec![Box::new(Kept(std::sync::Arc::clone(&kept)))]).unwrap();
        let note = "refused: conflicts with finance/budget-2027.md (FIN-001)";
        let hash = log
            .record(AuditEvent {
                note: Some(note.into()),
                ..event(1)
            })
            .unwrap();
        let last = log.record(event(2)).unwrap();
        let local = std::fs::read_to_string(&path).unwrap();
        assert!(local.contains("finance/budget-2027.md"), "the local log keeps it");
        let shipped = kept.lock().unwrap().clone();
        assert_eq!(shipped.len(), 2);
        assert!(!shipped[0].contains("finance/budget-2027") && !shipped[0].contains("FIN-001"));
        let first: serde_json::Value = serde_json::from_str(&shipped[0]).unwrap();
        // A keyed MAC: matching it needs the key beside the local log, so a sink cannot
        // test a guess of the note.
        let key: [u8; 32] = hex::decode(std::fs::read_to_string(note_key_path(&path)).unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(first["note_mac"], note_mac(&key, note));
        assert_ne!(
            first["note_mac"],
            blake3::hash(note.as_bytes()).to_hex().to_string()
        );
        assert!(first.get("note_blake3").is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(note_key_path(&path))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "the key is its owner's only");
        }
        assert_eq!(
            first["hash"], hash,
            "the chain's hash still anchors the local log"
        );
        assert!(first.get("note").is_none());
        assert_eq!(
            shipped[1],
            local.lines().nth(1).unwrap(),
            "records without a note ship as written"
        );
        assert_eq!(verify(&path, Some(&last)).unwrap().records, 2);
        // The log opened again keeps its key, and refuses one others may read.
        drop(log);
        let again = AuditLog::open(&path, Vec::new()).unwrap();
        assert_eq!(again.note_key, key);
        drop(again);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(note_key_path(&path), std::fs::Permissions::from_mode(0o644)).unwrap();
            let refused = AuditLog::open(&path, Vec::new()).unwrap_err();
            assert!(refused.to_string().contains("readable by others"), "{refused}");
        }
    }

    #[test]
    fn chain_verifies_and_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let log = AuditLog::open(&path, Vec::new()).unwrap();
        log.record(event(1)).unwrap();
        let second = log.record(event(2)).unwrap();
        drop(log);
        let report = verify(&path, Some(&second)).unwrap();
        assert_eq!(report.records, 2);
        let log = AuditLog::open(&path, Vec::new()).unwrap();
        log.record(event(3)).unwrap();
        assert_eq!(verify(&path, None).unwrap().records, 3);
    }

    #[test]
    fn any_edit_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let log = AuditLog::open(&path, Vec::new()).unwrap();
        for n in 0..5 {
            log.record(event(n)).unwrap();
        }
        let anchor = verify(&path, None).unwrap().last;
        let original = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = original.lines().collect();
        for target in 0..lines.len() {
            let mut edited: Vec<String> = lines.iter().map(|l| (*l).to_owned()).collect();
            edited[target] = edited[target].replacen("\"alice\"", "\"mallory\"", 1);
            std::fs::write(&path, edited.join("\n") + "\n").unwrap();
            let err = verify(&path, None).unwrap_err();
            assert!(matches!(err, AuditError::Broken { line, .. } if line == target + 1));
        }
        // Deleting a middle line and truncating the tail.
        let mut dropped: Vec<&str> = lines.clone();
        dropped.remove(2);
        std::fs::write(&path, dropped.join("\n") + "\n").unwrap();
        assert!(verify(&path, None).is_err());
        std::fs::write(&path, lines[..4].join("\n") + "\n").unwrap();
        assert!(verify(&path, None).is_ok());
        assert!(verify(&path, Some(&anchor)).is_err());
        // A record without a namespace has no such field.
        let old = dir.path().join("old.log");
        let log = AuditLog::open(&old, Vec::new()).unwrap();
        log.record(AuditEvent {
            namespace: String::new(),
            ..event(1)
        })
        .unwrap();
        drop(log);
        assert!(!std::fs::read_to_string(&old).unwrap().contains("namespace"));
        assert!(verify(&old, None).is_ok());
        // Opening a tampered log refuses to extend it.
        std::fs::write(&path, original.replacen("\"alice\"", "\"eve\"", 1)).unwrap();
        assert!(AuditLog::open(&path, Vec::new()).is_err());
    }

    #[test]
    fn concurrent_writers_never_fork_the_chain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        // Each writer opens the log on its own, as separate server processes do: the
        // file lock, not the in-process mutex, keeps the chain linear.
        let writers: Vec<_> = (0..4)
            .map(|w| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let log = AuditLog::open(&path, Vec::new()).unwrap();
                    for n in 0..50 {
                        log.record(event(w * 100 + n)).unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        let report = verify(&path, None).unwrap();
        assert_eq!(report.records, 200);
        assert!(report.breaks.is_empty());
    }

    #[test]
    fn a_torn_last_line_is_quarantined_and_the_chain_continues() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let log = AuditLog::open(&path, Vec::new()).unwrap();
        log.record(event(1)).unwrap();
        log.record(event(2)).unwrap();
        drop(log);
        // A crash in the middle of the third write.
        let torn = b"{\"seq\":3,\"ts\":\"2026-";
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(torn).unwrap();
        drop(file);
        let report = verify(&path, None).unwrap();
        assert_eq!((report.records, report.torn), (2, torn.len() as u64));
        // Serving resumes: the torn bytes move aside and a break is recorded. A sink gets
        // the break with a keyed hash of the torn bytes' hash, never their hash or size.
        let kept_lines = std::sync::Arc::new(Mutex::new(Vec::new()));
        let log = AuditLog::open(&path, vec![Box::new(Kept(std::sync::Arc::clone(&kept_lines)))]).unwrap();
        let last = log.record(event(3)).unwrap();
        drop(log);
        let shipped = kept_lines.lock().unwrap().clone();
        assert_eq!(shipped.len(), 2, "the break and the record");
        let break_record: serde_json::Value = serde_json::from_str(&shipped[0]).unwrap();
        let quarantined = &break_record["quarantined"];
        assert!(quarantined.get("blake3").is_none() && quarantined.get("bytes").is_none());
        assert!(quarantined["mac"].as_str().is_some_and(|mac| mac.len() == 64));
        let report = verify(&path, Some(&last)).unwrap();
        assert_eq!(report.records, 4);
        assert_eq!(report.torn, 0);
        assert_eq!(report.breaks.len(), 1);
        let moved = &report.breaks[0];
        assert_eq!(moved.line, 3);
        assert_eq!(moved.quarantine.bytes, torn.len() as u64);
        let kept = std::fs::read(dir.path().join(&moved.quarantine.file)).unwrap();
        assert_eq!(kept, torn);
        assert_eq!(moved.quarantine.blake3, blake3::hash(torn).to_hex().to_string());
    }

    /// A log whose records carry none of the optional fields: tool records of every
    /// kind, a refused proposal's note and a quarantined torn line.
    const MINIMAL_LOG: &str = include_str!("../tests/fixtures/audit-minimal.log");
    const MINIMAL_LAST: &str = "a9c72f559b4de9fcbbc21b4fdaaf2474498fdc2677ca6745a6fbf477ff769d99";

    fn full_event() -> AuditEvent {
        AuditEvent {
            status: Some(AuditStatus::Ok),
            ids: vec!["product/fees#rules".into(), "product/fees".into()],
            docs: vec!["product/fees".into()],
            results: Some(2),
            tokens: Some(311),
            budget: Some(800),
            latency_ms: Some(4),
            queries: vec!["take rate".into()],
            ..event(7)
        }
    }

    #[test]
    fn a_log_of_minimal_records_verifies_and_grows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        std::fs::write(&path, MINIMAL_LOG).unwrap();
        let report = verify(&path, Some(MINIMAL_LAST)).unwrap();
        assert_eq!((report.records, report.breaks.len()), (10, 1));
        // Every record reads back with its optional fields empty and hashes as written.
        for line in MINIMAL_LOG.lines() {
            let record: Record = serde_json::from_str(line).unwrap();
            assert_eq!(hash_body(&record.body).unwrap(), record.hash);
            assert_eq!(record.body.event.status, None);
            assert!(record.body.event.ids.is_empty());
        }
        // A server extends it with records carrying every field.
        let log = AuditLog::open(&path, Vec::new()).unwrap();
        let last = log.record(full_event()).unwrap();
        drop(log);
        let report = verify(&path, Some(&last)).unwrap();
        assert_eq!(report.records, 11);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(MINIMAL_LOG), "earlier lines are never rewritten");
        let added: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(added["prev"], MINIMAL_LAST);
        assert_eq!(added["status"], "ok");
        assert_eq!(added["ids"][0], "product/fees#rules");
        assert_eq!(added["docs"], serde_json::json!(["product/fees"]));
        assert_eq!(
            (
                &added["results"],
                &added["tokens"],
                &added["budget"],
                &added["latency_ms"]
            ),
            (
                &serde_json::json!(2),
                &serde_json::json!(311),
                &serde_json::json!(800),
                &serde_json::json!(4)
            )
        );
        assert_eq!(added["queries"], serde_json::json!(["take rate"]));
        // Editing a new field is caught like any other edit.
        let forged = text.replacen("\"tokens\":311", "\"tokens\":3", 1);
        std::fs::write(&path, forged).unwrap();
        let error = verify(&path, None).unwrap_err();
        assert!(matches!(error, AuditError::Broken { line: 11, .. }), "{error}");
    }

    #[test]
    fn empty_new_fields_are_left_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let log = AuditLog::open(&path, Vec::new()).unwrap();
        log.record(event(1)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        for key in [
            "status",
            "ids",
            "docs",
            "results",
            "tokens",
            "budget",
            "latency_ms",
            "queries",
            "reference",
            "note",
        ] {
            assert!(!text.contains(&format!("\"{key}\"")), "{key}: {text}");
        }
        assert_eq!(
            serde_json::to_string(&AuditStatus::Refused).unwrap(),
            "\"refused\""
        );
    }

    #[test]
    fn export_ships_verified_records_after_a_sequence_number() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let kept = std::sync::Arc::new(Mutex::new(Vec::new()));
        let log = AuditLog::open(&path, vec![Box::new(Kept(std::sync::Arc::clone(&kept)))]).unwrap();
        log.record(event(1)).unwrap();
        log.record(AuditEvent {
            note: Some("refused: finance/budget-2027.md".into()),
            status: Some(AuditStatus::Refused),
            ..event(2)
        })
        .unwrap();
        let last = log.record(full_event()).unwrap();
        drop(log);
        let mut out = Vec::new();
        assert_eq!(export(&path, 1, Some(&last), &mut out).unwrap(), 2);
        let exported = String::from_utf8(out).unwrap();
        // Exactly what the sinks received, after record 1.
        let shipped = kept.lock().unwrap().clone();
        assert_eq!(exported, shipped[1..].join("\n") + "\n");
        assert!(!exported.contains("budget-2027"));
        let mut all = Vec::new();
        assert_eq!(export(&path, 0, None, &mut all).unwrap(), 3);
        let mut none = Vec::new();
        assert_eq!(export(&path, 3, None, &mut none).unwrap(), 0);
        assert!(none.is_empty());
        // A wrong anchor or an edited line: nothing is exported.
        let mut out = Vec::new();
        assert!(export(&path, 0, Some(GENESIS), &mut out).is_err());
        assert!(out.is_empty());
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replacen("\"alice\"", "\"eve\"", 1)).unwrap();
        assert!(export(&path, 2, None, &mut out).is_err());
        assert!(out.is_empty());
    }

    #[test]
    fn export_of_a_note_needs_the_key_and_nothing_else_does() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let log = AuditLog::open(&path, Vec::new()).unwrap();
        log.record(event(1)).unwrap();
        log.record(AuditEvent {
            note: Some("a note".into()),
            ..event(2)
        })
        .unwrap();
        log.record(event(3)).unwrap();
        drop(log);
        std::fs::remove_file(note_key_path(&path)).unwrap();
        let mut out = Vec::new();
        assert!(export(&path, 0, None, &mut out).is_err());
        let mut out = Vec::new();
        assert_eq!(export(&path, 2, None, &mut out).unwrap(), 1);
        assert!(!note_key_path(&path).exists(), "an export never makes a key");
    }

    #[test]
    fn a_torn_line_another_writer_left_is_quarantined_on_append() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let log = AuditLog::open(&path, Vec::new()).unwrap();
        log.record(event(1)).unwrap();
        // Another process crashed mid-write while this one was serving.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"seq\":2").unwrap();
        drop(file);
        log.record(event(2)).unwrap();
        let report = verify(&path, None).unwrap();
        assert_eq!(report.records, 3);
        assert_eq!(report.breaks.len(), 1);
        // A complete line that is not a record is tampering: never extended.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"forged\n").unwrap();
        drop(file);
        assert!(log.record(event(3)).is_err());
    }
}
