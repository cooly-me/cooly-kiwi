//! The feedback loop: `kiwi_feedback` verdicts and searches without results become a
//! queue of knowledge gaps for writers.
//!
//! The file is bounded: when it grows past [`MAX_BYTES`], the oldest records are
//! dropped until it holds about half that. Appends from several processes take turns
//! through a lock file. A query is itself information (an agent writes it after reading
//! what its user may see), so the gap queue shows a writer only the queries of callers
//! whose view, as it was when they asked, the writer's own view covers; the others are
//! counted, not shown.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use kiwi_core::{date, text};
use serde::{Deserialize, Serialize};

/// A feedback verdict.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The atoms answered the question.
    Useful,
    /// The atoms were wrong or misleading.
    Wrong,
    /// The knowledge does not exist yet.
    Missing,
    /// Recorded by the server: a search returned nothing.
    NoResults,
}

/// One feedback line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackRecord {
    /// When.
    pub ts: String,
    /// Who.
    pub principal: String,
    /// Through which agent.
    pub agent: String,
    /// Over which channel (`stdio`, `http`); empty in records written before it was kept.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub channel: String,
    /// Snapshot.
    pub snapshot: String,
    /// The query or question.
    pub query: String,
    /// Visible atom ids concerned.
    pub ids: Vec<String>,
    /// Verdict.
    pub verdict: Verdict,
    /// Free-text note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The caller's view when it asked: the access chain of every permission class it
    /// could read. A writer is shown the query only if they may read every one of these
    /// chains. Absent in records written before it was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<Vec<Vec<kiwi_core::model::AccessDecl>>>,
}

/// An aggregated knowledge gap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Gap {
    /// The folded query.
    pub query: String,
    /// How many times it was reported or searched without results.
    pub count: usize,
    /// Last time seen.
    pub last_seen: String,
}

/// Past this size the file drops its oldest records.
pub const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// The gaps a reader may see, and how many reports were withheld from them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Gaps {
    /// Gaps, most frequent first.
    pub gaps: Vec<Gap>,
    /// Reports from callers who can see more than the reader, not shown.
    pub withheld: usize,
}

/// The feedback file, append-only up to its bound.
#[derive(Debug)]
pub struct FeedbackLog {
    path: PathBuf,
    lock: Mutex<()>,
}

impl FeedbackLog {
    /// A log at `path` (created on first write).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }

    /// The file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends a record, dropping the oldest ones when the file has grown past its
    /// bound.
    pub fn append(&self, record: FeedbackRecord) -> std::io::Result<()> {
        self.append_bounded(record, MAX_BYTES)
    }

    fn append_bounded(&self, mut record: FeedbackRecord, max: u64) -> std::io::Result<()> {
        record.ts = date::timestamp(date::now_unix());
        let mut line = serde_json::to_string(&record).map_err(std::io::Error::other)?;
        line.push('\n');
        let _guard = self
            .lock
            .lock()
            .map_err(|_| std::io::Error::other("feedback lock poisoned"))?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.path.with_extension("jsonl.lock"))?;
        lock.lock()?;
        let outcome = self.append_locked(&line, max);
        let _ = lock.unlock();
        outcome
    }

    fn append_locked(&self, line: &str, max: u64) -> std::io::Result<()> {
        let size = match std::fs::metadata(&self.path) {
            Ok(meta) => meta.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e),
        };
        if size + line.len() as u64 > max {
            // Keep the newest lines that fit in half the bound, then the new one.
            let text = std::fs::read_to_string(&self.path).unwrap_or_default();
            let mut kept: Vec<&str> = Vec::new();
            let mut bytes = line.len() as u64;
            for old in text.lines().rev() {
                bytes += old.len() as u64 + 1;
                if bytes > max / 2 {
                    break;
                }
                kept.push(old);
            }
            let mut out = String::with_capacity(bytes as usize);
            for old in kept.iter().rev() {
                out.push_str(old);
                out.push('\n');
            }
            out.push_str(line);
            let tmp = self
                .path
                .with_extension(format!("jsonl.tmp-{}", std::process::id()));
            std::fs::write(&tmp, out)?;
            return std::fs::rename(&tmp, &self.path);
        }
        let mut file = OpenOptions::new().create(true).append(true).open(&self.path)?;
        file.write_all(line.as_bytes())
    }

    /// Reads every record (malformed lines are skipped).
    pub fn records(&self) -> std::io::Result<Vec<FeedbackRecord>> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => Ok(text
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Knowledge gaps: `missing` verdicts and searches without results, grouped by
    /// folded query, most frequent first. Only reports `may_see` accepts are shown (a
    /// reader sees the queries of callers whose view the reader's covers); the others
    /// are counted in `withheld`.
    pub fn gaps(&self, mut may_see: impl FnMut(&FeedbackRecord) -> bool) -> std::io::Result<Gaps> {
        let mut groups: BTreeMap<String, Gap> = BTreeMap::new();
        let mut withheld = 0usize;
        for record in self.records()? {
            if !matches!(record.verdict, Verdict::Missing | Verdict::NoResults) {
                continue;
            }
            if !may_see(&record) {
                withheld += 1;
                continue;
            }
            let key = text::key(&record.query);
            let gap = groups.entry(key.clone()).or_insert(Gap {
                query: key,
                count: 0,
                last_seen: String::new(),
            });
            gap.count += 1;
            if record.ts > gap.last_seen {
                gap.last_seen = record.ts;
            }
        }
        let mut gaps: Vec<Gap> = groups.into_values().collect();
        gaps.sort_by(|a, b| b.count.cmp(&a.count).then(a.query.cmp(&b.query)));
        Ok(Gaps { gaps, withheld })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn record(principal: &str, query: &str) -> FeedbackRecord {
        FeedbackRecord {
            ts: String::new(),
            principal: principal.into(),
            agent: "claude-code".into(),
            channel: "stdio".into(),
            snapshot: "abc".into(),
            query: query.into(),
            ids: Vec::new(),
            verdict: Verdict::Missing,
            note: None,
            view: None,
        }
    }

    #[test]
    fn the_file_is_bounded_and_keeps_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        let log = FeedbackLog::new(dir.path().join("feedback.jsonl"));
        let max = 4_096;
        for n in 0..200 {
            log.append_bounded(record("alice", &format!("question number {n}")), max)
                .unwrap();
            assert!(std::fs::metadata(log.path()).unwrap().len() <= max);
        }
        let records = log.records().unwrap();
        assert!(records.len() < 200 && records.len() > 5);
        assert_eq!(records.last().unwrap().query, "question number 199");
    }

    #[test]
    fn gaps_withhold_queries_the_reader_may_not_see() {
        let dir = tempfile::tempdir().unwrap();
        let log = FeedbackLog::new(dir.path().join("feedback.jsonl"));
        log.append(record("carl", "office hours")).unwrap();
        log.append(record("fiona", "project halcyon budget")).unwrap();
        let shown = log.gaps(|r| r.principal == "carl").unwrap();
        assert_eq!(shown.withheld, 1);
        assert_eq!(shown.gaps.len(), 1);
        assert!(shown.gaps[0].query.contains("office"));
        assert!(!format!("{shown:?}").contains("halcyon"));
    }
}
