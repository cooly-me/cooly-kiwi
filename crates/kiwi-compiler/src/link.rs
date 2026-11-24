//! Linking: turning compiled documents into one pack — uids, permission classes,
//! facets, the exact-match dictionary, the typed graph and the plugin segments.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use kiwi_core::Pack;
use kiwi_core::model::{
    AccessDecl, AtomRecord, Body, ClassRecord, DocRecord, EdgeKind, EdgeRecord, FacetRecord, GlossaryEntry,
    KeyTarget, NO_UID, PACK_FORMAT, PackData, SegmentRecord, key_kind,
};
use kiwi_core::pack::encode_body;
use kiwi_core::plugin::{Capabilities, IndexDoc, Indexer};
use kiwi_core::text;
use roaring::RoaringBitmap;

use crate::compile::CompiledDoc;
use crate::error::{CompileError, Result};

/// The hash a network indexer sees for an atom it may not read.
const WITHHELD_HASH: [u8; 32] = [0; 32];

/// Settings of a link run that do not come from the documents.
#[derive(Debug, Clone)]
pub struct LinkInput<'a> {
    /// Corpus namespace.
    pub namespace: &'a str,
    /// Snapshot id.
    pub snapshot: &'a str,
    /// `git` or `dir`.
    pub source: &'a str,
    /// Snapshot date.
    pub as_of: &'a str,
    /// Configuration hash.
    pub config_hash: [u8; 32],
}

/// Decides whether a plugin may receive an atom with a given access chain.
pub type PluginGate<'a> = dyn Fn(&str, &Capabilities, &[AccessDecl]) -> bool + 'a;

fn serialize_bitmap(bitmap: &RoaringBitmap) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(bitmap.serialized_size());
    bitmap
        .serialize_into(&mut out)
        .map_err(|e| CompileError::Config(format!("bitmap: {e}")))?;
    Ok(out)
}

/// Links compiled documents (sorted by path) into pack data and runs the indexers.
///
/// An indexer that declares network access receives only the atoms `gate` lets it read;
/// the others arrive marked `withheld`, with empty text. Such an indexer needs a gate:
/// without the corpus policy there is no way to know what it may send.
///
/// `previous` is the corpus's previous pack: compressed bodies of unchanged atoms are
/// copied from it, and each indexer receives its previous segment to reuse work.
pub fn link(
    docs: &[CompiledDoc],
    input: &LinkInput<'_>,
    indexers: &[&dyn Indexer],
    previous: Option<&Pack>,
    gate: Option<&PluginGate<'_>>,
) -> Result<PackData> {
    let prior_bodies: HashMap<[u8; 32], u32> = previous
        .map(|pack| {
            pack.data()
                .atoms
                .iter()
                .enumerate()
                .map(|(uid, atom)| (atom.hash, uid as u32))
                .collect()
        })
        .unwrap_or_default();
    // Uids and documents.
    let mut doc_records = Vec::with_capacity(docs.len());
    let mut first_uid = Vec::with_capacity(docs.len());
    let mut next_uid = 0u32;
    for doc in docs {
        first_uid.push(next_uid);
        next_uid = next_uid
            .checked_add(doc.atoms.len() as u32)
            .ok_or_else(|| CompileError::Config("more than 4 billion atoms".into()))?;
    }
    let atom_count = next_uid as usize;

    // Classes: one per distinct access chain, in sorted order.
    let chains: BTreeSet<&Vec<AccessDecl>> = docs
        .iter()
        .flat_map(|d| d.atoms.iter().map(|a| &a.chain))
        .collect();
    let class_of: BTreeMap<&Vec<AccessDecl>, u32> = chains
        .iter()
        .enumerate()
        .map(|(i, chain)| (*chain, i as u32))
        .collect();
    let mut class_bitmaps = vec![RoaringBitmap::new(); class_of.len()];

    // Lookup tables for references.
    let mut doc_by_key: BTreeMap<String, u32> = BTreeMap::new();
    let mut atom_by_id: BTreeMap<String, u32> = BTreeMap::new();
    for (d, doc) in docs.iter().enumerate() {
        let root = first_uid[d];
        doc_by_key.insert(text::key(&doc.path), root);
        if let Some(id) = &doc.doc_id {
            doc_by_key.insert(text::key(id), root);
        }
        for (i, atom) in doc.atoms.iter().enumerate() {
            let uid = first_uid[d] + i as u32;
            atom_by_id.insert(text::key(&atom.id), uid);
            for alias in &atom.aliases {
                atom_by_id.insert(text::key(alias), uid);
            }
        }
    }

    let mut atoms = Vec::with_capacity(atom_count);
    let mut bodies = Vec::with_capacity(atom_count);
    // Each key keeps the kinds of each of its uids apart (an id of one atom may be the
    // title of another).
    let mut keys: BTreeMap<String, BTreeMap<u32, u8>> = BTreeMap::new();
    let mut add_key = |key: String, kind: u8, uid: u32| {
        if key.is_empty() {
            return;
        }
        *keys.entry(key).or_default().entry(uid).or_insert(0) |= kind;
    };
    let mut facets: BTreeMap<(String, String), RoaringBitmap> = BTreeMap::new();
    let mut edges: BTreeSet<EdgeRecord> = BTreeSet::new();
    let mut glossary = Vec::new();

    for (d, doc) in docs.iter().enumerate() {
        let first = first_uid[d];
        let root = first;
        doc_records.push(DocRecord {
            path: doc.path.clone(),
            doc_id: doc.doc_id.clone(),
            title: doc.title.clone(),
            file_hash: doc.file_hash,
            root,
            first,
            count: doc.atoms.len() as u32,
            meta: doc.meta.clone(),
        });
        let range = first..first + doc.atoms.len() as u32;
        let mut facet = |field: &str, value: &str| {
            facets
                .entry((field.to_owned(), text::key(value)))
                .or_default()
                .insert_range(range.clone());
        };
        facet("type", &doc.meta.doc_type);
        facet("status", &doc.meta.status);
        if let Some(owner) = &doc.meta.owner {
            facet("owner", owner);
        }
        for tag in &doc.meta.tags {
            facet("tag", tag);
        }

        add_key(text::key(&doc.path), key_kind::DOC_ID, root);
        if let Some(id) = &doc.doc_id {
            add_key(text::key(id), key_kind::DOC_ID, root);
        }
        add_key(text::key(&doc.title), key_kind::TITLE, root);

        for (i, atom) in doc.atoms.iter().enumerate() {
            let uid = first + i as u32;
            let class = class_of
                .get(&atom.chain)
                .copied()
                .ok_or_else(|| CompileError::Config("class missing".into()))?;
            class_bitmaps[class as usize].insert(uid);
            add_key(text::key(&atom.id), key_kind::ATOM_ID, uid);
            for alias in &atom.aliases {
                add_key(text::key(alias), key_kind::ATOM_ID, uid);
            }
            if i > 0 {
                add_key(text::key(&atom.title), key_kind::HEADING, uid);
            }
            atoms.push(AtomRecord {
                id: atom.id.clone(),
                anchor: atom.anchor.clone(),
                doc: d as u32,
                parent: atom.parent.map(|p| first + p as u32).unwrap_or(NO_UID),
                depth: atom.depth,
                hash: atom.hash,
                title: atom.title.clone(),
                l0: atom.l0.clone(),
                l1: atom.l1.clone(),
                tokens: atom.tokens,
                class,
                aliases: atom.aliases.clone(),
                line: atom.line,
            });
            // An unchanged atom (same content hash) keeps its compressed body.
            let reused = prior_bodies
                .get(&atom.hash)
                .and_then(|&uid| previous.and_then(|pack| pack.data().bodies.get(uid as usize)))
                .filter(|body| body.raw_len.to_native() as usize == atom.body.len())
                .map(|body| Body {
                    raw_len: body.raw_len.to_native(),
                    compressed: body.compressed,
                    data: body.data.as_slice().to_vec(),
                });
            bodies.push(reused.unwrap_or_else(|| encode_body(&atom.body)));

            // Mentions: relative links and document ids written in the text.
            for link in &atom.links {
                if let Some(missing) = link.strip_prefix("missing:") {
                    edges.insert(EdgeRecord {
                        from: uid,
                        kind: EdgeKind::Mentions.as_u8(),
                        to: NO_UID,
                        external: missing.to_owned(),
                    });
                    continue;
                }
                let target = atom_by_id
                    .get(&text::key(link))
                    .or_else(|| doc_by_key.get(&text::key(link)))
                    .copied();
                match target {
                    Some(to) if !range.contains(&to) => {
                        edges.insert(EdgeRecord {
                            from: uid,
                            kind: EdgeKind::Mentions.as_u8(),
                            to,
                            external: String::new(),
                        });
                    }
                    Some(_) => {}
                    None => {
                        edges.insert(EdgeRecord {
                            from: uid,
                            kind: EdgeKind::Mentions.as_u8(),
                            to: NO_UID,
                            external: link.clone(),
                        });
                    }
                }
            }
            for word in &atom.id_words {
                if let Some(&to) = doc_by_key.get(word)
                    && !range.contains(&to)
                {
                    edges.insert(EdgeRecord {
                        from: uid,
                        kind: EdgeKind::Mentions.as_u8(),
                        to,
                        external: String::new(),
                    });
                }
            }
        }

        // Typed frontmatter edges leave the document root.
        let typed = [
            (EdgeKind::Implements, &doc.refs.implements),
            (EdgeKind::Supersedes, &doc.refs.supersedes),
            (EdgeKind::DecidedBy, &doc.refs.decided_by),
            (EdgeKind::DependsOn, &doc.refs.depends_on),
            (EdgeKind::Amends, &doc.refs.amends),
            (EdgeKind::AmendedBy, &doc.refs.amended_by),
        ];
        for (kind, targets) in typed {
            for target in targets {
                let to = doc_by_key.get(&text::key(target)).copied();
                edges.insert(EdgeRecord {
                    from: root,
                    kind: kind.as_u8(),
                    to: to.unwrap_or(NO_UID),
                    external: if to.is_some() {
                        String::new()
                    } else {
                        target.clone()
                    },
                });
            }
        }
        for glob in &doc.refs.code {
            edges.insert(EdgeRecord {
                from: root,
                kind: EdgeKind::Code.as_u8(),
                to: NO_UID,
                external: glob.clone(),
            });
        }
        for (term, aliases, index) in &doc.glossary {
            let uid = first + *index as u32;
            add_key(text::key(term), key_kind::TERM, uid);
            for alias in aliases {
                add_key(text::key(alias), key_kind::TERM, uid);
            }
            glossary.push(GlossaryEntry {
                term: term.clone(),
                aliases: aliases.clone(),
                uid,
            });
        }
    }
    glossary.sort_by(|a, b| {
        text::key(&a.term)
            .cmp(&text::key(&b.term))
            .then(a.uid.cmp(&b.uid))
    });

    // Edge adjacency.
    let edges: Vec<EdgeRecord> = edges.into_iter().collect();
    let mut edge_offsets = vec![0u32; atom_count + 1];
    for edge in &edges {
        edge_offsets[edge.from as usize + 1] += 1;
    }
    for i in 0..atom_count {
        edge_offsets[i + 1] += edge_offsets[i];
    }
    let mut back: Vec<(u32, u32, u32)> = edges
        .iter()
        .enumerate()
        .filter(|(_, e)| e.to != NO_UID)
        .map(|(i, e)| (e.to, e.from, i as u32))
        .collect();
    back.sort();
    let mut back_offsets = vec![0u32; atom_count + 1];
    for &(to, _, _) in &back {
        back_offsets[to as usize + 1] += 1;
    }
    for i in 0..atom_count {
        back_offsets[i + 1] += back_offsets[i];
    }
    let back_edges = back.into_iter().map(|(_, _, i)| i).collect();

    // Exact-match dictionary. An id names one atom: two atoms answering to the same
    // folded id, document id or path would let the answer depend on which of them the
    // caller can see.
    let id_kinds = key_kind::ATOM_ID | key_kind::DOC_ID;
    let mut builder = fst::MapBuilder::memory();
    let mut key_targets = Vec::with_capacity(keys.len());
    for (key, targets) in keys {
        let named: Vec<u32> = targets
            .iter()
            .filter(|(_, kinds)| **kinds & id_kinds != 0)
            .map(|(uid, _)| *uid)
            .collect();
        if let [first, second, ..] = named.as_slice() {
            let name = |uid: u32| atoms.get(uid as usize).map_or("", |a: &AtomRecord| a.id.as_str());
            let doc = atoms
                .get(*second as usize)
                .and_then(|a| docs.get(a.doc as usize))
                .map_or(String::new(), |d| format!("{}.md", d.path));
            return Err(CompileError::Document {
                path: doc,
                line: 1,
                message: format!(
                    "`{key}` names both `{}` and `{}`; ids, document ids and paths must be unique, ignoring case",
                    name(*first),
                    name(*second)
                ),
            });
        }
        builder
            .insert(&key, key_targets.len() as u64)
            .map_err(|e| CompileError::Config(format!("key dictionary: {e}")))?;
        let (uids, kinds) = targets.into_iter().unzip();
        key_targets.push(KeyTarget { uids, kinds });
    }
    let keys_fst = builder
        .into_inner()
        .map_err(|e| CompileError::Config(format!("key dictionary: {e}")))?;

    let mut classes = Vec::with_capacity(class_bitmaps.len());
    for (chain, bitmap) in chains.iter().zip(&class_bitmaps) {
        classes.push(ClassRecord {
            chain: (*chain).clone(),
            bitmap: serialize_bitmap(bitmap)?,
        });
    }
    let mut facet_records = Vec::with_capacity(facets.len());
    for ((field, value), bitmap) in facets {
        facet_records.push(FacetRecord {
            field,
            value,
            bitmap: serialize_bitmap(&bitmap)?,
        });
    }

    // Plugin segments.
    let mut index_docs = Vec::with_capacity(atom_count);
    for (d, doc) in docs.iter().enumerate() {
        for (i, atom) in doc.atoms.iter().enumerate() {
            let uid = first_uid[d] + i as u32;
            index_docs.push(IndexDoc {
                uid,
                id: &atom.id,
                title: &atom.title,
                breadcrumb: &atom.l0,
                summary: &atom.l1,
                body: &atom.plain,
                hash: &atom.hash,
                class: atoms[uid as usize].class,
                withheld: false,
            });
        }
    }
    let chains: Vec<&[AccessDecl]> = docs
        .iter()
        .flat_map(|doc| doc.atoms.iter().map(|atom| atom.chain.as_slice()))
        .collect();
    let mut segments = Vec::new();
    for indexer in indexers {
        let prior = previous.and_then(|pack| {
            pack.data()
                .segments
                .iter()
                .find(|segment| segment.name.as_str() == indexer.name())
                .map(|segment| (segment.version.to_native(), segment.data.as_slice()))
        });
        let caps = indexer.capabilities();
        let data = if caps.network.is_empty() {
            indexer.build(&index_docs, prior)?
        } else {
            let gate = gate.ok_or_else(|| {
                CompileError::Config(format!(
                    "plugin `{}` sends atom text to {}; the build needs the corpus policy to decide what it may send",
                    indexer.name(),
                    caps.network.join(", ")
                ))
            })?;
            let governed: Vec<IndexDoc<'_>> = index_docs
                .iter()
                .zip(&chains)
                .map(|(doc, chain)| {
                    if gate(indexer.name(), &caps, chain) {
                        *doc
                    } else {
                        // Nothing of a withheld atom reaches the plugin: no text,
                        // no id (a path and anchor), no hash.
                        IndexDoc {
                            id: "",
                            title: "",
                            breadcrumb: "",
                            summary: "",
                            body: "",
                            hash: &WITHHELD_HASH,
                            withheld: true,
                            ..*doc
                        }
                    }
                })
                .collect();
            indexer.build(&governed, prior)?
        };
        segments.push(SegmentRecord {
            name: indexer.name().to_owned(),
            version: indexer.version(),
            data,
        });
    }
    segments.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(PackData {
        format: PACK_FORMAT,
        namespace: input.namespace.to_owned(),
        snapshot: input.snapshot.to_owned(),
        source: input.source.to_owned(),
        as_of: input.as_of.to_owned(),
        kiwi_version: env!("CARGO_PKG_VERSION").to_owned(),
        config_hash: input.config_hash,
        docs: doc_records,
        atoms,
        bodies,
        classes,
        facets: facet_records,
        keys_fst,
        key_targets,
        edges,
        edge_offsets,
        back_edges,
        back_offsets,
        glossary,
        segments,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::sync::Arc;

    use kiwi_core::api::{Filters, SearchRequest};
    use kiwi_core::{Engine, EngineConfig, Pack, View};

    use super::*;
    use crate::KiwiConfig;
    use crate::compile::compile_doc;

    fn pack(sources: &[(&str, &str)]) -> Pack {
        let access = kiwi_policy::PathDefaults::access(AccessDecl::new("internal", Vec::new()));
        let config = KiwiConfig::default();
        let docs: Vec<CompiledDoc> = sources
            .iter()
            .map(|(path, source)| compile_doc(path, source, &access, &config).unwrap())
            .collect();
        let input = LinkInput {
            namespace: "test",
            snapshot: "0000000",
            source: "dir",
            as_of: "2026-11-24",
            config_hash: config.build_hash(),
        };
        let data = link(&docs, &input, &[], None, None).unwrap();
        Pack::from_bytes(&kiwi_core::pack::encode(&data).unwrap()).unwrap()
    }

    fn ids(engine: &Engine, view: &View, types: &[&str]) -> Vec<String> {
        let request = SearchRequest {
            filters: Filters {
                doc_type: types.iter().map(|t| (*t).to_owned()).collect(),
                ..Filters::default()
            },
            budget: Some(4000),
            ..SearchRequest::default()
        };
        let (response, _) = engine.search(view, &request).unwrap();
        let mut ids: Vec<String> = response.results.into_iter().map(|r| r.id).collect();
        ids.sort();
        ids
    }

    #[test]
    fn the_type_filter_keeps_standards_apart_from_decisions() {
        let body = "This sentence is long enough to keep the section as its own atom. ".repeat(6);
        let standard = format!(
            "---\nid: STD-001\ntype: standard\nstatus: approved\n---\n# Quality\n\n{body}\n\n## Code review {{#code-review}}\n\n{body}\n"
        );
        let adr = format!("---\nid: ADR-001\ntype: adr\nstatus: approved\n---\n# Postgres\n\n{body}\n");
        let prd = format!("---\nid: PRD-001\ntype: prd\nstatus: draft\n---\n# Checkout\n\n{body}\n");
        let pack = Arc::new(pack(&[
            ("adr/adr-001.md", &adr),
            ("product/prd.md", &prd),
            ("standards/quality.md", &standard),
        ]));
        let view = View::everything(&pack);
        let engine = Engine::new(Arc::clone(&pack), Vec::new(), EngineConfig::default());

        let standards = vec!["standards/quality", "standards/quality#code-review"];
        assert_eq!(ids(&engine, &view, &["standard"]), standards);
        assert_eq!(ids(&engine, &view, &["Standard"]), standards, "types are folded");
        assert_eq!(
            ids(&engine, &view, &["standard", "adr"]),
            vec![
                "adr/adr-001",
                "standards/quality",
                "standards/quality#code-review"
            ]
        );
        assert!(ids(&engine, &view, &["runbook"]).is_empty());
    }

    fn try_pack(sources: &[(&str, &str)]) -> Result<PackData> {
        let access = kiwi_policy::PathDefaults::access(AccessDecl::new("internal", Vec::new()));
        let config = KiwiConfig::default();
        let docs: Vec<CompiledDoc> = sources
            .iter()
            .map(|(path, source)| compile_doc(path, source, &access, &config).unwrap())
            .collect();
        let input = LinkInput {
            namespace: "test",
            snapshot: "0000000",
            source: "dir",
            as_of: "2026-11-24",
            config_hash: config.build_hash(),
        };
        link(&docs, &input, &[], None, None)
    }

    #[test]
    fn amendments_link_both_ways_lint_their_sides_and_stay_in_the_view() {
        use kiwi_core::api::{Direction, TraceRequest};
        use kiwi_core::plugin::Lint;

        let body = "This sentence is long enough to keep the section as its own atom. ".repeat(6);
        let old = format!(
            "---\nid: ADR-001\ntype: adr\nstatus: approved\namended_by: [ADR-002]\n---\n# Old\n\n{body}\n"
        );
        let new =
            format!("---\nid: ADR-002\ntype: adr\nstatus: approved\namends: ADR-001\n---\n# New\n\n{body}\n");
        let one_sided =
            format!("---\nid: ADR-003\ntype: adr\namends: [ADR-001, ADR-404]\n---\n# One-sided\n\n{body}\n");
        let secret = format!(
            "---\nid: ADR-009\ntype: adr\naccess: {{ label: secret }}\namends: [ADR-001]\n---\n# Secret amendment\n\n{body}\n"
        );
        let pack = Arc::new(pack(&[
            ("adr/old.md", &old),
            ("adr/new.md", &new),
            ("adr/one-sided.md", &one_sided),
            ("adr/secret.md", &secret),
        ]));
        let engine = Engine::new(Arc::clone(&pack), Vec::new(), EngineConfig::default());
        let edges = |view: &View, direction: Direction| -> Vec<(String, String, String)> {
            let request = TraceRequest {
                id: "ADR-001".into(),
                direction: Some(direction),
                depth: Some(1),
                ..TraceRequest::default()
            };
            let (response, _) = engine.trace(view, &request).unwrap();
            let mut edges: Vec<(String, String, String)> = response
                .edges
                .into_iter()
                .map(|e| (e.from, e.rel, e.to))
                .collect();
            edges.sort();
            edges
        };
        let edge = |from: &str, rel: &str, to: &str| (from.to_owned(), rel.to_owned(), to.to_owned());
        let everything = View::everything(&pack);
        assert_eq!(
            edges(&everything, Direction::Out),
            vec![edge("adr/old", "amended_by", "adr/new")]
        );
        assert_eq!(
            edges(&everything, Direction::In),
            vec![
                edge("adr/new", "amends", "adr/old"),
                edge("adr/one-sided", "amends", "adr/old"),
                edge("adr/secret", "amends", "adr/old"),
            ]
        );
        // A reader who may not see the secret amendment is never told of it.
        let open: Vec<u32> = (0..pack.class_count())
            .filter(|&class| {
                pack.data().classes[class as usize]
                    .chain
                    .iter()
                    .all(|decl| decl.label.as_str() != "secret")
            })
            .collect();
        let view = View::from_classes(&pack, open);
        let seen = edges(&view, Direction::Both);
        assert!(
            seen.iter()
                .all(|(from, _, to)| from != "adr/secret" && to != "adr/secret")
        );
        assert!(seen.contains(&edge("adr/new", "amends", "adr/old")));

        let findings = crate::lint::BuiltinLint.check(&pack).unwrap();
        let rules = |rule: &str| -> Vec<String> {
            let mut ids: Vec<String> = findings
                .iter()
                .filter(|f| f.rule == rule)
                .map(|f| f.id.clone())
                .collect();
            ids.sort();
            ids
        };
        assert_eq!(rules("broken-reference"), vec!["adr/one-sided"]);
        assert!(
            findings
                .iter()
                .any(|f| f.rule == "broken-reference" && f.message.contains("amends: ADR-404"))
        );
        assert_eq!(
            rules("amendment-unmatched"),
            vec!["adr/one-sided", "adr/secret"],
            "each amendment needs the other side; old and new agree"
        );
    }

    #[test]
    fn a_title_never_stands_in_for_an_id() {
        // `aaa/notes` sorts first, so its uid is lower than the ADR's: with one set of
        // kinds per key, its title `ADR-005` used to answer for the ADR's id.
        let pack = pack(&[
            ("aaa/notes.md", "---\ntitle: ADR-005\n---\nNotes about the ADR.\n"),
            ("adr/adr-005.md", "---\nid: ADR-005\n---\n# Idempotency keys\n"),
        ]);
        let uid = pack.resolve_id("ADR-005").unwrap().unwrap();
        assert_eq!(pack.atom(uid).unwrap().id.as_str(), "adr/adr-005");
        let targets = pack.lookup("adr-005").unwrap().unwrap();
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].1, key_kind::TITLE);
        assert_eq!(targets[1].1, key_kind::DOC_ID);
        assert_eq!(pack.resolve_id("Notes about").unwrap(), None);
    }

    #[test]
    fn one_id_names_one_atom() {
        // A document id equal to another document's path, and two paths that differ
        // only in case, would let the answer depend on which atom the caller sees.
        let err = try_pack(&[
            ("a/one.md", "---\nid: b/two\n---\n# One\n"),
            ("b/two.md", "# Two\n"),
        ])
        .unwrap_err();
        assert!(err.to_string().contains("names both"), "{err}");
        let err = try_pack(&[("Notes/x.md", "# X\n"), ("notes/x.md", "# X\n")]).unwrap_err();
        assert!(err.to_string().contains("names both"), "{err}");
        assert!(try_pack(&[("a.md", "---\nid: A-1\n---\n# A\n"), ("b.md", "# A-1\n")]).is_ok());
    }
}
