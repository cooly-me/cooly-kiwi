//! Built-in exporters: `llms.txt` (an index of documents with their sections) and JSONL
//! (one atom per line). Both write only the atoms of a view.

use std::io::Write;

use roaring::RoaringBitmap;
use serde::Serialize;

use crate::error::{KiwiError, Result};
use crate::model::NO_UID;
use crate::pack::Pack;
use crate::plugin::Exporter;

fn io(e: std::io::Error) -> KiwiError {
    KiwiError::io("<export>", e)
}

/// Writes an `llms.txt`: a heading per visible document with its abstract, then a
/// bullet per visible section with its id and abstract.
pub fn llms_txt(pack: &Pack, visible: &RoaringBitmap, out: &mut dyn Write) -> Result<()> {
    writeln!(out, "# {}", pack.data().namespace).map_err(io)?;
    writeln!(out).map_err(io)?;
    writeln!(
        out,
        "> Knowledge snapshot {} ({} atoms visible).",
        pack.snapshot(),
        visible.len()
    )
    .map_err(io)?;
    for doc in pack.data().docs.iter() {
        let root = doc.root.to_native();
        if !visible.contains(root) {
            continue;
        }
        let Some(atom) = pack.atom(root) else { continue };
        writeln!(out).map_err(io)?;
        writeln!(out, "## {}", atom.l0).map_err(io)?;
        writeln!(out).map_err(io)?;
        if !atom.l1.is_empty() {
            writeln!(out, "{}", atom.l1).map_err(io)?;
            writeln!(out).map_err(io)?;
        }
        let first = doc.first.to_native();
        for uid in first..first + doc.count.to_native() {
            if uid == root || !visible.contains(uid) {
                continue;
            }
            let Some(section) = pack.atom(uid) else { continue };
            writeln!(
                out,
                "- [{}](kiwi://atom/{}@{}): {}",
                section.title,
                section.id,
                pack.snapshot(),
                section.l1
            )
            .map_err(io)?;
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct Line<'a> {
    id: &'a str,
    parent: Option<&'a str>,
    title: &'a str,
    summary: &'a str,
    text: String,
    #[serde(rename = "type")]
    doc_type: &'a str,
    status: &'a str,
    snapshot: &'a str,
}

/// Writes one JSON object per visible atom.
pub fn jsonl(pack: &Pack, visible: &RoaringBitmap, out: &mut dyn Write) -> Result<()> {
    for uid in visible.iter() {
        let (Some(atom), Some(doc)) = (pack.atom(uid), pack.doc_of(uid)) else {
            continue;
        };
        let parent = atom.parent.to_native();
        let line = Line {
            id: &atom.id,
            parent: (parent != NO_UID)
                .then(|| pack.atom(parent).map(|p| p.id.as_str()))
                .flatten(),
            title: &atom.l0,
            summary: &atom.l1,
            text: pack.body(uid)?,
            doc_type: &doc.meta.doc_type,
            status: &doc.meta.status,
            snapshot: pack.snapshot(),
        };
        let json = serde_json::to_string(&line).map_err(|e| KiwiError::InvalidInput(e.to_string()))?;
        writeln!(out, "{json}").map_err(io)?;
    }
    Ok(())
}

/// The `llms.txt` exporter.
#[derive(Debug, Default, Clone, Copy)]
pub struct LlmsTxt;

impl Exporter for LlmsTxt {
    fn name(&self) -> &str {
        "llms-txt"
    }

    fn export(&self, pack: &Pack, visible: &RoaringBitmap, out: &mut dyn Write) -> Result<()> {
        llms_txt(pack, visible, out)
    }
}

/// The JSONL exporter.
#[derive(Debug, Default, Clone, Copy)]
pub struct Jsonl;

impl Exporter for Jsonl {
    fn name(&self) -> &str {
        "jsonl"
    }

    fn export(&self, pack: &Pack, visible: &RoaringBitmap, out: &mut dyn Write) -> Result<()> {
        jsonl(pack, visible, out)
    }
}
