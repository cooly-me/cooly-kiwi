//! The knowledge model stored in a Kiwi Pack: documents, atoms, permission classes,
//! facets, the exact-match dictionary, graph edges and plugin segments.
//!
//! These are the owned, serialisable forms the compiler produces. A loaded pack exposes
//! their zero-copy archived counterparts (`Archived*`) straight from the memory map.

use rkyv::{Archive, Deserialize, Serialize};

/// Sentinel for "no atom" in `u32` uid fields (no parent, external edge target).
pub const NO_UID: u32 = u32::MAX;

/// The current pack format version. A reader refuses packs with another version.
pub const PACK_FORMAT: u32 = 3;

/// One access declaration: a sensitivity label and the groups it is limited to.
///
/// An empty `groups` list means "everyone allowed to see this label".
#[derive(
    Archive,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
#[rkyv(derive(Debug))]
pub struct AccessDecl {
    /// Sensitivity label, e.g. `public`, `internal`, `confidential`, `restricted`.
    pub label: String,
    /// Groups the content is limited to, sorted and deduplicated.
    pub groups: Vec<String>,
}

impl AccessDecl {
    /// Builds a declaration with sorted, deduplicated groups.
    pub fn new(label: impl Into<String>, groups: impl IntoIterator<Item = String>) -> Self {
        let mut groups: Vec<String> = groups.into_iter().collect();
        groups.sort();
        groups.dedup();
        Self {
            label: label.into(),
            groups,
        }
    }
}

/// Document metadata taken from frontmatter (the owner falling back to `KIWIOWNERS`);
/// every atom of a document inherits it.
#[derive(
    Archive,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    PartialEq,
    Eq,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
#[rkyv(derive(Debug))]
pub struct Meta {
    /// `vision`, `prd`, `rfc`, `adr`, `runbook`, `standard`, `glossary` or a corpus's
    /// own type, folded to lowercase.
    pub doc_type: String,
    /// `draft`, `review`, `approved` or `deprecated`.
    pub status: String,
    /// Owning team or person, e.g. `@team-payments`: the frontmatter `owner`, or else
    /// the `owner=` of the last matching `KIWIOWNERS` line that names one.
    pub owner: Option<String>,
    /// Date (`YYYY-MM-DD`) by which the document must be reviewed again.
    pub review_by: Option<String>,
    /// Free-form tags.
    pub tags: Vec<String>,
    /// The frontmatter `summary`, used as the L1 of the document root.
    pub summary: Option<String>,
}

/// One Markdown file of the corpus.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[rkyv(derive(Debug))]
pub struct DocRecord {
    /// Path relative to the corpus root, without the `.md` extension.
    pub path: String,
    /// The frontmatter `id`, e.g. `RFC-042`.
    pub doc_id: Option<String>,
    /// The document title (its first H1, or the file name).
    pub title: String,
    /// BLAKE3 of the file bytes, the key for incremental builds.
    pub file_hash: [u8; 32],
    /// The uid of the document root atom.
    pub root: u32,
    /// The first uid of the document's atoms (atoms of a document are contiguous).
    pub first: u32,
    /// Number of atoms in the document.
    pub count: u32,
    /// Frontmatter metadata.
    pub meta: Meta,
}

/// Token counts of one atom, precomputed at build time.
#[derive(
    Archive,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
#[rkyv(derive(Debug))]
pub struct Tokens {
    /// Tokens of the L0 line (title with breadcrumb).
    pub l0: u32,
    /// Tokens of the L1 abstract.
    pub l1: u32,
    /// Tokens of the L2 full text.
    pub l2: u32,
    /// Tokens of the atom id itself.
    pub id: u32,
}

/// A knowledge atom: one heading-bounded section, the leaf of the corpus tree.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[rkyv(derive(Debug))]
pub struct AtomRecord {
    /// Stable id `path#anchor`; the document root is the bare `path`.
    pub id: String,
    /// The anchor part of the id (empty for the document root).
    pub anchor: String,
    /// Index of the owning document in [`PackData::docs`].
    pub doc: u32,
    /// Parent atom uid, or [`NO_UID`] for a document root.
    pub parent: u32,
    /// Depth in the document tree (0 for the root).
    pub depth: u8,
    /// BLAKE3 of the atom content (heading and body).
    pub hash: [u8; 32],
    /// The heading text of the section.
    pub title: String,
    /// L0: the title with its breadcrumb.
    pub l0: String,
    /// L1: a short abstract.
    pub l1: String,
    /// Token counts for every level.
    pub tokens: Tokens,
    /// Permission class id, an index into [`PackData::classes`].
    pub class: u32,
    /// Other ids that resolve to this atom (sections merged into it).
    pub aliases: Vec<String>,
    /// 1-based line of the heading in the source file.
    pub line: u32,
}

/// The kinds of typed edges between atoms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EdgeKind {
    /// An RFC implements a PRD.
    Implements,
    /// A document replaces an older one.
    Supersedes,
    /// A document was decided by an ADR.
    DecidedBy,
    /// A document depends on another.
    DependsOn,
    /// A section mentions or links to another document.
    Mentions,
    /// A document governs a code path (external target).
    Code,
    /// A document changes part of another without replacing it (an ADR amending an
    /// earlier one).
    Amends,
    /// A document names a later one that amends it (the other side of [`Amends`]).
    ///
    /// [`Amends`]: EdgeKind::Amends
    AmendedBy,
}

impl EdgeKind {
    /// All kinds in their stable order.
    pub const ALL: [EdgeKind; 8] = [
        EdgeKind::Implements,
        EdgeKind::Supersedes,
        EdgeKind::DecidedBy,
        EdgeKind::DependsOn,
        EdgeKind::Mentions,
        EdgeKind::Code,
        EdgeKind::Amends,
        EdgeKind::AmendedBy,
    ];

    /// The stable byte stored in the pack.
    pub fn as_u8(self) -> u8 {
        match self {
            EdgeKind::Implements => 0,
            EdgeKind::Supersedes => 1,
            EdgeKind::DecidedBy => 2,
            EdgeKind::DependsOn => 3,
            EdgeKind::Mentions => 4,
            EdgeKind::Code => 5,
            EdgeKind::Amends => 6,
            EdgeKind::AmendedBy => 7,
        }
    }

    /// Decodes a stored byte.
    pub fn from_u8(value: u8) -> Option<Self> {
        Self::ALL.get(value as usize).copied()
    }

    /// The name used in frontmatter and in tool output.
    pub fn name(self) -> &'static str {
        match self {
            EdgeKind::Implements => "implements",
            EdgeKind::Supersedes => "supersedes",
            EdgeKind::DecidedBy => "decided_by",
            EdgeKind::DependsOn => "depends_on",
            EdgeKind::Mentions => "mentions",
            EdgeKind::Code => "code",
            EdgeKind::Amends => "amends",
            EdgeKind::AmendedBy => "amended_by",
        }
    }

    /// Parses a name used in frontmatter or tool input.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }
}

/// A typed edge. Internal edges point at an atom uid; `code` edges and unresolved
/// references carry an external target string and `to == NO_UID`.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[rkyv(derive(Debug))]
pub struct EdgeRecord {
    /// Source atom uid.
    pub from: u32,
    /// [`EdgeKind`] byte.
    pub kind: u8,
    /// Target atom uid, or [`NO_UID`] for an external target.
    pub to: u32,
    /// External target (a code glob), empty for internal edges.
    pub external: String,
}

/// A permission equivalence class: every atom with the same access chain.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[rkyv(derive(Debug))]
pub struct ClassRecord {
    /// Every declaration that must allow a reader: the document's access, then the
    /// access of each enclosing section that narrows it.
    pub chain: Vec<AccessDecl>,
    /// Roaring bitmap (portable serialisation) of the uids in the class.
    pub bitmap: Vec<u8>,
}

/// A structural facet: one metadata value and the atoms that carry it.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[rkyv(derive(Debug))]
pub struct FacetRecord {
    /// Field name: `type`, `status`, `owner` or `tag`.
    pub field: String,
    /// The folded value.
    pub value: String,
    /// Roaring bitmap (portable serialisation) of the uids.
    pub bitmap: Vec<u8>,
}

/// Bit flags describing why a key is in the exact-match dictionary.
pub mod key_kind {
    /// An atom id or alias.
    pub const ATOM_ID: u8 = 1;
    /// A document id from frontmatter (`RFC-042`) or a document path.
    pub const DOC_ID: u8 = 2;
    /// A document title.
    pub const TITLE: u8 = 4;
    /// A glossary term or alias.
    pub const TERM: u8 = 8;
    /// A section heading.
    pub const HEADING: u8 = 16;
}

/// The targets of one exact-match key, each with its own kinds: a key that is one
/// atom's id and another atom's title says which is which, so a title never stands in
/// for an id.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[rkyv(derive(Debug))]
pub struct KeyTarget {
    /// Target atom uids, ascending.
    pub uids: Vec<u32>,
    /// The [`key_kind`] flags of each uid, in the order of `uids`.
    pub kinds: Vec<u8>,
}

/// A glossary entry compiled from a `type: glossary` document.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[rkyv(derive(Debug))]
pub struct GlossaryEntry {
    /// The term as written.
    pub term: String,
    /// Its aliases as written.
    pub aliases: Vec<String>,
    /// The uid of the atom that defines it.
    pub uid: u32,
}

/// The L2 text of an atom, compressed with zstd when that saves space.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[rkyv(derive(Debug))]
pub struct Body {
    /// Length of the UTF-8 text in bytes.
    pub raw_len: u32,
    /// Whether `data` is a zstd frame.
    pub compressed: bool,
    /// The text bytes or the zstd frame.
    pub data: Vec<u8>,
}

/// A plugin segment: bytes written by one indexer, keyed by uid.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[rkyv(derive(Debug))]
pub struct SegmentRecord {
    /// The indexer name, e.g. `bm25`.
    pub name: String,
    /// The indexer's own format version.
    pub version: u32,
    /// Opaque segment bytes.
    pub data: Vec<u8>,
}

/// The whole content of a Kiwi Pack.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[rkyv(derive(Debug))]
pub struct PackData {
    /// [`PACK_FORMAT`] at build time.
    pub format: u32,
    /// The corpus namespace; ids carry it when a server federates several corpora.
    pub namespace: String,
    /// The snapshot every answer is pinned to: a commit hash, or `dir:<hash>`.
    pub snapshot: String,
    /// `git` or `dir`.
    pub source: String,
    /// The snapshot date (`YYYY-MM-DD`) that review deadlines are checked against.
    pub as_of: String,
    /// The kiwi version that built the pack.
    pub kiwi_version: String,
    /// BLAKE3 of the build configuration, so a config change invalidates the cache.
    pub config_hash: [u8; 32],
    /// Documents, sorted by path.
    pub docs: Vec<DocRecord>,
    /// Atoms; the index is the uid.
    pub atoms: Vec<AtomRecord>,
    /// L2 bodies; the index is the uid.
    pub bodies: Vec<Body>,
    /// Permission classes.
    pub classes: Vec<ClassRecord>,
    /// Structural facets, sorted by (field, value).
    pub facets: Vec<FacetRecord>,
    /// The exact-match dictionary: an `fst::Map` from folded key to an index in
    /// `key_targets`.
    pub keys_fst: Vec<u8>,
    /// Targets of the exact-match keys.
    pub key_targets: Vec<KeyTarget>,
    /// Edges sorted by (from, kind, to, external).
    pub edges: Vec<EdgeRecord>,
    /// `edges` index range per source uid: edges of uid `u` are
    /// `edges[edge_offsets[u]..edge_offsets[u + 1]]`.
    pub edge_offsets: Vec<u32>,
    /// Indexes into `edges`, sorted by (to, from).
    pub back_edges: Vec<u32>,
    /// `back_edges` range per target uid, like `edge_offsets`.
    pub back_offsets: Vec<u32>,
    /// Glossary entries, sorted by folded term.
    pub glossary: Vec<GlossaryEntry>,
    /// Plugin segments, sorted by name.
    pub segments: Vec<SegmentRecord>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_kind_round_trips() {
        for kind in EdgeKind::ALL {
            assert_eq!(EdgeKind::from_u8(kind.as_u8()), Some(kind));
            assert_eq!(EdgeKind::parse(kind.name()), Some(kind));
        }
        assert_eq!(EdgeKind::from_u8(42), None);
    }

    #[test]
    fn access_decl_sorts_groups() {
        let decl = AccessDecl::new("internal", ["b".to_owned(), "a".to_owned(), "a".to_owned()]);
        assert_eq!(decl.groups, vec!["a", "b"]);
    }
}
