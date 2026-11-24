//! The built-in lint rules, run by `kiwi lint`, at build time and on every proposal.

use std::collections::{BTreeMap, BTreeSet};

use kiwi_core::model::{EdgeKind, NO_UID};
use kiwi_core::plugin::{Finding, Lint, Severity};
use kiwi_core::{Pack, text};

/// Broken links and references, missing owners, overdue reviews, terms defined twice,
/// contradictory decisions and amendments declared on one side only.
#[derive(Debug, Default, Clone, Copy)]
pub struct BuiltinLint;

fn finding(severity: Severity, rule: &str, id: &str, message: String) -> Finding {
    Finding {
        severity,
        rule: rule.to_owned(),
        id: id.to_owned(),
        message,
    }
}

impl Lint for BuiltinLint {
    fn name(&self) -> &str {
        "builtin"
    }

    fn check(&self, pack: &Pack) -> kiwi_core::Result<Vec<Finding>> {
        let data = pack.data();
        let mut out = Vec::new();
        for edge in data.edges.iter() {
            if edge.to.to_native() != NO_UID {
                continue;
            }
            let Some(kind) = EdgeKind::from_u8(edge.kind) else {
                continue;
            };
            let Some(atom) = pack.atom(edge.from.to_native()) else {
                continue;
            };
            match kind {
                EdgeKind::Code => {}
                EdgeKind::Mentions => out.push(finding(
                    Severity::Error,
                    "broken-link",
                    &atom.id,
                    format!("link to `{}` resolves to no document or section", edge.external),
                )),
                other => out.push(finding(
                    Severity::Error,
                    "broken-reference",
                    &atom.id,
                    format!("`{}: {}` names no document", other.name(), edge.external),
                )),
            }
        }

        let as_of = pack.as_of();
        let mut superseded_by: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for (index, doc) in data.docs.iter().enumerate() {
            let root = doc.root.to_native();
            let id = pack.atom(root).map(|a| a.id.as_str()).unwrap_or("");
            if doc.meta.owner.is_none() {
                out.push(finding(
                    Severity::Warning,
                    "missing-owner",
                    id,
                    "no `owner` in frontmatter and no `owner=` in KIWIOWNERS".into(),
                ));
            }
            if doc.meta.summary.is_none() {
                out.push(finding(
                    Severity::Warning,
                    "missing-summary",
                    id,
                    "no `summary` in frontmatter; L1 falls back to the first sentence".into(),
                ));
            }
            if let Some(review_by) = doc.meta.review_by.as_ref()
                && review_by.as_str() < as_of
            {
                out.push(finding(
                    Severity::Warning,
                    "review-overdue",
                    id,
                    format!("review was due {review_by} (snapshot date {as_of})"),
                ));
            }
            for edge in pack.edges_from(root) {
                if EdgeKind::from_u8(edge.kind) == Some(EdgeKind::Supersedes) && edge.to.to_native() != NO_UID
                {
                    superseded_by
                        .entry(edge.to.to_native())
                        .or_default()
                        .push(index as u32);
                }
            }
        }
        for (target, successors) in &superseded_by {
            let Some(old) = pack.doc_of(*target) else {
                continue;
            };
            let old_id = pack.atom(*target).map(|a| a.id.as_str()).unwrap_or("");
            if old.meta.status.as_str() != "deprecated" {
                out.push(finding(
                    Severity::Warning,
                    "conflicting-decisions",
                    old_id,
                    format!("superseded but still `{}`; mark it deprecated", old.meta.status),
                ));
            }
            let approved: Vec<&str> = successors
                .iter()
                .filter_map(|d| data.docs.get(*d as usize))
                .filter(|d| d.meta.status.as_str() == "approved")
                .map(|d| d.path.as_str())
                .collect();
            if approved.len() > 1 {
                out.push(finding(
                    Severity::Warning,
                    "conflicting-decisions",
                    old_id,
                    format!(
                        "superseded by several approved documents: {}",
                        approved.join(", ")
                    ),
                ));
            }
        }
        // `amends` and `amended_by` are two sides of one relation: each declared side
        // needs the other.
        for doc in data.docs.iter() {
            let root = doc.root.to_native();
            for edge in pack.edges_from(root) {
                let (kind, other) = match EdgeKind::from_u8(edge.kind) {
                    Some(EdgeKind::Amends) => (EdgeKind::AmendedBy, "amended_by"),
                    Some(EdgeKind::AmendedBy) => (EdgeKind::Amends, "amends"),
                    _ => continue,
                };
                let target = edge.to.to_native();
                if target == NO_UID {
                    continue;
                }
                let answered = pack
                    .edges_from(target)
                    .iter()
                    .any(|back| EdgeKind::from_u8(back.kind) == Some(kind) && back.to.to_native() == root);
                if !answered {
                    let id = pack.atom(root).map(|a| a.id.as_str()).unwrap_or("");
                    let target_id = pack.atom(target).map(|a| a.id.as_str()).unwrap_or("");
                    let declared = EdgeKind::from_u8(edge.kind).map_or("", EdgeKind::name);
                    out.push(finding(
                        Severity::Warning,
                        "amendment-unmatched",
                        id,
                        format!(
                            "`{declared}` names {target_id}, whose `{other}` does not name this document"
                        ),
                    ));
                }
            }
        }
        for doc in data.docs.iter() {
            let root = doc.root.to_native();
            if doc.meta.status.as_str() == "deprecated" && !superseded_by.contains_key(&root) {
                let id = pack.atom(root).map(|a| a.id.as_str()).unwrap_or("");
                out.push(finding(
                    Severity::Warning,
                    "deprecated-without-successor",
                    id,
                    "deprecated but nothing supersedes it".into(),
                ));
            }
        }

        let mut terms: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for entry in data.glossary.iter() {
            let id = pack
                .atom(entry.uid.to_native())
                .map(|a| a.id.as_str().to_owned())
                .unwrap_or_default();
            for name in std::iter::once(entry.term.as_str()).chain(entry.aliases.iter().map(|a| a.as_str())) {
                terms.entry(text::key(name)).or_default().insert(id.clone());
            }
        }
        // Each definition of a term defined twice gets the finding, so a change to
        // either one reports it.
        for (term, ids) in terms {
            if ids.len() > 1 {
                for id in &ids {
                    let others: Vec<&str> = ids.iter().filter(|o| *o != id).map(String::as_str).collect();
                    out.push(finding(
                        Severity::Error,
                        "duplicate-term",
                        id,
                        format!("`{term}` is also defined by {}", others.join(", ")),
                    ));
                }
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }
}
