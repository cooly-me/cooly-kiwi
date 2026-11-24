//! The Kiwi Pack: one immutable, content-addressed file read through `mmap`.
//!
//! Layout: a 32-byte header (`KIWIPACK`, the format version, reserved bytes) followed by
//! an rkyv archive of [`PackData`]. The archive is validated once when the pack is
//! opened; every later read is zero-copy from the mapping.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::ops::{Deref, Range};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use memmap2::Mmap;
use roaring::RoaringBitmap;
use stable_deref_trait::StableDeref;

use crate::error::{KiwiError, Result};
use crate::model::{
    ArchivedAtomRecord, ArchivedDocRecord, ArchivedEdgeRecord, ArchivedPackData, Body, PACK_FORMAT, PackData,
};

const MAGIC: &[u8; 8] = b"KIWIPACK";
/// Size of the pack header; the rkyv archive starts at this (16-byte aligned) offset.
pub const HEADER_LEN: usize = 32;

enum Backing {
    Mmap(Mmap),
    Heap(rkyv::util::AlignedVec<16>),
}

impl Backing {
    fn bytes(&self) -> &[u8] {
        match self {
            Backing::Mmap(map) => map,
            Backing::Heap(vec) => vec,
        }
    }
}

/// A shared, immutable byte range of a pack. It derefs to `[u8]` and its address is
/// stable for its whole life, so plugins can hand it to zero-copy readers.
#[derive(Clone)]
pub struct PackBytes {
    backing: Arc<Backing>,
    range: Range<usize>,
}

impl PackBytes {
    /// A sub-range of these bytes (relative to this range).
    pub fn slice(&self, range: Range<usize>) -> Result<PackBytes> {
        if range.start > range.end || range.end > self.range.len() {
            return Err(KiwiError::Corrupt(format!(
                "slice {range:?} outside {} bytes",
                self.range.len()
            )));
        }
        Ok(PackBytes {
            backing: Arc::clone(&self.backing),
            range: self.range.start + range.start..self.range.start + range.end,
        })
    }

    /// The range of `inner` inside these bytes, when `inner` is borrowed from them.
    pub fn offset_of(&self, inner: &[u8]) -> Option<Range<usize>> {
        let base = self.deref().as_ptr() as usize;
        let start = (inner.as_ptr() as usize).checked_sub(base)?;
        let end = start.checked_add(inner.len())?;
        (end <= self.range.len()).then_some(start..end)
    }
}

impl Deref for PackBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.backing.bytes()[self.range.clone()]
    }
}

impl AsRef<[u8]> for PackBytes {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl std::fmt::Debug for PackBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PackBytes({} bytes)", self.range.len())
    }
}

// SAFETY: the bytes live in an `Arc`-owned memory map or heap buffer that is never
// mutated or reallocated, so the address `deref` returns is stable for as long as any
// clone of this handle exists, including after the handle itself is moved.
unsafe impl StableDeref for PackBytes {}

/// An opened, validated Kiwi Pack.
pub struct Pack {
    id: String,
    bytes: PackBytes,
    classes: Vec<RoaringBitmap>,
    facets: Vec<OnceLock<RoaringBitmap>>,
    all: RoaringBitmap,
}

impl std::fmt::Debug for Pack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pack")
            .field("snapshot", &self.snapshot())
            .field("atoms", &self.atom_count())
            .finish()
    }
}

impl Pack {
    /// Maps and validates the pack file at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let file = fs::File::open(path).map_err(|e| KiwiError::io(path, e))?;
        // SAFETY: a pack file is immutable once written (packs are content-addressed and
        // written through a temporary file and an atomic rename); the archive is fully
        // validated below before any read.
        let map = unsafe { Mmap::map(&file) }.map_err(|e| KiwiError::io(path, e))?;
        // The id is always the hash of the bytes, never the file name: response caches
        // and permission caches are keyed by it, so a file renamed over another pack's
        // name must not inherit that pack's entries. A stored pack is named by its hash,
        // and one whose bytes no longer match its name is refused.
        let id = content_id(&map);
        let named = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|stem| is_content_id(stem));
        if let Some(named) = named
            && named != id
        {
            return Err(KiwiError::Corrupt(format!(
                "{}: the pack's bytes do not match its name (hash {id})",
                path.display()
            )));
        }
        Self::from_backing(id, Backing::Mmap(map))
    }

    /// Loads and validates a pack held in memory.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let mut vec = rkyv::util::AlignedVec::<16>::with_capacity(bytes.len());
        vec.extend_from_slice(bytes);
        Self::from_backing(content_id(bytes), Backing::Heap(vec))
    }

    fn from_backing(id: String, backing: Backing) -> Result<Self> {
        let len = backing.bytes().len();
        let bytes = PackBytes {
            backing: Arc::new(backing),
            range: 0..len,
        };
        check_header(&bytes)?;
        let archived = rkyv::access::<ArchivedPackData, rkyv::rancor::Error>(&bytes[HEADER_LEN..])
            .map_err(|e| KiwiError::Corrupt(e.to_string()))?;
        if archived.format.to_native() != PACK_FORMAT {
            return Err(KiwiError::PackFormat(format!(
                "format {} (this kiwi reads {PACK_FORMAT})",
                archived.format.to_native()
            )));
        }
        let atom_count = archived.atoms.len();
        let mut classes = Vec::with_capacity(archived.classes.len());
        for class in archived.classes.iter() {
            let bitmap = RoaringBitmap::deserialize_from(class.bitmap.as_slice())
                .map_err(|e| KiwiError::Corrupt(format!("class bitmap: {e}")))?;
            if bitmap.max().is_some_and(|max| max as usize >= atom_count) {
                return Err(KiwiError::Corrupt("class bitmap names an unknown atom".into()));
            }
            classes.push(bitmap);
        }
        let check_offsets = |offsets: usize, name: &str| {
            if offsets != atom_count + 1 {
                Err(KiwiError::Corrupt(format!("{name} has {offsets} entries")))
            } else {
                Ok(())
            }
        };
        check_offsets(archived.edge_offsets.len(), "edge_offsets")?;
        check_offsets(archived.back_offsets.len(), "back_offsets")?;
        if archived.bodies.len() != atom_count {
            return Err(KiwiError::Corrupt("bodies and atoms differ in length".into()));
        }
        let facets = (0..archived.facets.len()).map(|_| OnceLock::new()).collect();
        let all = RoaringBitmap::from_iter(0..atom_count as u32);
        Ok(Self {
            id,
            bytes,
            classes,
            facets,
            all,
        })
    }

    /// The pack's content address: the BLAKE3 hash of its bytes. Two packs of one
    /// snapshot built differently (another embedder, `--as-of`) have different ids.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The archived pack data, read in place.
    pub fn data(&self) -> &ArchivedPackData {
        // SAFETY: the same bytes passed `rkyv::access` validation in `from_backing`, and
        // they are immutable for the life of `self`.
        unsafe { rkyv::access_unchecked::<ArchivedPackData>(&self.bytes[HEADER_LEN..]) }
    }

    /// The snapshot (commit hash) every answer from this pack is pinned to.
    pub fn snapshot(&self) -> &str {
        self.data().snapshot.as_str()
    }

    /// The snapshot date review deadlines are checked against.
    pub fn as_of(&self) -> &str {
        self.data().as_of.as_str()
    }

    /// Number of atoms.
    pub fn atom_count(&self) -> u32 {
        self.data().atoms.len() as u32
    }

    /// Every uid of the pack.
    pub fn all_atoms(&self) -> &RoaringBitmap {
        &self.all
    }

    /// The atom with `uid`.
    pub fn atom(&self, uid: u32) -> Option<&ArchivedAtomRecord> {
        self.data().atoms.get(uid as usize)
    }

    /// The document at `index`.
    pub fn doc(&self, index: u32) -> Option<&ArchivedDocRecord> {
        self.data().docs.get(index as usize)
    }

    /// The document owning atom `uid`.
    pub fn doc_of(&self, uid: u32) -> Option<&ArchivedDocRecord> {
        self.atom(uid).and_then(|atom| self.doc(atom.doc.to_native()))
    }

    /// Decompresses the L2 text of atom `uid`.
    pub fn body(&self, uid: u32) -> Result<String> {
        let body = self
            .data()
            .bodies
            .get(uid as usize)
            .ok_or_else(|| KiwiError::InvalidInput(format!("no atom {uid}")))?;
        let raw_len = body.raw_len.to_native() as usize;
        let bytes = if body.compressed {
            zstd::bulk::decompress(body.data.as_slice(), raw_len)
                .map_err(|e| KiwiError::Corrupt(format!("body {uid}: {e}")))?
        } else {
            body.data.as_slice().to_vec()
        };
        String::from_utf8(bytes).map_err(|e| KiwiError::Corrupt(format!("body {uid}: {e}")))
    }

    /// Number of permission classes.
    pub fn class_count(&self) -> u32 {
        self.classes.len() as u32
    }

    /// The uids of permission class `class`.
    pub fn class_bitmap(&self, class: u32) -> Option<&RoaringBitmap> {
        self.classes.get(class as usize)
    }

    /// The uids carrying a structural facet (`type`, `status`, `owner`, `tag`).
    pub fn facet(&self, field: &str, value: &str) -> Result<Option<&RoaringBitmap>> {
        let facets = &self.data().facets;
        let found = facets
            .binary_search_by(|facet| (facet.field.as_str(), facet.value.as_str()).cmp(&(field, value)));
        let Ok(index) = found else {
            return Ok(None);
        };
        if let Some(bitmap) = self.facets[index].get() {
            return Ok(Some(bitmap));
        }
        let bitmap = RoaringBitmap::deserialize_from(facets[index].bitmap.as_slice())
            .map_err(|e| KiwiError::Corrupt(format!("facet bitmap: {e}")))?;
        Ok(Some(self.facets[index].get_or_init(|| bitmap)))
    }

    /// Looks a folded key up in the exact-match dictionary: its target uids, ascending,
    /// each with its [`key_kind`](crate::model::key_kind) flags.
    pub fn lookup(&self, key: &str) -> Result<Option<Vec<(u32, u8)>>> {
        let data = self.data();
        let map = fst::Map::new(data.keys_fst.as_slice())
            .map_err(|e| KiwiError::Corrupt(format!("key dictionary: {e}")))?;
        let Some(index) = map.get(key) else {
            return Ok(None);
        };
        let target = data
            .key_targets
            .get(index as usize)
            .ok_or_else(|| KiwiError::Corrupt("key target out of range".into()))?;
        if target.kinds.len() != target.uids.len() {
            return Err(KiwiError::Corrupt(
                "key target kinds out of step with uids".into(),
            ));
        }
        Ok(Some(
            target
                .uids
                .iter()
                .zip(target.kinds.iter())
                .map(|(uid, kinds)| (uid.to_native(), *kinds))
                .collect(),
        ))
    }

    /// Resolves an atom id, alias, document id or path (exact, case-insensitive) to its
    /// uid. Only a uid the key names *as an id* answers: a title or heading that happens
    /// to equal an id never does. The compiler refuses a corpus where one id names two
    /// atoms, so the answer does not depend on who asks.
    pub fn resolve_id(&self, id: &str) -> Result<Option<u32>> {
        let key = crate::text::key(id);
        let id_kinds = crate::model::key_kind::ATOM_ID | crate::model::key_kind::DOC_ID;
        Ok(self
            .lookup(&key)?
            .and_then(|targets| targets.into_iter().find(|&(_, kinds)| kinds & id_kinds != 0))
            .map(|(uid, _)| uid))
    }

    /// Outgoing edges of atom `uid`.
    pub fn edges_from(&self, uid: u32) -> &[ArchivedEdgeRecord] {
        let data = self.data();
        let (Some(start), Some(end)) = (
            data.edge_offsets.get(uid as usize),
            data.edge_offsets.get(uid as usize + 1),
        ) else {
            return &[];
        };
        data.edges
            .get(start.to_native() as usize..end.to_native() as usize)
            .unwrap_or(&[])
    }

    /// Incoming edges of atom `uid`.
    pub fn edges_to(&self, uid: u32) -> Vec<&ArchivedEdgeRecord> {
        let data = self.data();
        let (Some(start), Some(end)) = (
            data.back_offsets.get(uid as usize),
            data.back_offsets.get(uid as usize + 1),
        ) else {
            return Vec::new();
        };
        let range = start.to_native() as usize..end.to_native() as usize;
        data.back_edges
            .get(range)
            .unwrap_or(&[])
            .iter()
            .filter_map(|index| data.edges.get(index.to_native() as usize))
            .collect()
    }

    /// The bytes of a plugin segment, as a zero-copy handle.
    pub fn segment(&self, name: &str) -> Option<(u32, PackBytes)> {
        let data = self.data();
        let segment = data
            .segments
            .iter()
            .find(|segment| segment.name.as_str() == name)?;
        let range = self.bytes.offset_of(segment.data.as_slice())?;
        let bytes = self.bytes.slice(range).ok()?;
        Some((segment.version.to_native(), bytes))
    }

    /// Names of the plugin segments in the pack.
    pub fn segment_names(&self) -> Vec<&str> {
        self.data()
            .segments
            .iter()
            .map(|segment| segment.name.as_str())
            .collect()
    }
}

fn check_header(bytes: &[u8]) -> Result<()> {
    if bytes.len() < HEADER_LEN || &bytes[..8] != MAGIC {
        return Err(KiwiError::PackFormat("missing KIWIPACK header".into()));
    }
    let mut version = [0u8; 4];
    version.copy_from_slice(&bytes[8..12]);
    let version = u32::from_le_bytes(version);
    if version != PACK_FORMAT {
        return Err(KiwiError::PackFormat(format!(
            "format {version} (this kiwi reads {PACK_FORMAT})"
        )));
    }
    Ok(())
}

/// Whether `name` has the form of a content address (64 lowercase hex digits).
fn is_content_id(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The content address of pack bytes: their BLAKE3 hash, computed on every core
/// (BLAKE3's tree mode), since a pack of 100,000 atoms is over 100 MB.
fn content_id(bytes: &[u8]) -> String {
    blake3::Hasher::new()
        .update_rayon(bytes)
        .finalize()
        .to_hex()
        .to_string()
}

/// Serialises pack data into the on-disk byte layout.
pub fn encode(data: &PackData) -> Result<Vec<u8>> {
    let archive = rkyv::to_bytes::<rkyv::rancor::Error>(data)
        .map_err(|e| KiwiError::Corrupt(format!("serialise: {e}")))?;
    let mut out = Vec::with_capacity(HEADER_LEN + archive.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&PACK_FORMAT.to_le_bytes());
    out.extend_from_slice(&[0u8; HEADER_LEN - 12]);
    out.extend_from_slice(&archive);
    Ok(out)
}

/// Compresses an L2 text for [`PackData::bodies`].
pub fn encode_body(text: &str) -> Body {
    let raw = text.as_bytes();
    match zstd::bulk::compress(raw, 3) {
        Ok(frame) if frame.len() < raw.len() => Body {
            raw_len: raw.len() as u32,
            compressed: true,
            data: frame,
        },
        _ => Body {
            raw_len: raw.len() as u32,
            compressed: false,
            data: raw.to_vec(),
        },
    }
}

/// The result of storing a pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPack {
    /// The pack file.
    pub path: PathBuf,
    /// BLAKE3 of the file, its content address.
    pub hash: String,
    /// False when an identical pack already existed and nothing was written.
    pub written: bool,
}

/// A directory of content-addressed packs with a `CURRENT` pointer and a snapshot index.
#[derive(Debug, Clone)]
pub struct PackStore {
    dir: PathBuf,
}

impl PackStore {
    /// A store rooted at `dir` (created on first write).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The store directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Takes the store's build lock, waiting while another build (in this process or
    /// another) holds it. Builds of one store run one at a time; readers never wait.
    pub fn lock(&self) -> Result<StoreLock> {
        fs::create_dir_all(&self.dir).map_err(|e| KiwiError::io(&self.dir, e))?;
        let path = self.dir.join(".lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| KiwiError::io(&path, e))?;
        file.lock().map_err(|e| KiwiError::io(&path, e))?;
        Ok(StoreLock { _file: file })
    }

    /// Writes `data` unless an identical pack exists, records its snapshot, and makes it
    /// the current pack.
    pub fn write(&self, data: &PackData) -> Result<StoredPack> {
        let bytes = encode(data)?;
        let hash = content_id(&bytes);
        ensure_ignored(&self.dir)?;
        let path = self.dir.join(format!("{hash}.kpack"));
        let written = if path.exists() {
            // An identical pack becomes current again: it is now the newest build, which
            // the retention of `gc` goes by.
            touch(&path)?;
            false
        } else {
            write_atomic(&path, &bytes)?;
            true
        };
        let mut index = self.index()?;
        index.insert(data.snapshot.clone(), hash.clone());
        let json = serde_json::to_vec_pretty(&index).map_err(|e| KiwiError::InvalidInput(e.to_string()))?;
        write_atomic(&self.dir.join("index.json"), &json)?;
        write_atomic(&self.dir.join("CURRENT"), format!("{hash}\n").as_bytes())?;
        Ok(StoredPack { path, hash, written })
    }

    /// The current pack file, if any build has completed.
    pub fn current(&self) -> Result<Option<PathBuf>> {
        let pointer = self.dir.join("CURRENT");
        match fs::read_to_string(&pointer) {
            Ok(hash) => Ok(Some(self.dir.join(format!("{}.kpack", hash.trim())))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(KiwiError::io(pointer, e)),
        }
    }

    /// The pack built for `snapshot` (a full or abbreviated commit hash), if kept.
    pub fn find_snapshot(&self, snapshot: &str) -> Result<Option<PathBuf>> {
        if snapshot.len() < 7 {
            return Ok(None);
        }
        let index = self.index()?;
        let mut matches = index
            .iter()
            .filter(|(snap, _)| snap.as_str() == snapshot || snap.starts_with(snapshot));
        match (matches.next(), matches.next()) {
            (Some((_, hash)), None) => Ok(Some(self.dir.join(format!("{hash}.kpack")))),
            _ => Ok(None),
        }
    }

    /// Removes old packs: the current pack is always kept, and so are the newest
    /// `keep` packs by build time (the current one counts as one of them), so at most
    /// `keep` packs remain once the current one is among the newest. Snapshots whose
    /// pack is removed leave the index, and temporary files a crashed build left behind
    /// for over an hour are removed. Runs under the store's build lock.
    ///
    /// A server still serving a removed pack keeps its memory map (on Unix the file
    /// lives until it is closed); a pinned read of a removed snapshot answers that the
    /// snapshot is not available.
    pub fn gc(&self, keep: usize) -> Result<GcReport> {
        let _lock = self.lock()?;
        let current = fs::read_to_string(self.dir.join("CURRENT"))
            .ok()
            .map(|hash| hash.trim().to_owned());
        let mut packs: Vec<(std::time::SystemTime, String, u64)> = Vec::new();
        let mut stale_temps = Vec::new();
        let hour_ago = std::time::SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(3600))
            .unwrap_or(std::time::UNIX_EPOCH);
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(GcReport::default()),
            Err(e) => return Err(KiwiError::io(&self.dir, e)),
        };
        for entry in entries {
            let entry = entry.map_err(|e| KiwiError::io(&self.dir, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let meta = entry.metadata().map_err(|e| KiwiError::io(entry.path(), e))?;
            if !meta.is_file() {
                continue;
            }
            let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            if let Some(hash) = name.strip_suffix(".kpack").filter(|h| is_content_id(h)) {
                packs.push((modified, hash.to_owned(), meta.len()));
            } else if name.contains(".tmp-") && modified < hour_ago {
                stale_temps.push(entry.path());
            }
        }
        // Newest first; ties by name, so the choice is deterministic.
        packs.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let keep = keep.max(1);
        let mut kept = 0usize;
        let mut report = GcReport::default();
        let mut removed = std::collections::BTreeSet::new();
        for (_, hash, len) in &packs {
            let is_current = current.as_deref() == Some(hash.as_str());
            if is_current || kept < keep.saturating_sub(usize::from(current.is_some())) {
                if !is_current {
                    kept += 1;
                }
                report.kept += 1;
                continue;
            }
            let path = self.dir.join(format!("{hash}.kpack"));
            fs::remove_file(&path).map_err(|e| KiwiError::io(&path, e))?;
            report.bytes += len;
            removed.insert(hash.clone());
        }
        for path in stale_temps {
            fs::remove_file(&path).map_err(|e| KiwiError::io(&path, e))?;
        }
        if !removed.is_empty() {
            let mut index = self.index()?;
            index.retain(|_, hash| !removed.contains(hash));
            let json =
                serde_json::to_vec_pretty(&index).map_err(|e| KiwiError::InvalidInput(e.to_string()))?;
            write_atomic(&self.dir.join("index.json"), &json)?;
        }
        report.removed = removed.into_iter().collect();
        Ok(report)
    }

    fn index(&self) -> Result<BTreeMap<String, String>> {
        let path = self.dir.join("index.json");
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| KiwiError::Corrupt(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(KiwiError::io(path, e)),
        }
    }
}

/// What [`PackStore::gc`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    /// Packs kept.
    pub kept: usize,
    /// Content addresses of the packs removed.
    pub removed: Vec<String>,
    /// Bytes freed.
    pub bytes: u64,
}

/// How many packs a store keeps by default: the current one and the nine before it.
pub const DEFAULT_KEEP_PACKS: usize = 10;

/// Sets a file's modification time to now.
fn touch(path: &Path) -> Result<()> {
    let file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| KiwiError::io(path, e))?;
    file.set_modified(std::time::SystemTime::now())
        .map_err(|e| KiwiError::io(path, e))
}

/// The build lock of a pack store, released when dropped.
#[derive(Debug)]
pub struct StoreLock {
    _file: fs::File,
}

/// Replaces a file through a temporary sibling and a rename (atomic, not synced), for
/// caches and pointers that can be rebuilt.
pub fn replace_file(path: &Path, bytes: &[u8]) -> Result<()> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp-{}-{unique}", std::process::id()));
    fs::write(&tmp, bytes).map_err(|e| KiwiError::io(&tmp, e))?;
    fs::rename(&tmp, path).map_err(|e| KiwiError::io(path, e))
}

/// Creates the directory `dir` Kiwi writes into (packs, state) with a `.gitignore` that
/// ignores everything in it, unless it has one: built packs, audit logs and their keys
/// never end up in the corpus's repository, and no file of the corpus is changed.
pub fn ensure_ignored(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).map_err(|e| KiwiError::io(dir, e))?;
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        write_atomic(&ignore, b"# Written by Kiwi: nothing here belongs in Git.\n*\n")?;
    }
    Ok(())
}

/// Writes a file through a temporary sibling, syncs it and renames it, so readers never
/// see a partial file and the content survives a crash.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = fs::File::create(&tmp).map_err(|e| KiwiError::io(&tmp, e))?;
    file.write_all(bytes).map_err(|e| KiwiError::io(&tmp, e))?;
    file.sync_all().map_err(|e| KiwiError::io(&tmp, e))?;
    drop(file);
    if let Err(error) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(KiwiError::io(path, error));
    }
    sync_dir(path)
}

/// Makes a rename in `path`'s directory durable (a no-op where directories cannot be
/// opened for syncing).
fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        let handle = fs::File::open(dir).map_err(|e| KiwiError::io(dir, e))?;
        handle.sync_all().map_err(|e| KiwiError::io(dir, e))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::model::{AtomRecord, ClassRecord, Tokens};

    fn sample() -> PackData {
        let mut bitmap = Vec::new();
        RoaringBitmap::from_iter([0u32])
            .serialize_into(&mut bitmap)
            .unwrap();
        PackData {
            format: PACK_FORMAT,
            snapshot: "abc1234def".into(),
            atoms: vec![AtomRecord {
                id: "doc".into(),
                anchor: String::new(),
                doc: 0,
                parent: crate::model::NO_UID,
                depth: 0,
                hash: [0; 32],
                title: "Doc".into(),
                l0: "Doc".into(),
                l1: "A doc.".into(),
                tokens: Tokens::default(),
                class: 0,
                aliases: vec![],
                line: 1,
            }],
            bodies: vec![encode_body(&"text ".repeat(100))],
            classes: vec![ClassRecord {
                chain: vec![],
                bitmap,
            }],
            edge_offsets: vec![0, 0],
            back_offsets: vec![0, 0],
            ..PackData::default()
        }
    }

    #[test]
    fn round_trips_through_bytes() {
        let bytes = encode(&sample()).unwrap();
        let pack = Pack::from_bytes(&bytes).unwrap();
        assert_eq!(pack.snapshot(), "abc1234def");
        assert_eq!(pack.atom_count(), 1);
        assert_eq!(pack.body(0).unwrap(), "text ".repeat(100));
        assert!(pack.body(0).is_ok());
        assert_eq!(pack.class_bitmap(0).unwrap().len(), 1);
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(
            Pack::from_bytes(b"not a pack at all, definitely not"),
            Err(KiwiError::PackFormat(_))
        ));
        let mut bytes = encode(&sample()).unwrap();
        let len = bytes.len();
        bytes.truncate(len - 7);
        assert!(Pack::from_bytes(&bytes).is_err());
    }

    #[test]
    fn builds_take_turns() {
        let dir = tempfile::tempdir().unwrap();
        let store = PackStore::new(dir.path());
        let held = store.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let other = store.clone();
        let waiter = std::thread::spawn(move || {
            let _lock = other.lock().unwrap();
            tx.send(()).unwrap();
        });
        assert!(rx.recv_timeout(std::time::Duration::from_millis(200)).is_err());
        drop(held);
        rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        waiter.join().unwrap();
    }

    #[test]
    fn a_pack_is_named_by_its_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let store = PackStore::new(dir.path());
        let stored = store.write(&sample()).unwrap();
        let pack = Pack::open(&stored.path).unwrap();
        assert_eq!(pack.id(), stored.hash);
        // Another pack's bytes copied under this pack's name are refused, and any other
        // file name gets the hash of the bytes.
        let mut other = sample();
        other.snapshot = "fedcba9876".into();
        let bytes = encode(&other).unwrap();
        std::fs::write(&stored.path, &bytes).unwrap();
        assert!(matches!(Pack::open(&stored.path), Err(KiwiError::Corrupt(_))));
        let loose = dir.path().join("loose.kpack");
        std::fs::write(&loose, &bytes).unwrap();
        assert_eq!(Pack::open(&loose).unwrap().id(), content_id(&bytes));
    }

    #[test]
    fn gc_keeps_the_current_pack_and_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        let store = PackStore::new(dir.path());
        let mut hashes = Vec::new();
        for n in 0..5u64 {
            let mut data = sample();
            data.snapshot = format!("snap{n:06}");
            let stored = store.write(&data).unwrap();
            // Distinct build times, oldest first.
            let file = fs::OpenOptions::new().write(true).open(&stored.path).unwrap();
            file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000 + n))
                .unwrap();
            hashes.push(stored.hash);
        }
        // The oldest build is made current again.
        std::fs::write(dir.path().join("CURRENT"), format!("{}\n", hashes[0])).unwrap();
        let report = store.gc(3).unwrap();
        assert_eq!(report.kept, 3);
        assert_eq!(report.removed.len(), 2);
        let exists = |h: &str| dir.path().join(format!("{h}.kpack")).exists();
        assert!(exists(&hashes[0]), "the current pack is kept");
        assert!(exists(&hashes[4]) && exists(&hashes[3]), "the newest are kept");
        assert!(!exists(&hashes[2]) && !exists(&hashes[1]));
        assert!(store.find_snapshot("snap000002").unwrap().is_none());
        assert!(store.find_snapshot("snap000004").unwrap().is_some());
        assert_eq!(store.gc(3).unwrap().removed.len(), 0);
    }

    #[test]
    fn store_is_content_addressed() {
        let dir = tempfile::tempdir().unwrap();
        let store = PackStore::new(dir.path());
        let first = store.write(&sample()).unwrap();
        assert!(first.written);
        let second = store.write(&sample()).unwrap();
        assert!(!second.written);
        assert_eq!(first.hash, second.hash);
        assert_eq!(store.current().unwrap(), Some(first.path.clone()));
        assert_eq!(store.find_snapshot("abc1234").unwrap(), Some(first.path));
        let pack = Pack::open(store.current().unwrap().unwrap()).unwrap();
        assert_eq!(pack.atom_count(), 1);
    }
}
