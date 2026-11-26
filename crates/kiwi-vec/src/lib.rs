//! Kiwi semantic plugin: binary-quantized embeddings scanned with Hamming
//! distance, re-scored with int8 vectors. No vector database and no ANN index.
//!
//! Each atom's embedding is cut to 384 dimensions (Matryoshka truncation, then
//! re-normalised) and stored twice: as 384 sign bits (48 bytes) for the scan, and as
//! 384 int8 values with one scale for re-scoring. A query is embedded once, its bits are
//! compared with every *candidate* atom (never a hidden one), the closest are re-scored
//! with the int8 dot product, and the best are returned. Embeddings are reused across
//! builds by the hash of the exact text embedded (breadcrumb and body), so only atoms
//! whose text changed, a renamed heading above them included, are embedded again.
//!
//! An embedder that reaches the network (the OpenAI-compatible client) receives only the
//! atoms the policy lets it read; the others are stored as withheld and never match.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;

use kiwi_core::plugin::{Capabilities, Hit, IndexDoc, Indexer, Retriever, RetrieverFactory, Scope};
use kiwi_core::{KiwiError, Pack, PackBytes, text};
use serde::{Deserialize, Serialize};

pub mod embed;
pub mod hamming;
#[cfg(feature = "openai")]
pub mod openai;

pub use embed::{Embedder, EmbedderConfig, HashEmbedder, embedder_by_name, embedder_from_config};
#[cfg(feature = "openai")]
pub use openai::{OpenAiEmbedder, OpenAiSettings};

/// The segment name.
pub const SEGMENT: &str = "vec";
const VERSION: u32 = 3;
/// Dimensions kept per vector.
pub const DIMS: usize = 384;
/// Bytes of one binary code.
pub const CODE_BYTES: usize = DIMS / 8;
/// Characters of breadcrumb and body embedded per atom: the gist of a section, and a
/// bounded cost (encoder time grows with the square of the sequence length).
const EMBED_TEXT_CHARS: usize = 1_000;

fn plugin_err(error: impl std::fmt::Display) -> KiwiError {
    KiwiError::plugin(SEGMENT, error)
}

#[derive(Debug, Serialize, Deserialize)]
struct Header {
    embedder: String,
    count: u32,
    dims: u32,
}

/// Cuts a vector to [`DIMS`] and normalises it to unit length.
pub fn truncate_normalize(mut vector: Vec<f32>) -> Vec<f32> {
    vector.truncate(DIMS);
    vector.resize(DIMS, 0.0);
    let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in &mut vector {
            *v /= norm;
        }
    }
    vector
}

/// The sign bits of a vector.
pub fn binarize(vector: &[f32]) -> [u8; CODE_BYTES] {
    let mut code = [0u8; CODE_BYTES];
    for (i, v) in vector.iter().take(DIMS).enumerate() {
        if *v > 0.0 {
            code[i / 8] |= 1 << (i % 8);
        }
    }
    code
}

/// Symmetric int8 quantisation: the values and their scale.
pub fn quantize(vector: &[f32]) -> ([i8; DIMS], f32) {
    let max = vector.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    let scale = if max > 0.0 { max / 127.0 } else { 1.0 };
    let mut out = [0i8; DIMS];
    for (slot, v) in out.iter_mut().zip(vector) {
        *slot = (v / scale).round().clamp(-127.0, 127.0) as i8;
    }
    (out, scale)
}

struct Layout {
    count: usize,
    codes: usize,
    int8: usize,
    scales: usize,
    hashes: usize,
    flags: usize,
}

/// Flag of an atom the policy withheld from the embedder: it has no vector.
const WITHHELD: u8 = 1;

impl Layout {
    fn new(data_start: usize, count: usize) -> Self {
        let codes = data_start;
        let int8 = codes + count * CODE_BYTES;
        let scales = int8 + count * DIMS;
        let hashes = scales + count * 4;
        let flags = hashes + count * 32;
        Self {
            count,
            codes,
            int8,
            scales,
            hashes,
            flags,
        }
    }

    fn end(&self) -> usize {
        self.flags + self.count
    }
}

fn parse_header(bytes: &[u8]) -> kiwi_core::Result<(Header, Layout)> {
    if bytes.len() < 4 {
        return Err(plugin_err("segment too short"));
    }
    let len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let header: Header =
        serde_json::from_slice(bytes.get(4..4 + len).ok_or_else(|| plugin_err("bad header"))?)
            .map_err(plugin_err)?;
    if header.dims as usize != DIMS {
        return Err(plugin_err(format!("segment has {} dimensions", header.dims)));
    }
    let layout = Layout::new(4 + len, header.count as usize);
    if layout.end() != bytes.len() {
        return Err(plugin_err("segment size does not match its header"));
    }
    Ok((header, layout))
}

/// Builds the vector segment.
pub struct VecIndexer {
    embedder: Arc<dyn Embedder>,
}

impl std::fmt::Debug for VecIndexer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VecIndexer")
            .field("embedder", &self.embedder.name())
            .finish()
    }
}

impl VecIndexer {
    /// An indexer using `embedder`.
    pub fn new(embedder: Arc<dyn Embedder>) -> Self {
        Self { embedder }
    }
}

fn passage_text(doc: &IndexDoc<'_>) -> String {
    let text = format!("{}\n{}", doc.breadcrumb, doc.body);
    text::truncate_chars(&text, EMBED_TEXT_CHARS)
}

/// The reuse key of an atom's vectors: the hash of the text that was embedded.
fn passage_hash(passage: &str) -> [u8; 32] {
    *blake3::hash(passage.as_bytes()).as_bytes()
}

/// The previous segment's vectors, by passage hash, for reuse.
struct Previous<'a> {
    bytes: &'a [u8],
    layout: Layout,
    by_hash: HashMap<[u8; 32], usize>,
}

impl<'a> Previous<'a> {
    fn read(embedder: &str, previous: Option<(u32, &'a [u8])>) -> Option<Self> {
        let (VERSION, bytes) = previous? else {
            return None;
        };
        let (header, layout) = parse_header(bytes).ok()?;
        if header.embedder != embedder {
            return None;
        }
        let mut by_hash = HashMap::with_capacity(layout.count);
        for i in 0..layout.count {
            if bytes[layout.flags + i] & WITHHELD != 0 {
                continue;
            }
            let mut hash = [0u8; 32];
            hash.copy_from_slice(&bytes[layout.hashes + i * 32..layout.hashes + (i + 1) * 32]);
            by_hash.insert(hash, i);
        }
        Some(Self {
            bytes,
            layout,
            by_hash,
        })
    }

    /// Copies the vectors of the previous atom `from` into slot `to` of the new arrays.
    fn copy(&self, from: usize, to: usize, codes: &mut [u8], int8: &mut [u8], scales: &mut [u8]) {
        let (b, l) = (self.bytes, &self.layout);
        codes[to * CODE_BYTES..(to + 1) * CODE_BYTES]
            .copy_from_slice(&b[l.codes + from * CODE_BYTES..l.codes + (from + 1) * CODE_BYTES]);
        int8[to * DIMS..(to + 1) * DIMS]
            .copy_from_slice(&b[l.int8 + from * DIMS..l.int8 + (from + 1) * DIMS]);
        scales[to * 4..(to + 1) * 4].copy_from_slice(&b[l.scales + from * 4..l.scales + (from + 1) * 4]);
    }
}

impl Indexer for VecIndexer {
    fn name(&self) -> &str {
        SEGMENT
    }

    fn version(&self) -> u32 {
        VERSION
    }

    fn capabilities(&self) -> Capabilities {
        self.embedder.capabilities()
    }

    fn build(&self, docs: &[IndexDoc<'_>], previous: Option<(u32, &[u8])>) -> kiwi_core::Result<Vec<u8>> {
        let reuse = Previous::read(&self.embedder.name(), previous);
        let mut codes = vec![0u8; docs.len() * CODE_BYTES];
        let mut int8 = vec![0u8; docs.len() * DIMS];
        let mut scales = vec![0u8; docs.len() * 4];
        let mut hashes = Vec::with_capacity(docs.len() * 32);
        let mut flags = vec![0u8; docs.len()];
        let mut pending: Vec<usize> = Vec::new();
        for (i, doc) in docs.iter().enumerate() {
            if doc.uid as usize != i {
                return Err(plugin_err("atoms must be indexed in uid order"));
            }
            if doc.withheld {
                hashes.extend_from_slice(&[0u8; 32]);
                flags[i] = WITHHELD;
                continue;
            }
            let hash = passage_hash(&passage_text(doc));
            hashes.extend_from_slice(&hash);
            match reuse
                .as_ref()
                .and_then(|r| r.by_hash.get(&hash).map(|&from| (r, from)))
            {
                Some((previous, from)) => previous.copy(from, i, &mut codes, &mut int8, &mut scales),
                None => pending.push(i),
            }
        }
        for batch in pending.chunks(self.embedder.batch_size().max(1)) {
            let texts: Vec<String> = batch.iter().map(|&i| passage_text(&docs[i])).collect();
            let vectors = self.embedder.embed_passages(&texts)?;
            if vectors.len() != batch.len() {
                return Err(plugin_err("embedder returned the wrong number of vectors"));
            }
            for (&i, vector) in batch.iter().zip(vectors) {
                let vector = truncate_normalize(vector);
                codes[i * CODE_BYTES..(i + 1) * CODE_BYTES].copy_from_slice(&binarize(&vector));
                let (values, scale) = quantize(&vector);
                for (slot, v) in int8[i * DIMS..(i + 1) * DIMS].iter_mut().zip(values) {
                    *slot = v as u8;
                }
                scales[i * 4..(i + 1) * 4].copy_from_slice(&scale.to_le_bytes());
            }
        }
        let header = serde_json::to_vec(&Header {
            embedder: self.embedder.name(),
            count: docs.len() as u32,
            dims: DIMS as u32,
        })
        .map_err(plugin_err)?;
        let mut out = Vec::with_capacity(
            4 + header.len() + codes.len() + int8.len() + scales.len() + hashes.len() + flags.len(),
        );
        out.extend_from_slice(&(header.len() as u32).to_le_bytes());
        out.extend_from_slice(&header);
        out.extend_from_slice(&codes);
        out.extend_from_slice(&int8);
        out.extend_from_slice(&scales);
        out.extend_from_slice(&hashes);
        out.extend_from_slice(&flags);
        Ok(out)
    }
}

/// Opens the vector retriever from a pack, with the embedder the segment was built
/// with (named in its header), so queries and atoms always share one vector space.
#[derive(Debug, Clone)]
pub struct VecFactory {
    cache_dir: std::path::PathBuf,
    configured: Option<EmbedderConfig>,
}

impl VecFactory {
    /// A factory loading models from `cache_dir`.
    pub fn new(cache_dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            cache_dir: cache_dir.into(),
            configured: None,
        }
    }

    /// The configured embedder, which serves a segment built in its vector space (the
    /// only way to query an `openai` segment, whose client needs the configuration).
    pub fn with_configured(mut self, config: EmbedderConfig) -> Self {
        self.configured = Some(config);
        self
    }
}

impl RetrieverFactory for VecFactory {
    fn name(&self) -> &str {
        SEGMENT
    }

    fn open(&self, pack: &Pack) -> kiwi_core::Result<Option<Box<dyn Retriever>>> {
        let Some((version, bytes)) = pack.segment(SEGMENT) else {
            return Ok(None);
        };
        if version != VERSION {
            return Err(plugin_err(format!(
                "segment version {version}, expected {VERSION}"
            )));
        }
        let (header, _) = parse_header(&bytes)?;
        let embedder =
            embed::embedder_by_name(&header.embedder, self.configured.as_ref(), self.cache_dir.clone())?;
        Ok(Some(Box::new(VecRetriever::open(bytes, embedder)?)))
    }
}

/// The vector retriever.
pub struct VecRetriever {
    bytes: PackBytes,
    layout: Layout,
    embedder: Arc<dyn Embedder>,
}

impl std::fmt::Debug for VecRetriever {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VecRetriever")
            .field("atoms", &self.layout.count)
            .finish()
    }
}

impl VecRetriever {
    /// Opens a segment; the embedder must be the one the segment was built with.
    pub fn open(bytes: PackBytes, embedder: Arc<dyn Embedder>) -> kiwi_core::Result<Self> {
        let (header, layout) = parse_header(&bytes)?;
        if header.embedder != embedder.name() {
            return Err(plugin_err(format!(
                "segment built with `{}`, queries embedded with `{}`",
                header.embedder,
                embedder.name()
            )));
        }
        Ok(Self {
            bytes,
            layout,
            embedder,
        })
    }

    fn code(&self, uid: usize) -> &[u8] {
        &self.bytes[self.layout.codes + uid * CODE_BYTES..self.layout.codes + (uid + 1) * CODE_BYTES]
    }

    fn rescore(&self, uid: usize, query: &[f32]) -> f32 {
        let values = &self.bytes[self.layout.int8 + uid * DIMS..self.layout.int8 + (uid + 1) * DIMS];
        let s = &self.bytes[self.layout.scales + uid * 4..self.layout.scales + (uid + 1) * 4];
        let scale = f32::from_le_bytes([s[0], s[1], s[2], s[3]]);
        let dot: f32 = values
            .iter()
            .zip(query)
            .map(|(v, q)| f32::from(*v as i8) * q)
            .sum();
        dot * scale
    }

    /// The Hamming scan alone: the `k` candidates closest to a query code, as
    /// (distance, uid), closest first. Exposed for benchmarks.
    pub fn scan(
        &self,
        query: &[u8; CODE_BYTES],
        candidates: &roaring::RoaringBitmap,
        k: usize,
    ) -> Vec<(u32, u32)> {
        let mut heap: BinaryHeap<(u32, u32)> = BinaryHeap::with_capacity(k + 1);
        for uid in candidates.iter() {
            if uid as usize >= self.layout.count
                || self.bytes[self.layout.flags + uid as usize] & WITHHELD != 0
            {
                continue;
            }
            let distance = hamming::distance(query, self.code(uid as usize));
            if heap.len() < k {
                heap.push((distance, uid));
            } else if heap.peek().is_some_and(|&worst| (distance, uid) < worst) {
                heap.pop();
                heap.push((distance, uid));
            }
        }
        let mut out = heap.into_vec();
        out.sort_unstable();
        out
    }

    /// Embeds a query and returns its code (for benchmarks of the scan alone).
    pub fn query_code(&self, query: &str) -> kiwi_core::Result<[u8; CODE_BYTES]> {
        Ok(binarize(&truncate_normalize(self.embedder.embed_query(query)?)))
    }
}

impl VecRetriever {
    fn rank(&self, vector: Vec<f32>, scope: Scope<'_>, limit: usize) -> Vec<Hit> {
        let vector = truncate_normalize(vector);
        let code = binarize(&vector);
        let shortlist = self.scan(&code, scope.candidates, (limit * 4).max(64));
        let mut hits: Vec<Hit> = shortlist
            .into_iter()
            .map(|(_, uid)| Hit {
                uid,
                score: self.rescore(uid as usize, &vector),
            })
            .collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.uid.cmp(&b.uid)));
        hits.truncate(limit);
        hits
    }
}

impl Retriever for VecRetriever {
    fn name(&self) -> &str {
        SEGMENT
    }

    fn wants_expansions(&self) -> bool {
        false
    }

    fn search_batch(
        &self,
        queries: &[String],
        scope: Scope<'_>,
        limit: usize,
    ) -> kiwi_core::Result<Vec<Vec<Hit>>> {
        let texts: Vec<String> = queries.iter().filter(|q| !q.trim().is_empty()).cloned().collect();
        // Nothing in scope: no query is embedded (an embeddings API is not called).
        if texts.is_empty() || limit == 0 || scope.candidates.is_empty() {
            return Ok(Vec::new());
        }
        let vectors = self.embedder.embed_queries(&texts).inspect_err(|error| {
            tracing::warn!(%error, "query embedding failed; the search goes on without vectors");
        })?;
        Ok(vectors.into_iter().map(|v| self.rank(v, scope, limit)).collect())
    }

    fn search(&self, query: &str, scope: Scope<'_>, limit: usize) -> kiwi_core::Result<Vec<Hit>> {
        if query.trim().is_empty() || limit == 0 || scope.candidates.is_empty() {
            return Ok(Vec::new());
        }
        let vector = self.embedder.embed_query(query).inspect_err(|error| {
            tracing::warn!(%error, "query embedding failed; the search goes on without vectors");
        })?;
        Ok(self.rank(vector, scope, limit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roaring::RoaringBitmap;

    fn segment(texts: &[&str], embedder: Arc<dyn Embedder>, previous: Option<&[u8]>) -> Vec<u8> {
        let hashes: Vec<[u8; 32]> = texts
            .iter()
            .map(|t| *blake3::hash(t.as_bytes()).as_bytes())
            .collect();
        let docs: Vec<IndexDoc<'_>> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| IndexDoc {
                uid: i as u32,
                id: t,
                title: t,
                breadcrumb: t,
                summary: "",
                body: t,
                hash: &hashes[i],
                class: 0,
                withheld: false,
            })
            .collect();
        VecIndexer::new(embedder)
            .build(&docs, previous.map(|p| (VERSION, p)))
            .unwrap()
    }

    fn pack(segment: Vec<u8>) -> Pack {
        let data = kiwi_core::model::PackData {
            format: kiwi_core::model::PACK_FORMAT,
            segments: vec![kiwi_core::model::SegmentRecord {
                name: SEGMENT.into(),
                version: VERSION,
                data: segment,
            }],
            edge_offsets: vec![0],
            back_offsets: vec![0],
            ..Default::default()
        };
        Pack::from_bytes(&kiwi_core::pack::encode(&data).unwrap()).unwrap()
    }

    #[test]
    fn quantization_round_trip() {
        let v = truncate_normalize(vec![0.5, -0.25, 0.0, 1.0]);
        assert_eq!(v.len(), DIMS);
        let code = binarize(&v);
        assert_eq!(code[0] & 0b1111, 0b1001);
        let (q, scale) = quantize(&v);
        assert!((f32::from(q[3]) * scale - v[3]).abs() < 0.01);
    }

    #[test]
    fn finds_related_text_within_candidates() {
        let embedder: Arc<dyn Embedder> = Arc::new(HashEmbedder);
        let texts = [
            "idempotency keys prevent double charges on retries",
            "kafka event bus partitions and consumer groups",
            "double entry ledger for seller payouts",
        ];
        let pack = pack(segment(&texts, Arc::clone(&embedder), None));
        let retriever = VecFactory::new(std::env::temp_dir())
            .open(&pack)
            .unwrap()
            .unwrap();
        let all = RoaringBitmap::from_iter(0..3u32);
        let hits = retriever
            .search(
                "retries charge twice idempotency",
                Scope {
                    visible: &all,
                    candidates: &all,
                },
                3,
            )
            .unwrap();
        assert_eq!(hits[0].uid, 0);
        let some = RoaringBitmap::from_iter([1u32, 2]);
        let hits = retriever
            .search(
                "idempotency",
                Scope {
                    visible: &some,
                    candidates: &some,
                },
                3,
            )
            .unwrap();
        assert!(hits.iter().all(|h| some.contains(h.uid)));
    }

    struct Counting {
        inner: HashEmbedder,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl Embedder for Counting {
        fn name(&self) -> String {
            self.inner.name()
        }
        fn capabilities(&self) -> Capabilities {
            self.inner.capabilities()
        }
        fn embed_passages(&self, texts: &[String]) -> kiwi_core::Result<Vec<Vec<f32>>> {
            self.calls
                .fetch_add(texts.len(), std::sync::atomic::Ordering::SeqCst);
            self.inner.embed_passages(texts)
        }
        fn embed_query(&self, text: &str) -> kiwi_core::Result<Vec<f32>> {
            self.inner.embed_query(text)
        }
    }

    #[test]
    fn unchanged_atoms_are_not_embedded_again() {
        use std::sync::atomic::Ordering;
        let counting = Arc::new(Counting {
            inner: HashEmbedder,
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let first = segment(&["a b c", "d e f"], counting.clone(), None);
        assert_eq!(counting.calls.load(Ordering::SeqCst), 2);
        let second = segment(&["a b c", "d e f", "g h"], counting.clone(), Some(&first));
        assert_eq!(counting.calls.load(Ordering::SeqCst), 3);
        let fresh = segment(&["a b c", "d e f", "g h"], Arc::new(HashEmbedder), None);
        assert_eq!(second, fresh);
    }

    #[test]
    fn a_renamed_heading_above_an_atom_embeds_it_again() {
        use std::sync::atomic::Ordering;
        let counting = Arc::new(Counting {
            inner: HashEmbedder,
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        // The atom's own hash does not change; the breadcrumb it is embedded with does.
        let hash = [7u8; 32];
        let doc = |breadcrumb: &'static str| IndexDoc {
            uid: 0,
            id: "a#b",
            title: "B",
            breadcrumb,
            summary: "",
            body: "the same body",
            hash: &hash,
            class: 0,
            withheld: false,
        };
        let indexer = VecIndexer::new(counting.clone());
        let first = indexer.build(&[doc("Old title › B")], None).unwrap();
        let same = indexer
            .build(&[doc("Old title › B")], Some((VERSION, &first)))
            .unwrap();
        assert_eq!(counting.calls.load(Ordering::SeqCst), 1);
        assert_eq!(same, first);
        indexer
            .build(&[doc("New title › B")], Some((VERSION, &first)))
            .unwrap();
        assert_eq!(counting.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn nothing_in_scope_embeds_no_query() {
        let counting = Arc::new(CountingQueries::default());
        let texts = ["alpha beta", "gamma delta"];
        let pack = pack(segment(&texts, Arc::new(HashEmbedder), None));
        let (_, bytes) = pack.segment(SEGMENT).unwrap();
        let retriever = VecRetriever::open(bytes, counting.clone()).unwrap();
        let all = RoaringBitmap::from_iter(0..2u32);
        let none = RoaringBitmap::new();
        let empty = Scope {
            visible: &all,
            candidates: &none,
        };
        assert!(retriever.search("alpha", empty, 5).unwrap().is_empty());
        assert!(
            retriever
                .search_batch(&["alpha".to_owned()], empty, 5)
                .unwrap()
                .is_empty()
        );
        assert_eq!(counting.queries.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[derive(Default)]
    struct CountingQueries {
        queries: std::sync::atomic::AtomicUsize,
    }

    impl Embedder for CountingQueries {
        fn name(&self) -> String {
            HashEmbedder.name()
        }
        fn capabilities(&self) -> Capabilities {
            HashEmbedder.capabilities()
        }
        fn embed_passages(&self, texts: &[String]) -> kiwi_core::Result<Vec<Vec<f32>>> {
            HashEmbedder.embed_passages(texts)
        }
        fn embed_query(&self, text: &str) -> kiwi_core::Result<Vec<f32>> {
            self.queries.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            HashEmbedder.embed_query(text)
        }
        fn embed_queries(&self, texts: &[String]) -> kiwi_core::Result<Vec<Vec<f32>>> {
            self.queries
                .fetch_add(texts.len(), std::sync::atomic::Ordering::SeqCst);
            HashEmbedder.embed_queries(texts)
        }
    }

    #[test]
    fn withheld_atoms_are_never_embedded_nor_found() {
        let counting = Arc::new(Counting {
            inner: HashEmbedder,
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let texts = [
            "idempotency keys retries",
            "secret idempotency plan",
            "ledger payouts",
        ];
        let hashes: Vec<[u8; 32]> = texts
            .iter()
            .map(|t| *blake3::hash(t.as_bytes()).as_bytes())
            .collect();
        let docs: Vec<IndexDoc<'_>> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| IndexDoc {
                uid: i as u32,
                id: t,
                title: if i == 1 { "" } else { t },
                breadcrumb: if i == 1 { "" } else { t },
                summary: "",
                body: if i == 1 { "" } else { t },
                hash: &hashes[i],
                class: 0,
                withheld: i == 1,
            })
            .collect();
        let segment = VecIndexer::new(counting.clone()).build(&docs, None).unwrap();
        assert_eq!(counting.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        let pack = pack(segment.clone());
        let retriever = VecFactory::new(std::env::temp_dir())
            .open(&pack)
            .unwrap()
            .unwrap();
        let all = RoaringBitmap::from_iter(0..3u32);
        let hits = retriever
            .search(
                "secret idempotency plan",
                Scope {
                    visible: &all,
                    candidates: &all,
                },
                3,
            )
            .unwrap();
        assert!(hits.iter().all(|h| h.uid != 1), "{hits:?}");
        // A rebuild reuses the embedded atoms and embeds no withheld one.
        let again = VecIndexer::new(counting.clone())
            .build(&docs, Some((VERSION, &segment)))
            .unwrap();
        assert_eq!(again, segment);
        assert_eq!(counting.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
