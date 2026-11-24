//! The BM25 segment: an rkyv archive of postings partitioned by permission class.
//!
//! - `terms`: an FST from term to ordinal; ordinals follow the terms' byte order.
//! - `lists[t * FIELDS + f] .. lists[t * FIELDS + f + 1]`: the partitions of term `t` in
//!   field `f`, one per permission class that holds the term there.
//! - A partition names its class, its number of postings and where they start in
//!   `postings`: pairs of LEB128 varints (the gap to the previous uid of the partition,
//!   then the term frequency), uids ascending.
//! - `lengths[uid * FIELDS + f]`: the token count of field `f` of each atom.
//! - `classes[c]`: atom count and token totals of class `c`, so collection statistics
//!   over a view are sums over its classes.
//! - `keys[uid]`: the reuse key of each atom's analysis, for incremental builds.

use std::collections::HashMap;

use kiwi_core::KiwiError;
use kiwi_core::plugin::IndexDoc;
use rkyv::{Archive, Archived, Deserialize, Serialize};

use crate::analyzer::{ANALYZER_VERSION, Analyzer};
use crate::{count_fields, plugin_err};

/// Indexed fields: title, breadcrumb, body.
pub const FIELDS: usize = 3;

/// One permission class's share of a posting list.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Part {
    /// The permission class.
    pub class: u32,
    /// Byte offset of its first posting in `postings`.
    pub start: u32,
    /// Number of postings.
    pub len: u32,
}

/// Totals of one permission class.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ClassStats {
    /// Atoms in the class.
    pub atoms: u32,
    /// Tokens per field over those atoms.
    pub tokens: [u64; FIELDS],
}

/// The archived segment.
#[derive(Archive, Serialize, Deserialize, Debug, Default)]
pub(crate) struct Segment {
    /// [`ANALYZER_VERSION`] at build time.
    pub analyzer: u32,
    /// Atoms indexed (every atom of the pack).
    pub atoms: u32,
    /// `fst::Map` from term to ordinal.
    pub terms: Vec<u8>,
    /// Partition ranges per (term, field); `terms * FIELDS + 1` entries.
    pub lists: Vec<u32>,
    /// Partitions.
    pub parts: Vec<Part>,
    /// The postings of every partition, as varint pairs.
    pub postings: Vec<u8>,
    /// Number of postings.
    pub count: u64,
    /// Field token counts per atom.
    pub lengths: Vec<u32>,
    /// Per-class totals.
    pub classes: Vec<ClassStats>,
    /// Reuse keys per atom.
    pub keys: Vec<[u8; 16]>,
}

/// The key of an atom with no analysis to reuse (one withheld from the indexer).
const NO_KEY: [u8; 16] = [0; 16];

/// The key under which an atom's analysis is reused: its content hash (which covers
/// the heading and body), its title and breadcrumb, and the analyzer version.
pub(crate) fn reuse_key(doc: &IndexDoc<'_>) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&ANALYZER_VERSION.to_le_bytes());
    hasher.update(doc.hash);
    for text in [doc.title, doc.breadcrumb] {
        hasher.update(&(text.len() as u64).to_le_bytes());
        hasher.update(text.as_bytes());
    }
    hasher.update(&(doc.body.len() as u64).to_le_bytes());
    let mut key = [0u8; 16];
    key.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    key
}

/// Accesses validated segment bytes without checking them again.
///
/// # Safety
/// `bytes` must have passed [`validate`].
pub(crate) unsafe fn access_unchecked(bytes: &[u8]) -> &ArchivedSegment {
    // SAFETY: the caller guarantees the bytes were validated.
    unsafe { rkyv::access_unchecked::<ArchivedSegment>(bytes) }
}

fn corrupt(message: impl std::fmt::Display) -> KiwiError {
    plugin_err(format!("corrupt segment: {message}"))
}

/// Validates segment bytes against the pack they belong to: the archive's structure,
/// the analyzer version, and every range and uid the query path reads.
pub(crate) fn validate(bytes: &[u8], pack_classes: usize, pack_atoms: u32) -> kiwi_core::Result<()> {
    let segment = rkyv::access::<ArchivedSegment, rkyv::rancor::Error>(bytes).map_err(corrupt)?;
    if segment.analyzer.to_native() != ANALYZER_VERSION {
        return Err(plugin_err(format!(
            "segment built with analyzer {}, this kiwi uses {ANALYZER_VERSION}; rebuild the pack",
            segment.analyzer.to_native()
        )));
    }
    let atoms = segment.atoms.to_native();
    if atoms != pack_atoms {
        return Err(corrupt(format!("{atoms} atoms in a pack of {pack_atoms}")));
    }
    if segment.lengths.len() != atoms as usize * FIELDS || segment.keys.len() != atoms as usize {
        return Err(corrupt("per-atom tables do not match the atom count"));
    }
    if segment.classes.len() > pack_classes {
        return Err(corrupt("more classes than the pack"));
    }
    let terms = fst::Map::new(segment.terms.as_slice()).map_err(corrupt)?;
    if segment.lists.len() != terms.len() * FIELDS + 1 {
        return Err(corrupt("list table does not match the term count"));
    }
    let mut previous = 0u32;
    for bound in segment.lists.iter() {
        let bound = bound.to_native();
        if bound < previous {
            return Err(corrupt("list table is not ordered"));
        }
        previous = bound;
    }
    if previous as usize != segment.parts.len() {
        return Err(corrupt("list table does not end at the partitions"));
    }
    // Each partition needs at least two bytes per posting. Decoding is bounds-checked and
    // stops at the first uid outside the pack, so a corrupt posting cannot reach past
    // the segment or name a missing atom.
    let mut postings = 0u64;
    for part in segment.parts.iter() {
        let end = u64::from(part.start.to_native()) + 2 * u64::from(part.len.to_native());
        if end > segment.postings.len() as u64 || part.class.to_native() as usize >= segment.classes.len() {
            return Err(corrupt("partition out of range"));
        }
        postings += u64::from(part.len.to_native());
    }
    if postings != segment.count.to_native() {
        return Err(corrupt("partitions do not add up to the postings"));
    }
    Ok(())
}

/// The ordinal of `term`, if indexed.
pub(crate) fn term_ordinal(segment: &ArchivedSegment, term: &str) -> kiwi_core::Result<Option<usize>> {
    let map = fst::Map::new(segment.terms.as_slice()).map_err(corrupt)?;
    Ok(map.get(term).map(|ordinal| ordinal as usize))
}

/// The partitions of a term in a field.
pub(crate) fn parts(segment: &ArchivedSegment, term: usize, field: usize) -> &[ArchivedPart] {
    let index = term * FIELDS + field;
    match (segment.lists.get(index), segment.lists.get(index + 1)) {
        (Some(start), Some(end)) => segment
            .parts
            .get(start.to_native() as usize..end.to_native() as usize)
            .unwrap_or(&[]),
        _ => &[],
    }
}

/// Appends `value` as an LEB128 varint.
fn put_varint(out: &mut Vec<u8>, mut value: u32) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// Reads an LEB128 varint at `*at`, advancing it; `None` past the end or on overflow.
#[inline]
fn get_varint(bytes: &[u8], at: &mut usize) -> Option<u32> {
    let mut value = 0u32;
    for shift in [0u32, 7, 14, 21, 28] {
        let byte = *bytes.get(*at)?;
        *at += 1;
        value |= u32::from(byte & 0x7f).checked_shl(shift)?;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// The postings of a partition: (uid, term frequency), uids ascending. Stops early on a
/// malformed posting or a uid of `atoms` or more.
pub(crate) fn postings<'a>(
    segment: &'a ArchivedSegment,
    part: &ArchivedPart,
) -> impl Iterator<Item = (u32, u32)> + 'a {
    let bytes = segment.postings.as_slice();
    let atoms = segment.atoms.to_native();
    let mut at = part.start.to_native() as usize;
    let mut uid = 0u32;
    let mut first = true;
    (0..part.len.to_native()).map_while(move |_| {
        let gap = get_varint(bytes, &mut at)?;
        let tf = get_varint(bytes, &mut at)?;
        uid = if first { gap } else { uid.checked_add(gap)? };
        first = false;
        (uid < atoms).then_some((uid, tf))
    })
}

/// The token count of field `field` of atom `uid`.
pub(crate) fn length(lengths: &[Archived<u32>], uid: u32, field: usize) -> u32 {
    lengths
        .get(uid as usize * FIELDS + field)
        .map_or(0, |length| length.to_native())
}

/// One posting of an atom: (term, field, frequency).
type Entry = (u32, u8, u16);

/// The previous segment, read back into per-atom term counts so unchanged atoms skip
/// analysis.
pub(crate) struct PreviousSegment {
    terms: Vec<String>,
    offsets: Vec<u32>,
    entries: Vec<Entry>,
    lengths: Vec<[u32; FIELDS]>,
    by_key: HashMap<[u8; 16], u32>,
}

/// An atom of the previous segment whose analysis can be reused.
pub(crate) struct PriorAtom<'a> {
    segment: &'a PreviousSegment,
    uid: u32,
}

impl PreviousSegment {
    /// Reads a previous segment; `None` when it is unreadable or from another analyzer
    /// (the build then analyses every atom).
    pub(crate) fn read(bytes: &[u8]) -> Option<Self> {
        let segment = rkyv::access::<ArchivedSegment, rkyv::rancor::Error>(bytes).ok()?;
        if segment.analyzer.to_native() != ANALYZER_VERSION {
            return None;
        }
        let atoms = segment.atoms.to_native() as usize;
        if segment.lengths.len() != atoms * FIELDS || segment.keys.len() != atoms {
            return None;
        }
        let map = fst::Map::new(segment.terms.as_slice()).ok()?;
        let mut terms = vec![String::new(); map.len()];
        let mut stream = map.stream();
        while let Some((term, ordinal)) = fst::Streamer::next(&mut stream) {
            *terms.get_mut(ordinal as usize)? = String::from_utf8(term.to_vec()).ok()?;
        }
        if segment.lists.len() != terms.len() * FIELDS + 1 {
            return None;
        }
        let mut counts = vec![0u32; atoms + 1];
        for part in segment.parts.iter() {
            for (uid, _) in postings(segment, part) {
                *counts.get_mut(uid as usize)? += 1;
            }
        }
        let mut offsets = Vec::with_capacity(atoms + 1);
        let mut total = 0u32;
        for count in &counts[..atoms] {
            offsets.push(total);
            total += count;
        }
        offsets.push(total);
        let mut cursor: Vec<u32> = offsets[..atoms].to_vec();
        let mut entries = vec![(0u32, 0u8, 0u16); total as usize];
        for key in 0..segment.lists.len() - 1 {
            let (term, field) = (key / FIELDS, key % FIELDS);
            for part in parts(segment, term, field) {
                for (uid, tf) in postings(segment, part) {
                    let slot = cursor.get_mut(uid as usize)?;
                    *entries.get_mut(*slot as usize)? = (term as u32, field as u8, tf as u16);
                    *slot += 1;
                }
            }
        }
        let lengths = (0..atoms)
            .map(|uid| std::array::from_fn(|field| length(segment.lengths.as_slice(), uid as u32, field)))
            .collect();
        // Withheld atoms carry no key: their (empty) analysis is never reused.
        let by_key = segment
            .keys
            .iter()
            .enumerate()
            .filter(|(_, key)| **key != NO_KEY)
            .map(|(uid, key)| (*key, uid as u32))
            .collect();
        Some(Self {
            terms,
            offsets,
            entries,
            lengths,
            by_key,
        })
    }

    /// The atom analysed under `key`, if any.
    pub(crate) fn atom(&self, key: &[u8; 16]) -> Option<PriorAtom<'_>> {
        self.by_key.get(key).map(|&uid| PriorAtom { segment: self, uid })
    }
}

/// A segment under construction.
pub(crate) struct Draft {
    names: Vec<String>,
    ids: HashMap<String, u32>,
    /// Previous ordinal to draft id, filled on first use.
    remap: Vec<u32>,
    offsets: Vec<u32>,
    entries: Vec<Entry>,
    lengths: Vec<u32>,
    classes: Vec<u32>,
    keys: Vec<[u8; 16]>,
}

impl Draft {
    pub(crate) fn new(atoms: usize) -> Self {
        Self {
            names: Vec::new(),
            ids: HashMap::new(),
            remap: Vec::new(),
            offsets: {
                let mut offsets = Vec::with_capacity(atoms + 1);
                offsets.push(0);
                offsets
            },
            entries: Vec::new(),
            lengths: Vec::with_capacity(atoms * FIELDS),
            classes: Vec::with_capacity(atoms),
            keys: Vec::with_capacity(atoms),
        }
    }

    fn intern(&mut self, term: &str) -> u32 {
        if let Some(&id) = self.ids.get(term) {
            return id;
        }
        let id = self.names.len() as u32;
        self.names.push(term.to_owned());
        self.ids.insert(term.to_owned(), id);
        id
    }

    fn close_atom(&mut self, key: [u8; 16], class: u32, lengths: [u32; FIELDS]) {
        self.offsets.push(self.entries.len() as u32);
        self.lengths.extend_from_slice(&lengths);
        self.classes.push(class);
        self.keys.push(key);
    }

    /// An atom with no indexed text (withheld from this plugin).
    pub(crate) fn push_empty(&mut self, class: u32) {
        self.close_atom(NO_KEY, class, [0; FIELDS]);
    }

    /// An atom whose analysis comes from the previous segment.
    pub(crate) fn push_reused(&mut self, key: [u8; 16], class: u32, prior: PriorAtom<'_>) {
        let previous = prior.segment;
        if self.remap.len() != previous.terms.len() {
            self.remap = vec![u32::MAX; previous.terms.len()];
        }
        let uid = prior.uid as usize;
        let (start, end) = (previous.offsets[uid] as usize, previous.offsets[uid + 1] as usize);
        for &(term, field, tf) in &previous.entries[start..end] {
            let mapped = match self.remap[term as usize] {
                u32::MAX => {
                    let id = self.intern(&previous.terms[term as usize]);
                    self.remap[term as usize] = id;
                    id
                }
                id => id,
            };
            self.entries.push((mapped, field, tf));
        }
        self.close_atom(key, class, previous.lengths[uid]);
    }

    /// An atom analysed now.
    pub(crate) fn push_analysed(
        &mut self,
        key: [u8; 16],
        class: u32,
        analyzer: &mut Analyzer,
        texts: [&str; FIELDS],
    ) {
        let (counts, lengths) = count_fields(analyzer, texts);
        for (field, terms) in counts.iter().enumerate() {
            for (term, tf) in terms {
                let id = self.intern(term);
                self.entries.push((id, field as u8, *tf));
            }
        }
        self.close_atom(key, class, lengths);
    }

    /// Assembles the inverted lists and serialises the segment.
    pub(crate) fn finish(self) -> kiwi_core::Result<Vec<u8>> {
        let atoms = self.classes.len();
        // Ordinals follow the byte order of the terms (the FST's order).
        let mut order: Vec<u32> = (0..self.names.len() as u32).collect();
        order.sort_unstable_by(|a, b| self.names[*a as usize].cmp(&self.names[*b as usize]));
        let mut ordinal = vec![0u32; self.names.len()];
        let mut builder = fst::MapBuilder::memory();
        for (rank, &id) in order.iter().enumerate() {
            ordinal[id as usize] = rank as u32;
            builder
                .insert(&self.names[id as usize], rank as u64)
                .map_err(plugin_err)?;
        }
        let terms = builder.into_inner().map_err(plugin_err)?;

        // Counting sort of postings by (term, field); uids stay ascending in each list.
        let lists_len = self.names.len() * FIELDS;
        let mut counts = vec![0u32; lists_len + 1];
        for &(id, field, _) in &self.entries {
            counts[ordinal[id as usize] as usize * FIELDS + field as usize] += 1;
        }
        let mut starts = Vec::with_capacity(lists_len + 1);
        let mut total = 0u32;
        for count in &counts[..lists_len] {
            starts.push(total);
            total += count;
        }
        starts.push(total);
        let mut cursor = starts.clone();
        let mut sorted = vec![(0u32, 0u32, 0u16); total as usize];
        // Atoms in (class, uid) order, so each list comes out partitioned by class with
        // uids ascending inside each partition, without sorting the lists.
        let mut by_class: Vec<u32> = (0..atoms as u32).collect();
        by_class.sort_by_key(|&uid| self.classes[uid as usize]);
        for uid in by_class {
            let uid = uid as usize;
            let class = self.classes[uid];
            let (begin, end) = (self.offsets[uid] as usize, self.offsets[uid + 1] as usize);
            for &(id, field, tf) in &self.entries[begin..end] {
                let list = ordinal[id as usize] as usize * FIELDS + field as usize;
                sorted[cursor[list] as usize] = (class, uid as u32, tf);
                cursor[list] += 1;
            }
        }

        // Cut each list into class partitions and encode them.
        let mut lists = Vec::with_capacity(lists_len + 1);
        let mut parts = Vec::new();
        let mut postings = Vec::with_capacity(sorted.len() * 2);
        for list in 0..lists_len {
            lists.push(parts.len() as u32);
            let slice = &sorted[starts[list] as usize..starts[list + 1] as usize];
            let mut index = 0;
            while index < slice.len() {
                let class = slice[index].0;
                let start =
                    u32::try_from(postings.len()).map_err(|_| plugin_err("more than 4 GiB of postings"))?;
                let mut previous = 0u32;
                let mut len = 0u32;
                while index < slice.len() && slice[index].0 == class {
                    let (_, uid, tf) = slice[index];
                    put_varint(&mut postings, uid - previous);
                    put_varint(&mut postings, u32::from(tf));
                    previous = uid;
                    len += 1;
                    index += 1;
                }
                parts.push(Part { class, start, len });
            }
        }
        lists.push(parts.len() as u32);

        let class_count = self.classes.iter().max().map_or(0, |max| *max as usize + 1);
        let mut classes = vec![ClassStats::default(); class_count];
        for (uid, &class) in self.classes.iter().enumerate() {
            let stats = &mut classes[class as usize];
            stats.atoms += 1;
            for (field, total) in stats.tokens.iter_mut().enumerate() {
                *total += u64::from(self.lengths[uid * FIELDS + field]);
            }
        }
        let segment = Segment {
            analyzer: ANALYZER_VERSION,
            atoms: atoms as u32,
            terms,
            lists,
            parts,
            count: sorted.len() as u64,
            postings,
            lengths: self.lengths,
            classes,
            keys: self.keys,
        };
        rkyv::to_bytes::<rkyv::rancor::Error>(&segment)
            .map(|bytes| bytes.to_vec())
            .map_err(plugin_err)
    }
}
