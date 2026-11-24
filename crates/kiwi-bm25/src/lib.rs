//! Kiwi lexical plugin: BM25 with postings stored in the Kiwi Pack.
//!
//! The segment is an rkyv archive read in place from the memory-mapped pack: a term
//! dictionary (an FST from term to ordinal), and for each term and field a posting list
//! of (atom uid, term frequency), **partitioned by permission class**. Three fields are
//! indexed per atom (title, breadcrumb, body) and scored with weights 3.0, 1.5 and 1.0.
//!
//! Governance is structural. A caller's view is a union of permission classes, so a
//! query reads only the partitions of classes in the view: postings of hidden atoms are
//! never touched, and in `strict` mode (the default) the statistics (atom count, average
//! field lengths, document frequencies) are sums over those partitions alone. A rare
//! word in a hidden section cannot shift a score, nor the work a query does.
//!
//! Builds are incremental per atom: each atom's analysis is keyed by its content and
//! breadcrumb, and an atom whose key is unchanged reuses its term counts from the
//! previous segment instead of being tokenised again.

#![cfg_attr(test, allow(clippy::unwrap_used))]

mod analyzer;
mod fold_table;
mod segment;

use std::cell::RefCell;
use std::collections::HashMap;

use kiwi_core::plugin::{Capabilities, Hit, IndexDoc, Indexer, Retriever, RetrieverFactory, Scope};
use kiwi_core::{KiwiError, Pack, PackBytes};
use roaring::RoaringBitmap;

use crate::analyzer::Analyzer;
pub use crate::segment::FIELDS;
use crate::segment::{ArchivedSegment, Draft, PreviousSegment};

/// The segment name.
pub const SEGMENT: &str = "bm25";
/// The segment format version.
pub const VERSION: u32 = 2;
/// Field weights: title, breadcrumb, body.
pub const FIELD_WEIGHTS: [f32; FIELDS] = [3.0, 1.5, 1.0];
const K1: f32 = 1.2;
const B: f32 = 0.75;
const MAX_QUERY_TERMS: usize = 32;

fn plugin_err(error: impl std::fmt::Display) -> KiwiError {
    KiwiError::plugin(SEGMENT, error)
}

/// Builds the BM25 segment.
#[derive(Debug, Default, Clone, Copy)]
pub struct Bm25Indexer;

impl Indexer for Bm25Indexer {
    fn name(&self) -> &str {
        SEGMENT
    }

    fn version(&self) -> u32 {
        VERSION
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            content: true,
            labels: false,
            network: Vec::new(),
        }
    }

    fn build(&self, docs: &[IndexDoc<'_>], previous: Option<(u32, &[u8])>) -> kiwi_core::Result<Vec<u8>> {
        let previous = match previous {
            Some((VERSION, bytes)) => PreviousSegment::read(bytes),
            _ => None,
        };
        let mut draft = Draft::new(docs.len());
        let mut analyzer = Analyzer::new();
        for (expected, doc) in docs.iter().enumerate() {
            if doc.uid as usize != expected {
                return Err(plugin_err("atoms must be indexed in uid order"));
            }
            if doc.withheld {
                draft.push_empty(doc.class);
                continue;
            }
            let key = segment::reuse_key(doc);
            if let Some(prior) = previous.as_ref().and_then(|p| p.atom(&key)) {
                draft.push_reused(key, doc.class, prior);
                continue;
            }
            draft.push_analysed(
                key,
                doc.class,
                &mut analyzer,
                [doc.title, doc.breadcrumb, doc.body],
            );
        }
        draft.finish()
    }
}

/// Opens the BM25 retriever from a pack.
#[derive(Debug, Clone, Copy)]
pub struct Bm25Factory {
    strict: bool,
}

impl Bm25Factory {
    /// `strict` computes statistics over the caller's visible atoms (recommended);
    /// otherwise over the whole corpus.
    pub fn new(strict: bool) -> Self {
        Self { strict }
    }
}

impl RetrieverFactory for Bm25Factory {
    fn name(&self) -> &str {
        SEGMENT
    }

    fn open(&self, pack: &Pack) -> kiwi_core::Result<Option<Box<dyn Retriever>>> {
        let Some((version, bytes)) = pack.segment(SEGMENT) else {
            return Ok(None);
        };
        if version != VERSION {
            return Err(plugin_err(format!(
                "segment version {version}, this kiwi reads {VERSION}; rebuild the pack with `kiwi build`"
            )));
        }
        let classes = (0..pack.class_count())
            .map(|class| pack.class_bitmap(class).cloned().unwrap_or_default())
            .collect();
        Ok(Some(Box::new(Bm25Retriever::open(
            bytes,
            classes,
            pack.atom_count(),
            self.strict,
        )?)))
    }
}

/// The BM25 retriever.
pub struct Bm25Retriever {
    bytes: PackBytes,
    classes: Vec<RoaringBitmap>,
    strict: bool,
}

impl std::fmt::Debug for Bm25Retriever {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bm25Retriever")
            .field("atoms", &self.segment().atoms.to_native())
            .finish()
    }
}

/// How much of one permission class a scope covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cover {
    /// Every atom of the class is visible.
    Full,
    /// Some are (a scope that is not a union of classes, such as in tests).
    Partial,
}

thread_local! {
    /// Score accumulator reused across queries on one thread; only touched slots are
    /// written, and they are reset before the next query.
    static SCRATCH: RefCell<Vec<f32>> = const { RefCell::new(Vec::new()) };
}

impl Bm25Retriever {
    /// Opens a segment. `classes` are the pack's permission-class bitmaps, by class id.
    pub fn open(
        bytes: PackBytes,
        classes: Vec<RoaringBitmap>,
        atom_count: u32,
        strict: bool,
    ) -> kiwi_core::Result<Self> {
        segment::validate(&bytes, classes.len(), atom_count)?;
        Ok(Self {
            bytes,
            classes,
            strict,
        })
    }

    fn segment(&self) -> &ArchivedSegment {
        // SAFETY: `open` validated these bytes with `rkyv::access` (and the structural
        // invariants the reads below rely on), and pack bytes are immutable.
        unsafe { segment::access_unchecked(&self.bytes) }
    }

    /// The classes `visible` touches, and how.
    fn cover(&self, visible: &RoaringBitmap) -> Vec<Option<Cover>> {
        self.classes
            .iter()
            .map(|class| {
                let shared = class.intersection_len(visible);
                if shared == 0 {
                    None
                } else if shared == class.len() {
                    Some(Cover::Full)
                } else {
                    Some(Cover::Partial)
                }
            })
            .collect()
    }

    fn query_terms(query: &str) -> Vec<String> {
        let mut analyzer = Analyzer::new();
        let mut words: Vec<String> = Vec::new();
        analyzer.terms(query, |term| {
            if words.len() < MAX_QUERY_TERMS && !words.iter().any(|w| w == term) {
                words.push(term.to_owned());
            }
        });
        words
    }
}

impl Retriever for Bm25Retriever {
    fn name(&self) -> &str {
        SEGMENT
    }

    fn search(&self, query: &str, scope: Scope<'_>, limit: usize) -> kiwi_core::Result<Vec<Hit>> {
        if limit == 0 || scope.candidates.is_empty() || scope.visible.is_empty() {
            return Ok(Vec::new());
        }
        let words = Self::query_terms(query);
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let segment = self.segment();
        let cover = self.cover(scope.visible);
        let lengths = segment.lengths.as_slice();

        // Collection statistics: over the visible classes in strict mode, else over all.
        let mut atoms = 0u64;
        let mut tokens = [0u64; FIELDS];
        for (class, stats) in segment.classes.iter().enumerate() {
            let counted = if self.strict {
                cover.get(class).copied().flatten()
            } else {
                Some(Cover::Full)
            };
            match counted {
                None => {}
                Some(Cover::Full) => {
                    atoms += u64::from(stats.atoms.to_native());
                    for (field, total) in tokens.iter_mut().enumerate() {
                        *total += stats.tokens[field].to_native();
                    }
                }
                Some(Cover::Partial) => {
                    for uid in self.classes[class]
                        .iter()
                        .filter(|uid| scope.visible.contains(*uid))
                    {
                        atoms += 1;
                        for (field, total) in tokens.iter_mut().enumerate() {
                            *total += u64::from(segment::length(lengths, uid, field));
                        }
                    }
                }
            }
        }
        if atoms == 0 {
            return Ok(Vec::new());
        }
        let average: [f32; FIELDS] = std::array::from_fn(|field| tokens[field] as f32 / atoms as f32);
        // Candidates equal the view unless a filter or a document scope narrowed them.
        let narrowed = scope.candidates.len() != scope.visible.len();

        SCRATCH.with(|scratch| {
            let mut scores = scratch.borrow_mut();
            let size = segment.atoms.to_native() as usize;
            if scores.len() < size {
                scores.resize(size, 0.0);
            }
            let mut touched: Vec<u32> = Vec::new();
            for word in &words {
                let Some(term) = segment::term_ordinal(segment, word)? else {
                    continue;
                };
                for field in 0..FIELDS {
                    let parts = segment::parts(segment, term, field);
                    let mut df = 0u64;
                    for part in parts {
                        let class = part.class.to_native() as usize;
                        let counted = if self.strict {
                            cover.get(class).copied().flatten()
                        } else {
                            Some(Cover::Full)
                        };
                        df += match counted {
                            None => 0,
                            Some(Cover::Full) => u64::from(part.len.to_native()),
                            Some(Cover::Partial) => segment::postings(segment, part)
                                .filter(|(uid, _)| scope.visible.contains(*uid))
                                .count() as u64,
                        };
                    }
                    if df == 0 {
                        continue;
                    }
                    let n = atoms as f32;
                    let idf = (1.0 + (n - df as f32 + 0.5) / (df as f32 + 0.5)).ln();
                    let weight = idf * (1.0 + K1) * FIELD_WEIGHTS[field];
                    let norm = K1 * B / average[field].max(f32::MIN_POSITIVE);
                    let base = K1 * (1.0 - B);
                    for part in parts {
                        let class = part.class.to_native() as usize;
                        let Some(reach) = cover.get(class).copied().flatten() else {
                            continue;
                        };
                        for (uid, tf) in segment::postings(segment, part) {
                            if (reach == Cover::Partial || narrowed) && !scope.candidates.contains(uid) {
                                continue;
                            }
                            let tf = tf as f32;
                            let dl = segment::length(lengths, uid, field) as f32;
                            let score = weight * tf / (tf + base + norm * dl);
                            if let Some(slot) = scores.get_mut(uid as usize) {
                                if *slot == 0.0 {
                                    touched.push(uid);
                                }
                                *slot += score;
                            }
                        }
                    }
                }
            }
            let mut hits: Vec<Hit> = touched
                .iter()
                .map(|&uid| Hit {
                    uid,
                    score: scores[uid as usize],
                })
                .collect();
            for &uid in &touched {
                scores[uid as usize] = 0.0;
            }
            let order = |a: &Hit, b: &Hit| b.score.total_cmp(&a.score).then(a.uid.cmp(&b.uid));
            if hits.len() > limit {
                hits.select_nth_unstable_by(limit, order);
                hits.truncate(limit);
            }
            hits.sort_by(order);
            Ok(hits)
        })
    }
}

/// Term counts of one atom, by field: `(term, count)` sorted by term.
pub(crate) type FieldCounts = [Vec<(String, u16)>; FIELDS];

/// Counts the terms of the three fields of one atom.
pub(crate) fn count_fields(analyzer: &mut Analyzer, texts: [&str; FIELDS]) -> (FieldCounts, [u32; FIELDS]) {
    let mut lengths = [0u32; FIELDS];
    let counts = std::array::from_fn(|field| {
        let mut counts: HashMap<String, u32> = HashMap::new();
        analyzer.terms(texts[field], |term| {
            lengths[field] = lengths[field].saturating_add(1);
            match counts.get_mut(term) {
                Some(count) => *count += 1,
                None => {
                    counts.insert(term.to_owned(), 1);
                }
            }
        });
        let mut sorted: Vec<(String, u16)> = counts
            .into_iter()
            .map(|(term, count)| (term, u16::try_from(count).unwrap_or(u16::MAX)))
            .collect();
        sorted.sort_unstable();
        sorted
    });
    (counts, lengths)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack_with(texts: &[(&str, &str)], classes: &[u32], previous: Option<&[u8]>) -> (Pack, Vec<u8>) {
        let hashes: Vec<[u8; 32]> = texts
            .iter()
            .map(|(t, b)| *blake3::hash(format!("{t}\0{b}").as_bytes()).as_bytes())
            .collect();
        let docs: Vec<IndexDoc<'_>> = texts
            .iter()
            .enumerate()
            .map(|(i, (title, body))| IndexDoc {
                uid: i as u32,
                id: title,
                title,
                breadcrumb: title,
                summary: "",
                body,
                hash: &hashes[i],
                class: classes[i],
                withheld: false,
            })
            .collect();
        let segment = Bm25Indexer.build(&docs, previous.map(|p| (VERSION, p))).unwrap();
        let class_count = classes.iter().max().map_or(0, |m| m + 1);
        let class_records = (0..class_count)
            .map(|c| {
                let bitmap = RoaringBitmap::from_iter(
                    classes
                        .iter()
                        .enumerate()
                        .filter(|(_, k)| **k == c)
                        .map(|(i, _)| i as u32),
                );
                let mut bytes = Vec::new();
                bitmap.serialize_into(&mut bytes).unwrap();
                kiwi_core::model::ClassRecord {
                    chain: Vec::new(),
                    bitmap: bytes,
                }
            })
            .collect();
        let atoms = texts
            .iter()
            .enumerate()
            .map(|(i, (title, _))| kiwi_core::model::AtomRecord {
                id: (*title).to_owned(),
                anchor: String::new(),
                doc: 0,
                parent: kiwi_core::model::NO_UID,
                depth: 0,
                hash: hashes[i],
                title: (*title).to_owned(),
                l0: (*title).to_owned(),
                l1: String::new(),
                tokens: kiwi_core::model::Tokens::default(),
                class: classes[i],
                aliases: Vec::new(),
                line: 1,
            })
            .collect();
        let data = kiwi_core::model::PackData {
            format: kiwi_core::model::PACK_FORMAT,
            atoms,
            bodies: texts
                .iter()
                .map(|(_, b)| kiwi_core::pack::encode_body(b))
                .collect(),
            classes: class_records,
            segments: vec![kiwi_core::model::SegmentRecord {
                name: SEGMENT.into(),
                version: VERSION,
                data: segment.clone(),
            }],
            edge_offsets: vec![0; texts.len() + 1],
            back_offsets: vec![0; texts.len() + 1],
            ..Default::default()
        };
        (
            Pack::from_bytes(&kiwi_core::pack::encode(&data).unwrap()).unwrap(),
            segment,
        )
    }

    fn open(texts: &[(&str, &str)], strict: bool) -> Box<dyn Retriever> {
        let classes: Vec<u32> = (0..texts.len() as u32).map(|i| u32::from(i == 3)).collect();
        let (pack, _) = pack_with(texts, &classes, None);
        Bm25Factory::new(strict).open(&pack).unwrap().unwrap()
    }

    const CORPUS: [(&str, &str); 4] = [
        (
            "Idempotency keys",
            "Every write API takes an idempotency key so retries never charge twice.",
        ),
        (
            "Saga",
            "The checkout saga captures payment asynchronously through Kafka.",
        ),
        ("Ledger", "The double-entry ledger records every payout."),
        (
            "Secret",
            "Project zephyr is a secret plan about payouts and payouts.",
        ),
    ];

    #[test]
    fn ranks_and_filters() {
        let retriever = open(&CORPUS, true);
        let all = RoaringBitmap::from_iter(0..4u32);
        let scope = Scope {
            visible: &all,
            candidates: &all,
        };
        let hits = retriever.search("charge twice retries", scope, 10).unwrap();
        assert_eq!(hits[0].uid, 0);
        let some = RoaringBitmap::from_iter([1u32, 2]);
        let scope = Scope {
            visible: &some,
            candidates: &some,
        };
        let hits = retriever.search("idempotency payouts", scope, 10).unwrap();
        assert!(hits.iter().all(|h| some.contains(h.uid)));
        assert_eq!(hits.len(), 1);
        let one = RoaringBitmap::from_iter([2u32]);
        let scope = Scope {
            visible: &all,
            candidates: &one,
        };
        let hits = retriever.search("payout saga kafka", scope, 10).unwrap();
        assert_eq!(hits.iter().map(|h| h.uid).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn strict_statistics_ignore_hidden_atoms() {
        // Scores for a caller who cannot see atom 3 (its own class) must equal the
        // scores on a corpus that does not contain atom 3 at all.
        let with_secret = open(&CORPUS, true);
        let without = open(&CORPUS[..3], true);
        let visible = RoaringBitmap::from_iter(0..3u32);
        let scope = Scope {
            visible: &visible,
            candidates: &visible,
        };
        for query in ["payouts ledger", "zephyr", "secret plan", "payment", "every"] {
            let a = with_secret.search(query, scope, 10).unwrap();
            let b = without.search(query, scope, 10).unwrap();
            assert_eq!(a, b, "query {query}");
        }
        let global = open(&CORPUS, false);
        let a = global.search("payouts ledger", scope, 10).unwrap();
        let b = without.search("payouts ledger", scope, 10).unwrap();
        assert_ne!(a, b, "global statistics do see hidden atoms");
    }

    #[test]
    fn partial_scopes_are_exact() {
        // A scope that is not a union of classes still counts only what it covers.
        let classes = [0u32, 0, 0, 0];
        let (pack, _) = pack_with(&CORPUS, &classes, None);
        let retriever = Bm25Factory::new(true).open(&pack).unwrap().unwrap();
        let without = open(&CORPUS[..3], true);
        let visible = RoaringBitmap::from_iter(0..3u32);
        let scope = Scope {
            visible: &visible,
            candidates: &visible,
        };
        for query in ["payouts ledger", "zephyr secret", "payment checkout"] {
            assert_eq!(
                retriever.search(query, scope, 10).unwrap(),
                without.search(query, scope, 10).unwrap(),
                "{query}"
            );
        }
    }

    #[test]
    fn scores_follow_bm25() {
        // "alpha" is in the title and the breadcrumb of atom 0, each one token long, as
        // every title and breadcrumb is: the tf factor is 1 / (1 + k1) in both fields.
        let corpus = [("alpha", "x"), ("beta", "y"), ("gamma", "z")];
        let retriever = open(&corpus, true);
        let all = RoaringBitmap::from_iter(0..3u32);
        let hits = retriever
            .search(
                "alpha",
                Scope {
                    visible: &all,
                    candidates: &all,
                },
                10,
            )
            .unwrap();
        let idf = (1.0f32 + (3.0 - 1.0 + 0.5) / (1.0 + 0.5)).ln();
        let expected = idf * (3.0 + 1.5);
        assert_eq!(hits.len(), 1);
        assert!(
            (hits[0].score - expected).abs() < 1e-5,
            "{} vs {expected}",
            hits[0].score
        );
    }

    #[test]
    fn unchanged_atoms_reuse_their_analysis() {
        let classes = [0u32, 0, 0, 0];
        let (_, first) = pack_with(&CORPUS, &classes, None);
        let (_, again) = pack_with(&CORPUS, &classes, Some(&first));
        assert_eq!(first, again, "a rebuild from the previous segment is identical");
        let mut changed = CORPUS;
        changed[1].1 = "The checkout saga now captures payment synchronously.";
        let (_, fresh) = pack_with(&changed, &classes, None);
        let (_, incremental) = pack_with(&changed, &classes, Some(&first));
        assert_eq!(
            fresh, incremental,
            "reuse gives the same segment as a full analysis"
        );
    }

    #[test]
    fn corrupt_segments_are_refused() {
        let classes = [0u32, 0, 0, 0];
        let (pack, segment) = pack_with(&CORPUS, &classes, None);
        let (_, bytes) = pack.segment(SEGMENT).unwrap();
        let mut broken = segment.clone();
        let len = broken.len();
        broken.truncate(len - 9);
        let data = kiwi_core::model::PackData {
            format: kiwi_core::model::PACK_FORMAT,
            segments: vec![kiwi_core::model::SegmentRecord {
                name: SEGMENT.into(),
                version: VERSION,
                data: broken,
            }],
            edge_offsets: vec![0],
            back_offsets: vec![0],
            ..Default::default()
        };
        let pack = Pack::from_bytes(&kiwi_core::pack::encode(&data).unwrap()).unwrap();
        let (_, broken_bytes) = pack.segment(SEGMENT).unwrap();
        assert!(Bm25Retriever::open(broken_bytes, Vec::new(), 0, true).is_err());
        // A segment that names classes or atoms the pack does not have.
        assert!(Bm25Retriever::open(bytes.clone(), Vec::new(), 4, true).is_err());
        assert!(Bm25Retriever::open(bytes, vec![RoaringBitmap::new()], 2, true).is_err());
    }
}
