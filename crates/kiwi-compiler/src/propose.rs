//! Writes through Git: a proposal becomes a branch with one commit on top of the base
//! revision, created without touching the work tree, then a pull request (or a local
//! proposal record). Agents never write to the main branch.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use gix::bstr::BStr;
use kiwi_core::model::AccessDecl;
use kiwi_core::plugin::{Finding, Lint, Severity};
use kiwi_core::{Pack, date, text};
use serde::{Deserialize, Serialize};

use crate::build::{check_unique_ids, compile_docs, load_config, load_owners};
use crate::compile::{CompiledDoc, SectionInfo, SectionSpan, compile_with_outline};
use crate::config::ForgeConfig;
use crate::error::{CompileError, Result};
use crate::link::{LinkInput, link};
use crate::lint::BuiltinLint;
use crate::source::{self, Snapshot, SourceMode};

/// One file change: new content, or `None` to delete the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Change {
    /// Corpus-relative Markdown path, e.g. `adr/adr-009-retries.md`.
    pub path: String,
    /// The complete new file content; omit (or null) to delete the file.
    #[serde(default)]
    pub content: Option<String>,
}

/// Who proposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Author {
    /// User id.
    pub principal: String,
    /// The agent acting for them.
    pub agent: String,
    /// Display name for the commit.
    pub name: String,
    /// E-mail for the commit.
    pub email: String,
}

/// A created proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    /// Proposal id.
    pub id: String,
    /// The branch.
    pub branch: String,
    /// The commit on the branch.
    pub commit: String,
    /// The base commit.
    pub base: String,
    /// A link to the pull request, or `kiwi://proposal/<id>` for the local forge.
    pub link: String,
    /// Title.
    pub title: String,
    /// Rationale.
    pub rationale: String,
    /// User.
    pub principal: String,
    /// Agent.
    pub agent: String,
    /// Files changed.
    pub files: Vec<String>,
    /// `direct` when the policy lets the user merge without review, else `review`, as
    /// decided on what the user can see.
    pub merge: String,
    /// The change conflicts with content the user cannot see (the audit log says how):
    /// an owner reviews it whatever `merge` says. The pull request says so, without the
    /// hidden content.
    #[serde(default)]
    pub owner_review: bool,
    /// The owners whose review is requested.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub review_owners: Vec<String>,
    /// What could not be done on the pull request of an `owner_review` proposal
    /// (drafting, labelling, requesting reviews), for the operator.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owner_review_unapplied: Vec<String>,
    /// Lint findings on the proposed corpus that concern the changed files.
    pub lint: Vec<Finding>,
    /// When.
    pub created: String,
}

/// What a pull request or commit of a proposal that needs an owner's review says, and
/// all it says: never the hidden content or where it is.
pub const OWNER_REVIEW_NOTICE: &str = "Kiwi: an owner must review this proposal. Put together with content its author cannot see, the change conflicts; Kiwi's audit log has the details.";

/// Checks a change path: a relative Markdown path, outside `.kiwi/`, no `..`, never the
/// root README or under `KIWIOWNERS`.
pub fn check_path(path: &str) -> Result<()> {
    let bad = |why: &str| Err(CompileError::Config(format!("change path `{path}`: {why}")));
    if path.is_empty() || path.starts_with('/') || path.contains('\\') {
        return bad("must be relative with forward slashes");
    }
    if path
        .split('/')
        .any(|p| p == ".." || p == "." || p.is_empty() || p.starts_with('.'))
    {
        return bad("must not contain `..`, `.` or hidden segments");
    }
    if !path.ends_with(".md") {
        return bad("only Markdown documents can be proposed");
    }
    // Files the corpus is governed by, and ones Kiwi does not read as documents.
    if path.eq_ignore_ascii_case("README.md") {
        return bad("the root README is not a document of the corpus");
    }
    if path
        .split('/')
        .any(|part| part.eq_ignore_ascii_case("KIWIOWNERS"))
    {
        return bad("KIWIOWNERS is not changed through proposals");
    }
    if path.len() > 200 {
        return bad("too long");
    }
    Ok(())
}

/// The corpus and repository a proposal applies to.
#[derive(Debug, Clone)]
pub struct ProposalTarget {
    /// The corpus root (inside a Git work tree).
    pub corpus: PathBuf,
    /// The base revision.
    pub base: String,
    /// The forge settings.
    pub forge: ForgeConfig,
    /// Where local proposal records go.
    pub state_dir: PathBuf,
    /// The compile cache to share (a store's `cache` directory), so a proposal compiles
    /// only the documents it changes.
    pub cache_dir: PathBuf,
}

/// What one caller may do, as the corpus policy decides. `propose` and `merge` include
/// the right to read.
pub trait Permissions {
    /// May read content with this access chain.
    fn read(&self, chain: &[AccessDecl]) -> std::result::Result<bool, String>;
    /// May propose changes to it.
    fn propose(&self, chain: &[AccessDecl]) -> std::result::Result<bool, String>;
    /// May merge changes to it without review.
    fn merge(&self, chain: &[AccessDecl]) -> std::result::Result<bool, String>;
}

/// One requested change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    /// A new body for one section (its heading line is kept).
    Section {
        /// The section's id (`path#anchor`, a document id or path for the root).
        id: String,
        /// The new body.
        body: String,
    },
    /// Frontmatter keys to set on a document.
    Set {
        /// The document.
        path: String,
        /// The keys and their values.
        keys: BTreeMap<String, serde_json::Value>,
    },
    /// A file to create or replace.
    Write {
        /// The file.
        path: String,
        /// Its new content.
        content: String,
    },
    /// A file to delete.
    Delete {
        /// The file.
        path: String,
    },
}

/// Why a proposal was not prepared.
#[derive(Debug, thiserror::Error)]
pub enum ProposeError {
    /// Refused; the message reveals nothing outside the caller's view.
    #[error("{0}")]
    Refused(String),
    /// Failed for a reason that may concern content the caller cannot see: only a
    /// generic message reaches the caller, the cause goes to the audit log.
    #[error("the proposal could not be prepared")]
    Failed(#[source] CompileError),
}

/// A proposal checked against the caller's permissions, ready to commit.
#[derive(Debug, Clone)]
pub struct Plan {
    /// The base snapshot.
    pub snapshot: Snapshot,
    /// The whole-file changes to commit: the caller's changes with every section and
    /// document the caller cannot see put back.
    pub changes: Vec<Change>,
    /// `direct` when the policy lets the caller merge every touched section without
    /// review, else `review`, as decided on the caller's view.
    pub merge: String,
    /// Lint findings on the changed documents, computed on the proposed corpus as the
    /// caller sees it.
    pub lint: Vec<Finding>,
    /// Conflicts with content hidden from the caller, for the audit log only.
    pub notes: Vec<String>,
    /// The change conflicts with content the caller cannot see: the proposal needs an
    /// owner's review whatever `merge` says.
    pub owner_review: bool,
    /// The owners of the files in conflict, whose review is requested.
    pub review_owners: Vec<String>,
    /// Documents compiled for this proposal; every other one came from the compile
    /// cache, so a proposal costs what it changes, not a build of the corpus.
    pub compiled: usize,
}

type Planned<T> = std::result::Result<T, ProposeError>;

fn refused<T>(message: impl Into<String>) -> Planned<T> {
    Err(ProposeError::Refused(message.into()))
}

/// The caller's permissions, decided once per access chain.
struct Judge<'a> {
    permissions: &'a dyn Permissions,
    read: BTreeMap<Vec<AccessDecl>, bool>,
}

impl<'a> Judge<'a> {
    fn new(permissions: &'a dyn Permissions) -> Self {
        Self {
            permissions,
            read: BTreeMap::new(),
        }
    }

    fn failed(message: String) -> ProposeError {
        ProposeError::Failed(CompileError::Config(format!("policy: {message}")))
    }

    fn read(&mut self, chain: &[AccessDecl]) -> Planned<bool> {
        if let Some(&known) = self.read.get(chain) {
            return Ok(known);
        }
        let allowed = self.permissions.read(chain).map_err(Self::failed)?;
        self.read.insert(chain.to_vec(), allowed);
        Ok(allowed)
    }

    fn propose(&self, chain: &[AccessDecl]) -> Planned<bool> {
        self.permissions.propose(chain).map_err(Self::failed)
    }

    fn merge(&self, chain: &[AccessDecl]) -> Planned<bool> {
        self.permissions.merge(chain).map_err(Self::failed)
    }

    fn reads_all(&mut self, doc: &CompiledDoc) -> Planned<bool> {
        for atom in &doc.atoms {
            if !self.read(&atom.chain)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn reads_root(&mut self, doc: &CompiledDoc) -> Planned<bool> {
        match doc.atoms.first() {
            Some(root) => self.read(&root.chain),
            None => Ok(false),
        }
    }
}

/// The edits of one file.
#[derive(Default)]
struct FileEdits {
    sections: Vec<(String, String)>,
    sets: Vec<(String, serde_json::Value)>,
    whole: Option<Option<String>>,
}

/// Checks `edits` against the caller's permissions on the base revision and prepares
/// the whole-file changes, the merge decision and the lint findings.
///
/// Everything the caller is told (accepted or refused, every message, compile errors,
/// lint findings, `merge`) is a function of the request and of the caller's view alone:
/// the proposal is decided on the base revision stripped of every document and section
/// the caller cannot read (as [`strip_hidden`] cuts them), every change before any real
/// file is built, exactly as on a corpus that never held the hidden content. Only then
/// are the real files built ([`realize`]): each hidden section is put back by its own
/// span after the visible section it followed. A real file that does not compile, a
/// hidden section that would move, change level, parent, access or text, or be taken
/// in by the caller's text, a hidden document replaced, and an id or case clash with
/// hidden content never change the answer: the proposal opens as decided, marked for an
/// owner's review, with the conflict in the audit note.
///
/// A touched section needs `propose` on its access chain before and after the change;
/// a change of access also needs `merge` on the new chain and always needs review, as
/// does every new file.
pub fn plan(target: &ProposalTarget, permissions: &dyn Permissions, edits: &[Edit]) -> Planned<Plan> {
    if edits.is_empty() {
        return refused("a proposal needs at least one change");
    }
    let snapshot =
        source::read(&target.corpus, &SourceMode::Git(target.base.clone())).map_err(ProposeError::Failed)?;
    let config = load_config(&snapshot).map_err(ProposeError::Failed)?;
    let owners = load_owners(&snapshot).map_err(ProposeError::Failed)?;
    let compiled =
        compile_docs(&snapshot, &config, &owners, &target.cache_dir).map_err(ProposeError::Failed)?;
    let mut fresh = compiled.iter().filter(|(_, _, fresh)| *fresh).count();
    let base: BTreeMap<String, CompiledDoc> = compiled
        .into_iter()
        .map(|(doc, _, _)| (format!("{}.md", doc.path), doc))
        .collect();
    let mut judge = Judge::new(permissions);

    // The caller's view of the base revision.
    let mut files = snapshot.files.clone();
    for (file, doc) in &base {
        if !judge.reads_root(doc)? {
            files.remove(file);
        } else if !judge.reads_all(doc)? {
            let text = snapshot
                .file(file)
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
                .unwrap_or_default();
            let stripped = strip_hidden(&text, &doc.sections, |chain| judge.read(chain))?;
            // The view must parse as the visible part of the real file does; a cut that
            // changed it would make the answer differ from the real file, so fail closed.
            let defaults = owners.defaults(file);
            let (_, real) =
                compile_with_outline(file, &text, &defaults, &config).map_err(ProposeError::Failed)?;
            let (_, viewed) =
                compile_with_outline(file, &stripped, &defaults, &config).map_err(ProposeError::Failed)?;
            let mut readable = Vec::with_capacity(real.len());
            for section in &real {
                readable.push(judge.read(&section.chain)?);
            }
            if !view_holds(&real, &readable, &viewed) {
                return Err(ProposeError::Failed(CompileError::Config(format!(
                    "{file}: the caller's view does not parse as the visible part of the document"
                ))));
            }
            files.insert(file.clone(), stripped.into_bytes());
        }
    }
    let view = Snapshot {
        files,
        ..snapshot.clone()
    };
    let context = Context {
        config: &config,
        owners: &owners,
        cache_dir: &target.cache_dir,
    };
    let decided = decide(&view, &context, &mut judge, edits)?;
    fresh += decided.compiled;
    let realized = realize(&snapshot, &base, &decided.changes, &context, &mut judge)?;
    let mut notes = realized.notes;
    let mut owner_review = realized.conflict;
    let mut review_owners = realized.owners;
    // A duplicate id with hidden content.
    let mut full: Vec<CompiledDoc> = base
        .iter()
        .filter(|(file, _)| !realized.changes.iter().any(|c| &c.path == *file))
        .map(|(_, doc)| doc.clone())
        .collect();
    full.extend(realized.docs);
    if let Err(conflict) = check_unique_ids(&full) {
        notes.push(format!("the proposed corpus does not build: {conflict}"));
        owner_review = true;
        // The owners of the changed files review it.
        review_owners.extend(realized.changes.iter().filter_map(|change| {
            base.get(&change.path)
                .and_then(|doc| doc.meta.owner.clone())
                .or_else(|| owners.owner(&change.path).map(str::to_owned))
        }));
    }
    Ok(Plan {
        snapshot,
        changes: realized.changes,
        merge: decided.merge,
        lint: decided.lint,
        notes,
        owner_review,
        review_owners: review_owners.into_iter().collect(),
        compiled: fresh,
    })
}

/// What the planning of a proposal reads besides the snapshot.
struct Context<'a> {
    config: &'a crate::KiwiConfig,
    owners: &'a kiwi_policy::KiwiOwners,
    cache_dir: &'a Path,
}

/// The answer decided on the caller's view.
struct Decided {
    changes: Vec<Change>,
    merge: String,
    lint: Vec<Finding>,
    compiled: usize,
}

/// Decides every change of a proposal on `view`, the base revision as the caller sees
/// it: ids, edits, permissions, compile errors, `merge` and lint.
fn decide(view: &Snapshot, context: &Context<'_>, judge: &mut Judge<'_>, edits: &[Edit]) -> Planned<Decided> {
    let config = context.config;
    let defaults_of = |file: &str| context.owners.defaults(file);
    let compiled =
        compile_docs(view, config, context.owners, context.cache_dir).map_err(ProposeError::Failed)?;
    let mut fresh = compiled.iter().filter(|(_, _, fresh)| *fresh).count();
    let base: BTreeMap<String, CompiledDoc> = compiled
        .into_iter()
        .map(|(doc, _, _)| (format!("{}.md", doc.path), doc))
        .collect();

    // The ids the caller may name: atoms, merged sections, document ids and paths.
    let mut ids: BTreeMap<String, (String, String)> = BTreeMap::new();
    for (file, doc) in &base {
        let root = doc.atoms[0].id.clone();
        for atom in &doc.atoms {
            for id in std::iter::once(&atom.id).chain(&atom.aliases) {
                ids.entry(text::key(id)).or_insert((file.clone(), id.clone()));
            }
        }
        ids.entry(text::key(&doc.path))
            .or_insert((file.clone(), root.clone()));
        if let Some(doc_id) = &doc.doc_id {
            ids.entry(text::key(doc_id)).or_insert((file.clone(), root));
        }
    }

    let mut files: BTreeMap<String, FileEdits> = BTreeMap::new();
    for edit in edits {
        match edit {
            Edit::Section { id, body } => {
                let (file, named) = ids
                    .get(&text::key(id))
                    .cloned()
                    .ok_or_else(|| ProposeError::Refused(format!("no section `{id}`")))?;
                files
                    .entry(file)
                    .or_default()
                    .sections
                    .push((named, body.clone()));
            }
            Edit::Set { path, keys } => {
                check_path(path).map_err(|e| ProposeError::Refused(e.to_string()))?;
                if !base.contains_key(path) {
                    return refused(format!("no document `{path}`"));
                }
                let entry = files.entry(path.clone()).or_default();
                entry
                    .sets
                    .extend(keys.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
            Edit::Write { path, .. } | Edit::Delete { path } => {
                check_path(path).map_err(|e| ProposeError::Refused(e.to_string()))?;
                if matches!(edit, Edit::Write { .. })
                    && let Some((why, _)) =
                        layout_clash(path, view.files.keys().map(|f| (f.as_str(), Entry::Blob)))
                {
                    return refused(format!("`{path}`: {why}"));
                }
                let entry = files.entry(path.clone()).or_default();
                if entry.whole.is_some() {
                    return refused(format!("{path}: change a file at most once per proposal"));
                }
                entry.whole = Some(match edit {
                    Edit::Write { content, .. } => Some(content.clone()),
                    _ => None,
                });
            }
        }
    }
    let new_paths: Vec<&String> = files
        .iter()
        .filter(|(_, e)| matches!(e.whole, Some(Some(_))))
        .map(|(path, _)| path)
        .collect();
    for (i, a) in new_paths.iter().enumerate() {
        if let Some(b) = new_paths[i + 1..].iter().find(|b| path_key(b) == path_key(a)) {
            return refused(format!("`{a}` and `{b}` differ only in case"));
        }
        // The proposal's own new files must not run into each other either.
        let others = new_paths
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(_, other)| (other.as_str(), Entry::Blob));
        if let Some((why, _)) = layout_clash(a, others) {
            return refused(format!("`{a}`: {why}"));
        }
        for b in &new_paths[i + 1..] {
            if let Some((mine, theirs)) = case_clash(a, b) {
                return refused(format!("`{mine}` differs only in case from `{theirs}`"));
            }
        }
    }

    let mut review = false;
    let mut direct = true;
    let mut changes = Vec::new();
    let mut new_docs: BTreeMap<String, CompiledDoc> = BTreeMap::new();
    for (file, edit) in &files {
        let old_doc = base.get(file);
        let old_text = view
            .file(file)
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned());
        let old: Option<(&CompiledDoc, &str)> = old_doc.zip(old_text.as_deref());
        let new_text: Option<String> = match &edit.whole {
            Some(content) => {
                if !edit.sections.is_empty() || !edit.sets.is_empty() {
                    return refused(format!(
                        "{file}: do not mix a whole-file change with section or set changes"
                    ));
                }
                if old.is_none() {
                    if content.is_none() {
                        return refused(format!("no document `{file}`"));
                    }
                    review = true;
                    check_case(file, &base)?;
                }
                if let Some(content) = content
                    && !ends_closed(content, "")
                {
                    return refused(format!(
                        "{file}: the text leaves a code block, HTML block or other construct open at its end; close it"
                    ));
                }
                content.clone()
            }
            None => {
                let Some((_, text)) = old else {
                    return refused(format!("no document `{file}`"));
                };
                let (_, outline) = compile_with_outline(file, text, &defaults_of(file), config)
                    .map_err(ProposeError::Failed)?;
                let mut text = replace_sections(text, &outline, &edit.sections)?;
                for (key, value) in &edit.sets {
                    let yaml =
                        serde_json::to_string(value).map_err(|e| ProposeError::Refused(e.to_string()))?;
                    text = set_frontmatter(&text, key, &yaml)
                        .map_err(|e| ProposeError::Refused(e.to_string()))?;
                }
                Some(text)
            }
        };

        // Governance over every section the change touches.
        let new = match &new_text {
            Some(text) => Some(
                compile_with_outline(file, text, &defaults_of(file), config)
                    .map_err(|e| ProposeError::Refused(e.to_string()))?,
            ),
            None => None,
        };
        let old_outline = match old {
            Some((_, text)) => {
                compile_with_outline(file, text, &defaults_of(file), config)
                    .map_err(ProposeError::Failed)?
                    .1
            }
            None => Vec::new(),
        };
        let empty = Vec::new();
        let new_outline = new.as_ref().map_or(&empty, |(_, outline)| outline);
        let meta_changed = match (old, &new) {
            (Some((old_doc, _)), Some((new_doc, _))) => {
                old_doc.meta != new_doc.meta
                    || old_doc.refs != new_doc.refs
                    || old_doc.doc_id != new_doc.doc_id
                    || old_doc.title != new_doc.title
            }
            _ => false,
        };
        let verdict = govern(
            file,
            &old_outline,
            new_outline,
            meta_changed,
            old.is_some(),
            judge,
        )?;
        review |= verdict.review;
        direct &= verdict.direct;
        if let Some((doc, _)) = new {
            new_docs.insert(file.clone(), doc);
        }
        changes.push(Change {
            path: file.clone(),
            content: new_text,
        });
    }
    if changes
        .iter()
        .all(|c| c.content.as_deref().map(str::as_bytes) == view.file(&c.path))
    {
        return refused("the changes do not modify the corpus");
    }
    fresh += new_docs.len();
    let lint = lint_changes(view, context.config, &base, &new_docs, &changes)?;
    Ok(Decided {
        changes,
        merge: if direct && !review { "direct" } else { "review" }.to_owned(),
        lint,
        compiled: fresh,
    })
}

/// What [`govern`] decided for one file.
struct Verdict {
    direct: bool,
    review: bool,
}

/// Checks the sections one file's change touches (on the caller's view): the caller
/// needs `propose` on every touched section's chain before and after the change, and a
/// change of access (a section whose chain changes, or a chain new to an existing
/// document) needs `merge` on the new chain and review.
fn govern(
    file: &str,
    old: &[SectionInfo],
    new: &[SectionInfo],
    meta_changed: bool,
    existed: bool,
    judge: &mut Judge<'_>,
) -> Planned<Verdict> {
    let mut verdict = Verdict {
        direct: true,
        review: false,
    };
    let old_by_id: BTreeMap<&str, &SectionInfo> = old.iter().map(|s| (s.id.as_str(), s)).collect();
    let new_by_id: BTreeMap<&str, &SectionInfo> = new.iter().map(|s| (s.id.as_str(), s)).collect();
    let old_chains: BTreeSet<&Vec<AccessDecl>> = old.iter().map(|s| &s.chain).collect();
    let refuse_here = || ProposeError::Refused(format!("{file}: you may not propose changes here"));
    let ids: BTreeSet<&str> = old_by_id.keys().chain(new_by_id.keys()).copied().collect();
    for id in ids {
        let before = old_by_id.get(id).copied();
        let after = new_by_id.get(id).copied();
        let is_root = id == file.trim_end_matches(".md");
        let touched = match (before, after) {
            (Some(a), Some(b)) => !same_section(a, b) || (is_root && meta_changed),
            _ => true,
        };
        if !touched {
            continue;
        }
        if let Some(before) = before {
            if !judge.propose(&before.chain)? {
                return Err(refuse_here());
            }
            verdict.direct &= judge.merge(&before.chain)?;
        }
        if let Some(after) = after {
            if !judge.propose(&after.chain)? {
                return Err(refuse_here());
            }
            verdict.direct &= judge.merge(&after.chain)?;
            let access_changed = match before {
                Some(before) => before.chain != after.chain,
                None => existed && !old_chains.contains(&after.chain),
            };
            if access_changed {
                if !judge.merge(&after.chain)? {
                    return refused(format!(
                        "{file}: changing access needs the right to merge the result"
                    ));
                }
                verdict.review = true;
            }
        }
    }
    Ok(verdict)
}

/// Whether two sections read the same: access, heading and text.
fn same_section(a: &SectionInfo, b: &SectionInfo) -> bool {
    a.chain == b.chain
        && a.title == b.title
        && a.level == b.level
        && a.markdown == b.markdown
        && a.links == b.links
}

/// Refuses a new path that differs only in case from a document's path or directory
/// in the caller's view.
fn check_case(file: &str, base: &BTreeMap<String, CompiledDoc>) -> Planned<()> {
    for other in base.keys() {
        if let Some((mine, theirs)) = case_clash(file, other) {
            return refused(format!("`{mine}` differs only in case from `{theirs}`"));
        }
    }
    Ok(())
}

/// What a path of a tree is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    Blob,
    Tree,
    Link,
    Submodule,
}

/// A path as paths are compared: Unicode NFC, then case folded, so `Café.md`,
/// `cafe\u{301}.md` and `CAFÉ.md` are one path.
fn path_key(path: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    path.nfc().collect::<String>().to_lowercase()
}

/// Why writing the file `path` would replace another entry of a tree, and that entry:
/// a directory on its way is a file, a link or a submodule; `path` is a directory of
/// any entry; or `path` itself is a link or a submodule (compared by [`path_key`]).
fn layout_clash<'a>(
    path: &str,
    entries: impl Iterator<Item = (&'a str, Entry)>,
) -> Option<(String, &'a str)> {
    let key = path_key(path);
    let parts: Vec<&str> = key.split('/').collect();
    let prefixes: Vec<String> = (1..parts.len()).map(|n| parts[..n].join("/")).collect();
    let as_dir = format!("{key}/");
    for (other, kind) in entries {
        let theirs = path_key(other);
        if kind != Entry::Tree && prefixes.contains(&theirs) {
            return Some((format!("`{other}` is a file, not a directory"), other));
        }
        if theirs.starts_with(&as_dir) {
            return Some(("it is a directory".into(), other));
        }
        if theirs == key {
            // A directory is found through the entries under it, the files it holds
            // whose owners review the change.
            match kind {
                Entry::Blob | Entry::Tree => {}
                Entry::Link => return Some(("it is a symbolic link".into(), other)),
                Entry::Submodule => return Some(("it is a submodule".into(), other)),
            }
        }
    }
    None
}

/// Every entry (files, directories, links, submodules) of the base revision's tree
/// under the corpus, by path relative to the corpus; the snapshot's files when the
/// snapshot is no Git revision.
fn tree_entries(snapshot: &Snapshot) -> Result<Vec<(String, Entry)>> {
    let Some(location) = &snapshot.repo else {
        return Ok(snapshot.files.keys().map(|f| (f.clone(), Entry::Blob)).collect());
    };
    let repo = gix::open(&location.repo_path).map_err(CompileError::git)?;
    let tree = repo
        .rev_parse_single(location.commit.as_str())
        .map_err(CompileError::git)?
        .object()
        .map_err(CompileError::git)?
        .peel_to_tree()
        .map_err(CompileError::git)?;
    let mut recorder = gix::traverse::tree::Recorder::default();
    tree.traverse()
        .breadthfirst(&mut recorder)
        .map_err(CompileError::git)?;
    let prefix = if location.prefix.is_empty() {
        String::new()
    } else {
        format!("{}/", location.prefix)
    };
    Ok(recorder
        .records
        .into_iter()
        .filter_map(|entry| {
            let path = entry.filepath.to_string();
            let path = path.strip_prefix(&prefix)?.to_owned();
            let kind = if entry.mode.is_tree() {
                Entry::Tree
            } else if entry.mode.is_link() {
                Entry::Link
            } else if entry.mode.is_commit() {
                Entry::Submodule
            } else {
                Entry::Blob
            };
            Some((path, kind))
        })
        .collect())
}

/// The first path prefix (a directory or the file) of `file` that differs from the one
/// of `other` only in case.
fn case_clash(file: &str, other: &str) -> Option<(String, String)> {
    if file == other {
        return None;
    }
    let prefixes = |path: &str| -> Vec<String> {
        let parts: Vec<&str> = path.split('/').collect();
        (1..=parts.len()).map(|n| parts[..n].join("/")).collect()
    };
    prefixes(file)
        .into_iter()
        .zip(prefixes(other))
        .find(|(a, b)| a != b && path_key(a) == path_key(b))
}

/// The real files of a decided proposal, and its conflicts with hidden content.
struct Realized {
    changes: Vec<Change>,
    /// The real documents of the changed files.
    docs: Vec<CompiledDoc>,
    notes: Vec<String>,
    conflict: bool,
    /// The owners of the files in conflict (frontmatter `owner:`, else `KIWIOWNERS`).
    owners: BTreeSet<String>,
}

/// Builds the real files of `decided` (changes to the caller's view of the base): a
/// document with sections the caller cannot see gets them back by their own spans
/// ([`reinsert`]). Nothing here is ever a refusal or a failure the caller would see:
/// what does not fit is a conflict for the owner.
fn realize(
    snapshot: &Snapshot,
    base: &BTreeMap<String, CompiledDoc>,
    decided: &[Change],
    context: &Context<'_>,
    judge: &mut Judge<'_>,
) -> Planned<Realized> {
    let mut realized = Realized {
        changes: Vec::new(),
        docs: Vec::new(),
        notes: Vec::new(),
        conflict: false,
        owners: BTreeSet::new(),
    };
    let conflict = |realized: &mut Realized, note: String| {
        realized.notes.push(note);
        realized.conflict = true;
    };
    // Everything the base tree holds under the corpus: what a new path must not run
    // into, whether the caller can see it or Kiwi reads it at all.
    let entries = tree_entries(snapshot).map_err(ProposeError::Failed)?;
    for change in decided {
        let file = &change.path;
        let defaults = context.owners.defaults(file);
        let notes_before = realized.notes.len();
        let owner = base
            .get(file)
            .and_then(|doc| doc.meta.owner.clone())
            .or_else(|| defaults.owner.clone());
        // A file whose path runs into one the caller cannot see (a hidden document, or
        // a file that is not a document) is left out: writing it would replace that.
        if change.content.is_some()
            && let Some((why, other)) = layout_clash(file, entries.iter().map(|(p, k)| (p.as_str(), *k)))
        {
            conflict(
                &mut realized,
                format!("{file}: left out of the commit, it would replace another file ({why})"),
            );
            // The owner of the file it would replace reviews it.
            let theirs = base
                .get(other)
                .and_then(|doc| doc.meta.owner.clone())
                .or_else(|| context.owners.owner(other).map(str::to_owned));
            realized.owners.extend(theirs);
            continue;
        }
        let content = match (base.get(file), &change.content) {
            (None, content) => {
                if content.is_some() {
                    for (other, _) in &entries {
                        if case_clash(file, other).is_some() {
                            conflict(
                                &mut realized,
                                format!(
                                    "{file}: differs only in case from {other}, which the proposer cannot see"
                                ),
                            );
                        }
                    }
                }
                content.clone()
            }
            (Some(doc), content) if !judge.reads_root(doc)? => {
                conflict(
                    &mut realized,
                    format!("{file}: the proposal replaces a document the proposer cannot see"),
                );
                content.clone()
            }
            (Some(doc), content) if judge.reads_all(doc)? => content.clone(),
            (Some(_), None) => {
                conflict(
                    &mut realized,
                    format!("{file}: the proposal deletes sections the proposer cannot see"),
                );
                None
            }
            (Some(_), Some(new)) => {
                let old = snapshot
                    .file(file)
                    .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
                    .unwrap_or_default();
                let (_, outline) = compile_with_outline(file, &old, &defaults, context.config)
                    .map_err(ProposeError::Failed)?;
                match compile_with_outline(file, new, &defaults, context.config) {
                    Ok((_, written)) => {
                        let real = reinsert(&old, &outline, new, &written, judge)?;
                        if let Some(problem) = check_real(file, &real, &outline, &written, context, judge)? {
                            conflict(&mut realized, problem);
                        }
                        Some(real)
                    }
                    Err(error) => {
                        conflict(&mut realized, format!("{file}: {error}"));
                        Some(new.clone())
                    }
                }
            }
        };
        if let Some(text) = &content
            && let Ok(doc) = crate::compile::compile_doc(file, text, &defaults, context.config)
        {
            realized.docs.push(doc);
        }
        if realized.notes.len() > notes_before {
            realized.owners.extend(owner);
        }
        realized.changes.push(Change {
            path: file.clone(),
            content,
        });
    }
    Ok(realized)
}

/// Whether `viewed` (a document's outline with its hidden sections cut) lists exactly the
/// sections of `real` that are `readable`, with the same id, title, level and parent.
pub fn view_holds(real: &[SectionInfo], readable: &[bool], viewed: &[SectionInfo]) -> bool {
    let real_parents = parents(real);
    let kept: Vec<(&SectionInfo, Option<&str>)> = real
        .iter()
        .zip(real_parents)
        .zip(readable)
        .filter(|(_, seen)| **seen)
        .map(|(pair, _)| pair)
        .collect();
    kept.len() == viewed.len()
        && kept
            .iter()
            .zip(viewed.iter().zip(parents(viewed)))
            .all(|((a, pa), (b, pb))| a.id == b.id && a.title == b.title && a.level == b.level && *pa == pb)
}

/// The parent of each section of an outline (the nearest earlier section of a lower
/// level), by id.
fn parents(outline: &[SectionInfo]) -> Vec<Option<&str>> {
    let mut stack: Vec<(u8, &str)> = Vec::new();
    outline
        .iter()
        .map(|section| {
            while stack.last().is_some_and(|(level, _)| *level >= section.level) {
                stack.pop();
            }
            let parent = stack.last().map(|(_, id)| *id);
            stack.push((section.level, section.id.as_str()));
            parent
        })
        .collect()
}

/// Why the real text of a document with hidden sections does not hold what the
/// caller's text and the hidden sections each said, if it does not: it must compile,
/// every hidden section must keep its order, level, parent, access and text, and every
/// section the caller wrote must read as written.
fn check_real(
    file: &str,
    real: &str,
    old: &[SectionInfo],
    written: &[SectionInfo],
    context: &Context<'_>,
    judge: &mut Judge<'_>,
) -> Planned<Option<String>> {
    let (_, outline) = match compile_with_outline(file, real, &context.owners.defaults(file), context.config)
    {
        Ok(compiled) => compiled,
        Err(error) => return Ok(Some(format!("{file}: the real file does not compile: {error}"))),
    };
    let mut hidden: Vec<(&SectionInfo, Option<&str>)> = Vec::new();
    for (section, parent) in old.iter().zip(parents(old)) {
        if !judge.read(&section.chain)? {
            hidden.push((section, parent));
        }
    }
    let is_hidden = |id: &str| hidden.iter().any(|(s, _)| s.id == id);
    let real_parents = parents(&outline);
    let kept: Vec<(&SectionInfo, Option<&str>)> = outline
        .iter()
        .zip(real_parents.iter().copied())
        .filter(|(s, _)| is_hidden(&s.id))
        .collect();
    let hidden_holds = kept.len() == hidden.len()
        && kept
            .iter()
            .zip(&hidden)
            .all(|((a, pa), (b, pb))| a.id == b.id && same_section(a, b) && pa == pb);
    if !hidden_holds {
        return Ok(Some(format!(
            "{file}: the change moves, re-parents, takes in or changes sections the proposer cannot see"
        )));
    }
    let written_parents = parents(written);
    let visible: Vec<(&SectionInfo, Option<&str>)> = outline
        .iter()
        .zip(real_parents.iter().copied())
        .filter(|(s, _)| !is_hidden(&s.id))
        .collect();
    let written_holds = visible.len() == written.len()
        && visible
            .iter()
            .zip(written.iter().zip(written_parents))
            .all(|((a, pa), (b, pb))| a.id == b.id && same_section(a, b) && *pa == pb);
    if !written_holds {
        return Ok(Some(format!(
            "{file}: with the sections the proposer cannot see put back, the change reads differently"
        )));
    }
    Ok(None)
}

/// Replaces the bodies of `sections` (named by id) in `text`, by the spans of
/// `outline`. A body must leave every construct it opens closed.
fn replace_sections(text: &str, outline: &[SectionInfo], sections: &[(String, String)]) -> Planned<String> {
    let mut targets: Vec<(&SectionInfo, bool, &str)> = Vec::new();
    for (id, body) in sections {
        let Some(section) = outline.iter().find(|s| s.ids.iter().any(|i| i == id)) else {
            let whole = id
                .rsplit_once('~')
                .map_or(id.as_str(), |(head, _)| head.trim_end_matches('#'));
            return refused(format!(
                "`{id}` is one part of a longer section; change the whole section `{whole}`"
            ));
        };
        if targets.iter().any(|(t, _, _)| t.id == section.id) {
            return refused(format!("`{id}` is changed twice"));
        }
        let takes_indented = check_body(id, body)?;
        targets.push((section, takes_indented, body));
    }
    targets.sort_by_key(|(section, _, _)| std::cmp::Reverse(section.content_start));
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = text.to_owned();
    for (section, takes_indented, body) in targets {
        let head = &out[..section.content_start];
        let tail = &out[section.end..];
        let body: Vec<&str> = body.trim_matches(['\n', '\r']).lines().collect();
        let mut next = String::with_capacity(out.len() + body.len());
        next.push_str(head);
        if !head.is_empty() && !head.ends_with('\n') {
            next.push_str(newline);
        }
        if !body.is_empty() {
            if !head.is_empty() {
                next.push_str(newline);
            }
            next.push_str(&body.join(newline));
            next.push_str(newline);
        }
        if !tail.is_empty() {
            next.push_str(newline);
            if takes_indented && !body.is_empty() {
                next.push_str(&heading_at_margin(tail));
            } else {
                next.push_str(tail);
            }
        }
        out = next;
    }
    Ok(out)
}

/// `text` with its first line moved to the left margin when it is indented by one to
/// three spaces. Placed after text that ends in a list, the next section's heading line
/// (`  ## x`) would otherwise continue that list, and which section follows (one the
/// caller may not see, or another) would decide what the change does. The heading's
/// level, title and attributes are unchanged.
fn heading_at_margin(text: &str) -> std::borrow::Cow<'_, str> {
    let indent = text.bytes().take_while(|b| *b == b' ').count();
    let rest = &text[indent..];
    if (1..=3).contains(&indent) && rest.starts_with(|c: char| !c.is_whitespace()) {
        std::borrow::Cow::Owned(rest.to_owned())
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

/// The real text of a change to a document with sections the caller cannot see: the
/// caller's text (`content`, compiled as `written`) with each run of hidden sections of
/// the old text put back by its own span before the visible section that followed it,
/// or the nearest later one the text still has, at worst at the end. A document the
/// change leaves as it was comes back byte for byte.
fn reinsert(
    old: &str,
    outline: &[SectionInfo],
    content: &str,
    written: &[SectionInfo],
    judge: &mut Judge<'_>,
) -> Planned<String> {
    let mut visible = Vec::with_capacity(outline.len());
    for section in outline {
        visible.push(judge.read(&section.chain)?);
    }
    // Each run of hidden sections, with the visible sections after it, nearest first.
    let mut runs: Vec<(Vec<&str>, String)> = Vec::new();
    let mut run = String::new();
    for (index, section) in outline.iter().enumerate() {
        if !visible[index] {
            run.push_str(old.get(section.start..section.end).unwrap_or(""));
            continue;
        }
        if !run.is_empty() {
            let after: Vec<&str> = outline[index..]
                .iter()
                .zip(&visible[index..])
                .filter(|(_, seen)| **seen)
                .map(|(s, _)| s.id.as_str())
                .collect();
            runs.push((after, std::mem::take(&mut run)));
        }
    }
    if !run.is_empty() {
        runs.push((Vec::new(), run));
    }
    let mut placed: Vec<(usize, usize, String)> = Vec::new();
    for (order, (after, text)) in runs.into_iter().enumerate() {
        let at = after
            .iter()
            .find_map(|id| written.iter().skip(1).find(|s| s.ids.iter().any(|i| i == id)))
            .map_or(content.len(), |section| section.start);
        placed.push((at, order, text));
    }
    // From the end, so earlier offsets stay valid; runs at one place keep their order.
    placed.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    let newline = if content.contains("\r\n") { "\r\n" } else { "\n" };
    let blank = |text: &str| text.ends_with("\n\n") || text.ends_with("\r\n\r\n");
    let mut out = content.to_owned();
    for (at, _, text) in placed {
        let (head, tail) = out.split_at(at);
        let mut next = String::with_capacity(out.len() + text.len() + 8);
        next.push_str(head);
        if !head.is_empty() && !head.ends_with('\n') {
            next.push_str(newline);
        }
        if !head.is_empty() && !blank(&next) {
            next.push_str(newline);
        }
        let text = if text.starts_with(' ') && !ends_closed(head, "   ") {
            heading_at_margin(&text).into_owned()
        } else {
            text
        };
        next.push_str(&text);
        if !tail.is_empty() {
            if !text.ends_with('\n') {
                next.push_str(newline);
            }
            if !blank(&next) {
                next.push_str(newline);
            }
            if tail.starts_with(' ') && !ends_closed(&text, "   ") {
                next.push_str(&heading_at_margin(tail));
            } else {
                next.push_str(tail);
            }
        }
        out = next;
    }
    Ok(out)
}

/// A section body must not change the document around it: it must close every block
/// it opens (a code fence, an HTML block), so the next heading stays a heading. Returns
/// whether it ends inside a list that would take in a following line indented by up to
/// three spaces, such as an indented heading.
fn check_body(id: &str, body: &str) -> Planned<bool> {
    if !ends_closed(body, "") {
        return refused(format!(
            "{id}: the new body leaves a code block, HTML block or other construct open; close it"
        ));
    }
    Ok(!ends_closed(body, "   "))
}

/// Whether a heading indented by `indent`, after `text` and a blank line, is a heading
/// of the document (not inside a code block, an HTML block, a list or a quote).
fn ends_closed(text: &str, indent: &str) -> bool {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};
    const PROBE: &str = "kiwi end of body probe";
    let probed = format!("{}\n\n{indent}# {PROBE}\n", text.trim_end());
    let mut depth = 0usize;
    let mut last_top_heading = None;
    let mut heading: Option<String> = None;
    for event in Parser::new_ext(&probed, crate::parse::markdown_options()) {
        match event {
            Event::Start(Tag::Heading { .. }) => heading = Some(String::new()),
            Event::End(TagEnd::Heading(_)) if depth == 0 => last_top_heading = heading.take(),
            Event::Text(text) => {
                if let Some(h) = heading.as_mut() {
                    h.push_str(&text);
                }
            }
            Event::Start(Tag::BlockQuote(_) | Tag::Item | Tag::FootnoteDefinition(_)) => depth += 1,
            Event::End(TagEnd::BlockQuote(_) | TagEnd::Item | TagEnd::FootnoteDefinition) => {
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
    }
    last_top_heading.as_deref() == Some(PROBE)
}

/// Lints the proposed corpus as the caller sees it (`view`, with the changes applied)
/// and returns the findings on the changed documents.
fn lint_changes(
    view: &Snapshot,
    config: &crate::KiwiConfig,
    base: &BTreeMap<String, CompiledDoc>,
    new_docs: &BTreeMap<String, CompiledDoc>,
    changes: &[Change],
) -> Planned<Vec<Finding>> {
    let removed: BTreeSet<&str> = changes
        .iter()
        .filter(|c| c.content.is_none())
        .map(|c| c.path.as_str())
        .collect();
    let mut docs: Vec<CompiledDoc> = base
        .iter()
        .filter(|(file, _)| !removed.contains(file.as_str()) && !new_docs.contains_key(*file))
        .map(|(_, doc)| doc.clone())
        .collect();
    docs.extend(new_docs.values().cloned());
    docs.sort_by(|a, b| a.path.cmp(&b.path));
    check_unique_ids(&docs).map_err(|e| ProposeError::Refused(e.to_string()))?;
    let input = LinkInput {
        namespace: &config.namespace,
        snapshot: &view.id,
        source: view.kind,
        as_of: &view.as_of,
        config_hash: config.build_hash(),
    };
    let data = link(&docs, &input, &[], None, None).map_err(|e| ProposeError::Refused(e.to_string()))?;
    let pack = kiwi_core::pack::encode(&data)
        .and_then(|bytes| Pack::from_bytes(&bytes))
        .map_err(|e| ProposeError::Failed(e.into()))?;
    let changed: BTreeSet<String> = changes
        .iter()
        .filter(|c| c.content.is_some())
        .map(|c| c.path.trim_end_matches(".md").to_owned())
        .collect();
    let findings = BuiltinLint
        .check(&pack)
        .map_err(|e| ProposeError::Failed(e.into()))?;
    Ok(findings
        .into_iter()
        .filter(|f| changed.contains(f.id.split('#').next().unwrap_or("")))
        .collect())
}

/// `text` (the source `sections` were compiled from) without the sections whose access
/// chain `readable` refuses, cut by the parser's spans. The bytes before the first
/// section (frontmatter) are kept. At each cut a blank line separates the text before it
/// from the section after it (text ended by a hidden ATX heading would otherwise run
/// into a setext heading or continue a list or quote), and a kept heading indented by
/// up to three spaces that now follows a list is moved to the margin, so every kept
/// section parses as it did.
pub fn strip_hidden<E>(
    text: &str,
    sections: &[SectionSpan],
    mut readable: impl FnMut(&[AccessDecl]) -> std::result::Result<bool, E>,
) -> std::result::Result<String, E> {
    let span = |s: &SectionSpan| text.get(s.start as usize..s.end as usize).unwrap_or("");
    let mut out = String::with_capacity(text.len());
    let first = sections.first().map_or(text.len(), |s| s.start as usize);
    out.push_str(text.get(..first).unwrap_or(""));
    let mut dropped = false;
    for section in sections {
        if !readable(&section.chain)? {
            dropped = true;
            continue;
        }
        let kept = span(section);
        if dropped && !out.is_empty() {
            let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
            if !out.ends_with('\n') {
                out.push_str(newline);
            }
            if !(out.ends_with("\n\n") || out.ends_with("\r\n\r\n")) {
                out.push_str(newline);
            }
        }
        if dropped && kept.starts_with(' ') && !ends_closed(&out, "   ") {
            out.push_str(&heading_at_margin(kept));
        } else {
            out.push_str(kept);
        }
        dropped = false;
    }
    Ok(out)
}

fn signature_time() -> String {
    format!("{} +0000", date::now_unix())
}

/// Commits a [`Plan`] on a new branch on top of its base commit and opens the pull
/// request (or records a local proposal).
pub fn open(
    target: &ProposalTarget,
    plan: &Plan,
    title: &str,
    rationale: &str,
    author: &Author,
) -> Result<Proposal> {
    // The changes may be empty: every one the caller asked for can have been left out
    // for running into content they cannot see; the proposal opens all the same, for an
    // owner, so its outcome never depends on that content.
    let changes = &plan.changes;
    for change in changes {
        check_path(&change.path)?;
    }
    let location = plan
        .snapshot
        .repo
        .clone()
        .ok_or_else(|| CompileError::git("proposals need the corpus in a Git repository"))?;
    let repo = gix::open(&location.repo_path).map_err(CompileError::git)?;
    let base_id = repo
        .rev_parse_single(location.commit.as_str())
        .map_err(CompileError::git)?
        .detach();
    let base_commit = repo
        .find_object(base_id)
        .map_err(CompileError::git)?
        .peel_to_commit()
        .map_err(CompileError::git)?;
    let base_tree = base_commit.tree_id().map_err(CompileError::git)?.detach();
    let mut editor = repo.edit_tree(base_tree).map_err(CompileError::git)?;
    let mut files = Vec::new();
    for change in changes {
        let full = if location.prefix.is_empty() {
            change.path.clone()
        } else {
            format!("{}/{}", location.prefix, change.path)
        };
        match &change.content {
            Some(content) => {
                let blob = repo.write_blob(content.as_bytes()).map_err(CompileError::git)?;
                editor
                    .upsert(full.as_str(), gix::object::tree::EntryKind::Blob, blob.detach())
                    .map_err(CompileError::git)?;
            }
            None => {
                editor.remove(full.as_str()).map_err(CompileError::git)?;
            }
        }
        files.push(change.path.clone());
    }
    // Whether the change modifies the corpus was decided on the caller's view; the real
    // tree may repeat the base (a hidden document replaced byte for byte), and the
    // proposal opens all the same, so its outcome never depends on hidden content.
    let tree = editor.write().map_err(CompileError::git)?.detach();
    // Fail closed: the commit changes exactly the files the plan names.
    let named: BTreeSet<String> = changes
        .iter()
        .map(|change| {
            if location.prefix.is_empty() {
                change.path.clone()
            } else {
                format!("{}/{}", location.prefix, change.path)
            }
        })
        .collect();
    let (before, after) = (blobs(&repo, base_tree)?, blobs(&repo, tree)?);
    let touched = before
        .iter()
        .filter(|(path, id)| after.get(*path) != Some(*id))
        .map(|(path, _)| path)
        .chain(after.keys().filter(|path| !before.contains_key(*path)));
    for path in touched {
        if !named.contains(path) {
            return Err(CompileError::Config(format!(
                "the commit would change {path}, which the proposal does not name"
            )));
        }
    }
    // The same change proposed twice (by one user through two agents, or again later)
    // gets its own branch.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut hasher = blake3::Hasher::new();
    hasher.update(tree.as_bytes());
    hasher.update(author.principal.as_bytes());
    hasher.update(b"\0");
    hasher.update(author.agent.as_bytes());
    hasher.update(b"\0");
    hasher.update(title.as_bytes());
    hasher.update(&nanos.to_le_bytes());
    let short = &hasher.finalize().to_hex()[..10];
    let today = date::today().replace('-', "");
    let user: String = author
        .principal
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let id = format!("{today}-{short}");
    let branch = format!("kiwi/propose/{user}/{id}");
    let body = if plan.owner_review {
        format!("{rationale}\n\n{OWNER_REVIEW_NOTICE}")
    } else {
        rationale.to_owned()
    };
    let message = format!(
        "{title}\n\n{body}\n\nProposed-by: {} via {}\n",
        author.principal, author.agent
    );
    let time = signature_time();
    let signature = gix::actor::SignatureRef {
        name: BStr::new(author.name.as_str()),
        email: BStr::new(author.email.as_str()),
        time: time.as_str(),
    };
    let commit = repo
        .commit_as(
            signature,
            signature,
            format!("refs/heads/{branch}").as_str(),
            message,
            tree,
            [base_id],
        )
        .map_err(CompileError::git)?
        .detach()
        .to_string();
    let mut unapplied = Vec::new();
    let link = match target.forge.kind.as_str() {
        "github" => {
            let request = PullRequest {
                base: &target.forge.base,
                branch: &branch,
                title,
                body: &body,
                owner_review: plan.owner_review,
                reviewers: plan
                    .review_owners
                    .iter()
                    .filter_map(|o| github_reviewer(o))
                    .collect(),
            };
            for owner in plan.review_owners.iter().filter(|o| github_reviewer(o).is_none()) {
                unapplied.push(format!(
                    "owner `{owner}` is not a GitHub handle; no review requested"
                ));
            }
            let (url, failed) = open_pull_request(&location.repo_path, &target.forge, &request)?;
            unapplied.extend(failed);
            url
        }
        _ => format!("kiwi://proposal/{id}"),
    };
    let proposal = Proposal {
        id: id.clone(),
        branch,
        commit,
        base: base_id.to_string(),
        link,
        title: title.to_owned(),
        rationale: rationale.to_owned(),
        principal: author.principal.clone(),
        agent: author.agent.clone(),
        files,
        merge: plan.merge.clone(),
        owner_review: plan.owner_review,
        review_owners: plan.review_owners.clone(),
        owner_review_unapplied: unapplied,
        lint: plan.lint.clone(),
        created: date::timestamp(date::now_unix()),
    };
    let dir = target.state_dir.join("proposals");
    std::fs::create_dir_all(&dir).map_err(|e| CompileError::source(&dir, e))?;
    let json = serde_json::to_vec_pretty(&proposal).map_err(|e| CompileError::Config(e.to_string()))?;
    kiwi_core::pack::write_atomic(&dir.join(format!("{id}.json")), &json)?;
    Ok(proposal)
}

/// The label of a pull request an owner must review.
pub const OWNER_REVIEW_LABEL: &str = "kiwi:owner-review";

/// The GitHub reviewer an owner names: `@user` is the user, `@org/team` the team;
/// anything else (an e-mail address) names no GitHub account.
pub fn github_reviewer(owner: &str) -> Option<&str> {
    let handle = owner.strip_prefix('@')?;
    let valid = !handle.is_empty()
        && handle.split('/').count() <= 2
        && handle
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '.'));
    valid.then_some(handle)
}

/// The paths and blob ids of a tree, recursively.
fn blobs(repo: &gix::Repository, tree: gix::ObjectId) -> Result<BTreeMap<String, gix::ObjectId>> {
    let tree = repo
        .find_object(tree)
        .map_err(CompileError::git)?
        .peel_to_tree()
        .map_err(CompileError::git)?;
    let mut recorder = gix::traverse::tree::Recorder::default();
    tree.traverse()
        .breadthfirst(&mut recorder)
        .map_err(CompileError::git)?;
    Ok(recorder
        .records
        .into_iter()
        .filter(|entry| !entry.mode.is_tree())
        .map(|entry| (entry.filepath.to_string(), entry.oid))
        .collect())
}

/// The pull request a proposal opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest<'a> {
    /// The base branch.
    pub base: &'a str,
    /// The proposal's branch.
    pub branch: &'a str,
    /// Title.
    pub title: &'a str,
    /// Body.
    pub body: &'a str,
    /// An owner must review it: it opens as a draft labelled [`OWNER_REVIEW_LABEL`].
    pub owner_review: bool,
    /// GitHub users and teams whose review is requested (with `owner_review`).
    pub reviewers: Vec<&'a str>,
}

/// The `gh pr create` arguments of `request`: with `owner_review` the pull request is a
/// draft with its label from the moment it exists. Reviews are never requested here:
/// `gh` creates the pull request before it requests them, so a rejected reviewer would
/// fail a call whose pull request exists.
pub fn pr_create_args(request: &PullRequest<'_>) -> Vec<String> {
    let mut args: Vec<String> = [
        "pr",
        "create",
        "--base",
        request.base,
        "--head",
        request.branch,
        "--title",
        request.title,
        "--body",
        request.body,
    ]
    .iter()
    .map(|a| (*a).to_owned())
    .collect();
    if request.owner_review {
        args.extend(["--draft", "--label", OWNER_REVIEW_LABEL].map(str::to_owned));
    }
    args
}

/// Pushes the branch and opens the pull request with the GitHub CLI. Returns its URL
/// and what could not be done for an owner's review.
fn open_pull_request(
    repo: &Path,
    forge: &ForgeConfig,
    request: &PullRequest<'_>,
) -> Result<(String, Vec<String>)> {
    let output = Command::new("git")
        .args([
            "push",
            forge.remote.as_str(),
            &format!("{}:{}", request.branch, request.branch),
        ])
        .current_dir(repo)
        .output()
        .map_err(|e| CompileError::git(format!("git: {e}")))?;
    if !output.status.success() {
        return Err(CompileError::git(format!(
            "git push: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    create_pull_request(Path::new("gh"), repo, request)
}

/// Opens the pull request of `request` with the GitHub CLI `gh` (the program at that
/// path), in the repository `repo`.
///
/// An `owner_review` pull request is created as a draft with its label in one call (the
/// label is created first if missing), so it is never open for merging without them;
/// its owners' reviews are then requested one by one, best effort. After any failed
/// create, the pull request of the branch is looked up before anything is retried, so
/// a create that failed after making the pull request never makes a second one. When
/// GitHub rejects the draft or the label (drafts are not available on every plan), the
/// pull request is created plain, as any other proposal's, and then marked. It opens
/// whenever a plain one would, so whether it opens never depends on content the author
/// cannot see; what failed is returned for the operator.
fn create_pull_request(gh: &Path, repo: &Path, request: &PullRequest<'_>) -> Result<(String, Vec<String>)> {
    let run = |args: &[String]| -> std::result::Result<String, String> {
        let output = Command::new(gh)
            .args(args)
            .current_dir(repo)
            .output()
            .map_err(|e| format!("gh: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "gh {}: {}",
                args.iter().take(2).cloned().collect::<Vec<_>>().join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    let args = |parts: &[&str]| -> Vec<String> { parts.iter().map(|p| (*p).to_owned()).collect() };
    // The pull request of the branch, if one exists.
    let existing = || {
        run(&args(&[
            "pr",
            "view",
            request.branch,
            "--json",
            "url",
            "--jq",
            ".url",
        ]))
        .ok()
        .filter(|url| !url.is_empty())
    };
    let mut failed = Vec::new();
    if request.owner_review {
        failed.extend(
            run(&args(&[
                "label",
                "create",
                OWNER_REVIEW_LABEL,
                "--force",
                "--color",
                "B60205",
                "--description",
                "Kiwi: conflicts with content the author cannot see; an owner must review",
            ]))
            .err(),
        );
    }
    // (url, created as a draft with the label)
    let mut opened: Option<(String, bool)> = None;
    match run(&pr_create_args(request)) {
        Ok(out) => opened = Some((out, request.owner_review)),
        Err(error) => {
            failed.push(error);
            if let Some(url) = existing() {
                opened = Some((url, false));
            }
        }
    }
    if opened.is_none() && request.owner_review {
        let plain = PullRequest {
            owner_review: false,
            ..request.clone()
        };
        match run(&pr_create_args(&plain)) {
            Ok(out) => opened = Some((out, false)),
            Err(error) => {
                failed.push(error);
                opened = existing().map(|url| (url, false));
            }
        }
    }
    let Some((out, marked)) = opened else {
        return Err(CompileError::git(failed.join("; ")));
    };
    if !request.owner_review {
        // A plain pull request: nothing was owed to an owner's review.
        failed.clear();
    }
    let url = out.lines().last().unwrap_or("").to_owned();
    if request.owner_review {
        if !marked {
            failed.extend(run(&args(&["pr", "ready", &url, "--undo"])).err());
            failed.extend(run(&args(&["pr", "edit", &url, "--add-label", OWNER_REVIEW_LABEL])).err());
        }
        for reviewer in &request.reviewers {
            failed.extend(run(&args(&["pr", "edit", &url, "--add-reviewer", reviewer])).err());
        }
    }
    Ok((url, failed))
}

/// Sets one top-level frontmatter key to a YAML value (written on one line), keeping
/// every other line, comment, the key order and the file's line endings. Creates the
/// frontmatter when the file has none. The result is parsed again and must hold exactly
/// the old keys with this one set, or the change is refused.
pub fn set_frontmatter(text: &str, key: &str, yaml_value: &str) -> Result<String> {
    let bad = |message: String| CompileError::Config(message);
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
        return Err(bad(format!(
            "frontmatter key `{key}` is not lowercase snake_case"
        )));
    }
    if yaml_value.contains('\n') || yaml_value.contains('\r') {
        return Err(bad("frontmatter values must fit on one line".into()));
    }
    let value: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(yaml_value).map_err(|e| bad(format!("value of `{key}`: {e}")))?;
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let new_line = format!("{key}: {yaml_value}");
    let (yaml, _) = crate::parse::split_frontmatter(text).map_err(bad)?;
    let (out, old_yaml, new_yaml) = match yaml {
        None => {
            let bom = if text.starts_with('\u{feff}') {
                '\u{feff}'.len_utf8()
            } else {
                0
            };
            let yaml = format!("{new_line}{newline}");
            let out = format!(
                "{}---{newline}{yaml}---{newline}{newline}{}",
                &text[..bom],
                &text[bom..]
            );
            (out, String::new(), yaml)
        }
        Some(range) => {
            let old = &text[range.clone()];
            let mut lines: Vec<String> = Vec::new();
            let mut replaced = false;
            let mut skipping = false;
            for line in old.split_inclusive('\n') {
                let bare = line.trim_end_matches(['\n', '\r']);
                if skipping && (bare.starts_with(' ') || bare.starts_with('\t') || bare.starts_with("- ")) {
                    continue;
                }
                skipping = false;
                if bare.split(':').next() == Some(key) && bare.contains(':') {
                    lines.push(format!("{new_line}{newline}"));
                    replaced = true;
                    skipping = true;
                    continue;
                }
                lines.push(line.to_owned());
            }
            if !replaced {
                if lines.last().is_some_and(|l| !l.ends_with('\n')) {
                    lines.push(newline.to_owned());
                }
                lines.push(format!("{new_line}{newline}"));
            }
            let yaml = lines.concat();
            let out = format!("{}{yaml}{}", &text[..range.start], &text[range.end..]);
            (out, old.to_owned(), yaml)
        }
    };
    let mapping = |yaml: &str| -> Result<serde_yaml_ng::Mapping> {
        match serde_yaml_ng::from_str::<serde_yaml_ng::Value>(yaml).map_err(|e| bad(e.to_string()))? {
            serde_yaml_ng::Value::Null => Ok(serde_yaml_ng::Mapping::new()),
            serde_yaml_ng::Value::Mapping(m) => Ok(m),
            _ => Err(bad("frontmatter is not a mapping".into())),
        }
    };
    let mut expected = mapping(&old_yaml)?;
    expected.insert(serde_yaml_ng::Value::String(key.to_owned()), value);
    if mapping(&new_yaml)? != expected {
        return Err(bad(format!(
            "`{key}` cannot be set in place; change the whole file instead"
        )));
    }
    Ok(out)
}

/// Whether findings include errors.
pub fn has_errors(findings: &[Finding]) -> bool {
    findings.iter().any(|f| f.severity == Severity::Error)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// A reader in some groups: public and internal content (unless limited to other
    /// groups) and confidential content of their groups. `merge` lets them merge
    /// public and internal content without review.
    struct Reader {
        groups: &'static [&'static str],
        merge: bool,
    }

    impl Permissions for Reader {
        fn read(&self, chain: &[AccessDecl]) -> std::result::Result<bool, String> {
            Ok(!chain.is_empty()
                && chain.iter().all(|d| {
                    let member = d.groups.iter().any(|g| self.groups.contains(&g.as_str()));
                    match d.label.as_str() {
                        "public" | "internal" => d.groups.is_empty() || member,
                        "confidential" => member,
                        _ => false,
                    }
                }))
        }

        fn propose(&self, chain: &[AccessDecl]) -> std::result::Result<bool, String> {
            self.read(chain)
        }

        fn merge(&self, chain: &[AccessDecl]) -> std::result::Result<bool, String> {
            Ok(self.merge
                && self.read(chain)?
                && chain
                    .iter()
                    .all(|d| matches!(d.label.as_str(), "public" | "internal")))
        }
    }

    const ALICE: Reader = Reader {
        groups: &["eng"],
        merge: false,
    };
    const PAUL: Reader = Reader {
        groups: &["eng"],
        merge: true,
    };
    const FIONA: Reader = Reader {
        groups: &["eng", "finance"],
        merge: true,
    };

    fn long() -> String {
        "A sentence that is long enough to count for the size rule of sections. ".repeat(10)
    }

    /// The test corpus: `doc.md` has a finance section between two visible ones (its
    /// heading indented, which a line scanner does not see); `notes/plan.md` is hidden
    /// from non-finance readers by its frontmatter; `finance/` by its path rule.
    fn files() -> Vec<(String, String)> {
        let long = long();
        vec![
            (
                "KIWIOWNERS".into(),
                "* label=internal\nfinance/** label=confidential groups=finance\n".into(),
            ),
            (
                "doc.md".into(),
                format!(
                    "---\nid: DOC-1\nowner: \"@eng\"\nsummary: The doc.\n---\n# Doc\n\n{long}\n\n## Open {{#open}}\n\nOld open.\n\n  ## Secret {{#secret access=finance}}\n\nHidden words {long}\n\n## After {{#after}}\n\nAfter text.\n"
                ),
            ),
            (
                "plain.md".into(),
                format!("---\nid: PLAIN-1\n---\n# Plain\n\n{long}\n\n## Part {{#part}}\n\n{long}\n"),
            ),
            (
                "notes/plan.md".into(),
                format!("---\nid: PLAN-7\naccess: finance\n---\n# Plan\n\n{long}\n"),
            ),
            (
                "finance/terms.md".into(),
                "---\ntype: glossary\n---\n# Finance terms\n\n## Margin {#margin}\n\nWhat is left.\n".into(),
            ),
        ]
    }

    struct Repo {
        _dir: tempfile::TempDir,
        root: PathBuf,
    }

    fn git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(["-c", "user.email=t@test", "-c", "user.name=T"])
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }

    fn repo(files: &[(String, String)]) -> Repo {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("kb");
        for (path, content) in files {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, content).unwrap();
        }
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "init"]);
        Repo { _dir: dir, root }
    }

    fn target(repo: &Repo) -> ProposalTarget {
        ProposalTarget {
            corpus: repo.root.clone(),
            base: "HEAD".into(),
            forge: ForgeConfig::default(),
            state_dir: repo.root.join(".kiwi/state"),
            cache_dir: repo.root.join(".kiwi/packs/cache"),
        }
    }

    fn section(id: &str, body: &str) -> Edit {
        Edit::Section {
            id: id.into(),
            body: body.into(),
        }
    }

    fn write(path: &str, content: &str) -> Edit {
        Edit::Write {
            path: path.into(),
            content: content.into(),
        }
    }

    fn set(path: &str, key: &str, value: serde_json::Value) -> Edit {
        Edit::Set {
            path: path.into(),
            keys: BTreeMap::from([(key.to_owned(), value)]),
        }
    }

    fn content<'a>(plan: &'a Plan, path: &str) -> &'a str {
        plan.changes
            .iter()
            .find(|c| c.path == path)
            .and_then(|c| c.content.as_deref())
            .unwrap()
    }

    fn refusal(outcome: Planned<Plan>) -> String {
        match outcome {
            Err(ProposeError::Refused(message)) => message,
            Err(ProposeError::Failed(e)) => panic!("expected a refusal, got a failure: {e}"),
            Ok(plan) => panic!("expected a refusal, got {:?}", plan.changes),
        }
    }

    #[test]
    fn section_edits_follow_the_parser_and_keep_hidden_sections() {
        let repo = repo(&files());
        let plan = plan(&target(&repo), &ALICE, &[section("doc#open", "New open.")]).unwrap();
        let text = content(&plan, "doc.md");
        assert!(
            text.contains(
                "## Open {#open}\n\nNew open.\n\n  ## Secret {#secret access=finance}\n\nHidden words"
            ),
            "{text}"
        );
        assert!(text.contains("## After {#after}\n\nAfter text.\n"), "{text}");
        assert_eq!(plan.merge, "review");
        // `after` is merged into the root atom; its edit changes only its own span.
        let plan = plan_of(&repo, &PAUL, &[section("doc#after", "Later text.")]);
        let text = content(&plan, "doc.md");
        assert!(
            text.contains("Old open.") && text.ends_with("## After {#after}\n\nLater text.\n"),
            "{text}"
        );
        assert_eq!(plan.merge, "direct");
        // The hidden section is unknown to alice, a part of a split section is not a
        // section, and the root can be named by its document id.
        assert!(refusal(plan_try(&repo, &ALICE, &[section("doc#secret", "x")])).contains("no section"));
        let plan = plan_of(&repo, &ALICE, &[section("DOC-1", "A new introduction.")]);
        assert!(content(&plan, "doc.md").contains("# Doc\n\nA new introduction.\n\n## Open"));
    }

    fn plan_try(repo: &Repo, who: &Reader, edits: &[Edit]) -> Planned<Plan> {
        plan(&target(repo), who, edits)
    }

    fn plan_of(repo: &Repo, who: &Reader, edits: &[Edit]) -> Plan {
        plan_try(repo, who, edits).unwrap()
    }

    #[test]
    fn section_bodies_close_what_they_open_and_restructure_only_with_review() {
        let repo = repo(&files());
        let open = refusal(plan_try(
            &repo,
            &PAUL,
            &[section("doc#open", "```\nunclosed fence")],
        ));
        assert!(open.contains("open"), "{open}");
        let html = refusal(plan_try(
            &repo,
            &PAUL,
            &[section("doc#open", "<!-- an open comment")],
        ));
        assert!(html.contains("open"), "{html}");
        // Headings in a body add sections, whoever proposes and whatever they see; the
        // hidden section is put back where it was.
        for who in [&PAUL, &FIONA] {
            let plan = plan_of(
                &repo,
                who,
                &[section("doc#open", "Text.\n\n### Sub {#sub}\n\nMore.")],
            );
            let text = content(&plan, "doc.md");
            assert!(
                text.contains("### Sub {#sub}\n\nMore.\n\n  ## Secret {#secret access=finance}"),
                "{text}"
            );
            assert_eq!(plan.merge, "direct");
            assert!(!plan.owner_review, "{:?}", plan.notes);
        }
        // Demoting the visible section under the hidden one's level would give the
        // visible text a hidden parent: decided on what paul sees, reviewed by an owner.
        let demoted = plan_of(
            &repo,
            &PAUL,
            &[section(
                "doc#open",
                "Text.\n\n### Sub {#sub}\n\nMore.\n\n# Top {#top}\n\nNew top.",
            )],
        );
        assert_eq!(demoted.merge, "direct");
        assert!(demoted.owner_review, "{:?}", demoted.notes);
        // A body that ends in a list does not take in the indented heading after it.
        let plan = plan_of(&repo, &PAUL, &[section("doc#open", "Items:\n\n- one\n- two")]);
        assert!(
            content(&plan, "doc.md").contains("- two\n\n## Secret {#secret access=finance}"),
            "{}",
            content(&plan, "doc.md")
        );
        assert_eq!(plan.merge, "direct");
        assert!(
            refusal(plan_try(
                &repo,
                &PAUL,
                &[section("doc#open", "a"), section("doc#open", "b")]
            ))
            .contains("twice")
        );
        assert!(
            refusal(plan_try(&repo, &PAUL, &[section("doc#open", "Old open.")])).contains("do not modify")
        );
    }

    #[test]
    fn merge_needs_every_touched_chain() {
        let repo = repo(&files());
        // Fiona may merge internal content only: an edit of the finance section needs
        // review, one of a visible section does not.
        assert_eq!(
            plan_of(&repo, &FIONA, &[section("doc#secret", "New.")]).merge,
            "review"
        );
        assert_eq!(
            plan_of(&repo, &FIONA, &[section("doc#open", "New.")]).merge,
            "direct"
        );
        assert_eq!(
            plan_of(
                &repo,
                &FIONA,
                &[section("doc#open", "New."), section("doc#secret", "New.")]
            )
            .merge,
            "review"
        );
    }

    #[test]
    fn access_changes_need_merge_rights_and_review() {
        let repo = repo(&files());
        let public = serde_json::json!({"label": "public"});
        let plan = plan_of(&repo, &PAUL, &[set("plain.md", "access", public.clone())]);
        assert_eq!(plan.merge, "review", "an access change is never merged directly");
        assert!(content(&plan, "plain.md").contains("access: {\"label\":\"public\"}"));
        let denied = refusal(plan_try(
            &repo,
            &ALICE,
            &[set("plain.md", "access", public.clone())],
        ));
        assert!(denied.contains("merge"), "{denied}");
        let finance = refusal(plan_try(
            &repo,
            &PAUL,
            &[set("plain.md", "access", "finance".into())],
        ));
        assert!(finance.contains("may not propose"), "{finance}");
        // A document with sections paul cannot see: the change is reviewed and noted,
        // never refused for them.
        let hidden = plan_of(&repo, &PAUL, &[set("doc.md", "access", public)]);
        assert_eq!(hidden.merge, "review");
        assert!(
            hidden.notes.iter().any(|n| n.contains("cannot see")),
            "{:?}",
            hidden.notes
        );
        // A whole-file change that narrows a section is an access change too.
        let text = std::fs::read_to_string(repo.root.join("plain.md"))
            .unwrap()
            .replace("## Part {#part}", "## Part {#part label=public}");
        assert_eq!(plan_of(&repo, &PAUL, &[write("plain.md", &text)]).merge, "review");
        // Other keys are not.
        assert_eq!(
            plan_of(&repo, &PAUL, &[set("plain.md", "status", "approved".into())]).merge,
            "direct"
        );
    }

    /// Section bodies built from a fixed random sequence of Markdown pieces: headings of
    /// every level (some named like the hidden section), fences open and closed, lists,
    /// quotes, HTML blocks, setext headings and link definitions.
    fn fuzzed_bodies(count: usize) -> Vec<String> {
        const PIECES: [&str; 20] = [
            "# One {#one}",
            "## Two",
            "### Secret",
            "## Secret {#secret}",
            "#### Four {#four}",
            "##### Five",
            "###### Six",
            "  ## Indented {#indented}",
            "```\ncode\n```",
            "```\nan open fence",
            "- item\n- item",
            "1. one\n2. two",
            "<div>\nhtml\n</div>",
            "<!-- an open comment",
            "Plain text.",
            "> quoted",
            "Setext\n---",
            "[ref]: plain.md",
            "    indented code",
            "Text with [a link](plain.md).",
        ];
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        (0..count)
            .map(|_| {
                let pieces = 1 + (next() % 4) as usize;
                (0..pieces)
                    .map(|_| PIECES[(next() % PIECES.len() as u64) as usize])
                    .collect::<Vec<_>>()
                    .join("\n\n")
            })
            .collect()
    }

    #[test]
    fn hidden_documents_answer_like_absent_ones() {
        let full = repo(&files());
        let visible: Vec<(String, String)> = files()
            .into_iter()
            .filter(|(path, _)| path != "notes/plan.md" && !path.starts_with("finance/"))
            .map(|(path, text)| {
                let text = if path == "doc.md" {
                    let start = text.find("  ## Secret").unwrap();
                    let end = text.find("## After").unwrap();
                    format!("{}{}", &text[..start], &text[end..])
                } else {
                    text
                };
                (path, text)
            })
            .collect();
        let stripped = repo(&visible);
        let rewritten = visible
            .iter()
            .find(|(path, _)| path == "doc.md")
            .map(|(_, text)| text.replace("Old open.", "Rewritten open.\n\n- a list\n- at the end"))
            .unwrap();
        let new_doc = "---\nid: NEW-1\nowner: \"@eng\"\nsummary: S.\nsupersedes: [PLAN-7]\n---\n# New\n\nSee [plan](../notes/plan.md) and [gone](gone.md).\n";
        let same_id = "---\nid: PLAN-7\nowner: \"@eng\"\nsummary: S.\n---\n# Same id\n";
        let glossary = "---\ntype: glossary\nowner: \"@eng\"\nsummary: S.\n---\n# Terms\n\n## Margin {#margin}\n\nWhat remains.\n";
        let cases: Vec<Vec<Edit>> = vec![
            vec![write("notes/plan.md", "# Mine\n\nA plan of my own.\n")],
            vec![write("Notes/Plan.md", "# Mine\n\nA plan of my own.\n")],
            vec![Edit::Delete {
                path: "notes/plan.md".into(),
            }],
            vec![set("notes/plan.md", "status", "draft".into())],
            vec![write("adr/new.md", new_doc)],
            vec![write("adr/same.md", same_id)],
            vec![write("terms.md", glossary)],
            vec![section("doc#open", "Edited.")],
            vec![section("PLAN-7", "Edited.")],
            // Changes that could reach the hidden section of doc.md.
            vec![section("doc#open", "Text.\n\n### Sub {#sub}\n\nMore.")],
            vec![section("doc#open", "Items:\n\n- one\n- two")],
            vec![section("doc#open", "See [x].\n\n[x]: plain.md")],
            vec![Edit::Delete {
                path: "doc.md".into(),
            }],
            vec![write("doc.md", &rewritten)],
            vec![set("doc.md", "access", serde_json::json!({"label": "public"}))],
            vec![set("doc.md", "status", "approved".into())],
            // Probes of the hidden section: its anchor and title, a bad value, a
            // demotion under its level, a block left open where it is put back, and
            // several changes at once, where an earlier one must not be an oracle.
            vec![section("doc#open", "Mine.\n\n### Secret {#secret}\n\nMine too.")],
            vec![section("doc#open", "Mine.\n\n### Secret\n\nMine too.")],
            vec![set("doc.md", "status", "bogus".into())],
            vec![section("doc#open", "Text.\n\n# Top {#top}\n\nA new top section.")],
            vec![section("doc#open", "```\nan open fence")],
            vec![write("doc.md", &format!("{rewritten}\n```\nan open fence\n"))],
            vec![write("doc.md", &rewritten.replace("## After", "#### After"))],
            vec![
                section("doc#open", "Mine.\n\n### Secret {#secret}\n\nMine too."),
                set("doc.md", "status", "bogus".into()),
            ],
            vec![
                section("doc#open", "Mine.\n\n## Secret {#secret}\n\nMine too."),
                section("doc#after", "Later."),
            ],
            vec![
                Edit::Delete {
                    path: "doc.md".into(),
                },
                set("plain.md", "status", "bogus".into()),
            ],
        ];
        let mut cases = cases;
        cases.extend(fuzzed_bodies(40).into_iter().flat_map(|body| {
            [
                vec![section("doc#open", &body)],
                vec![write(
                    "doc.md",
                    &format!("---\nid: DOC-1\nowner: \"@eng\"\nsummary: The doc.\n---\n# Doc\n\n{body}\n"),
                )],
            ]
        }));
        for edits in cases {
            for who in [&ALICE, &PAUL] {
                let answer = |repo: &Repo| match plan_try(repo, who, &edits) {
                    Ok(plan) => Ok((plan.merge, plan.lint)),
                    Err(ProposeError::Refused(message)) => Err(message),
                    Err(ProposeError::Failed(e)) => panic!("{edits:?}: {e}"),
                };
                assert_eq!(answer(&full), answer(&stripped), "{edits:?}");
            }
        }
        // A whole-file change keeps the hidden section after the one it followed.
        let plan = plan_of(&full, &PAUL, &[write("doc.md", &rewritten)]);
        let text = content(&plan, "doc.md");
        assert!(
            text.contains("- at the end\n\n## Secret {#secret access=finance}\n\nHidden words"),
            "{text}"
        );
        assert!(text.contains("## After {#after}"), "{text}");
        assert!(!plan.owner_review, "{:?}", plan.notes);
        // The full corpus records what alice was not told.
        let plan = plan_of(&full, &ALICE, &[write("notes/plan.md", "# Mine\n")]);
        assert!(
            plan.owner_review && plan.notes.iter().any(|n| n.contains("cannot see")),
            "{:?}",
            plan.notes
        );
        let plan = plan_of(&full, &ALICE, &[write("adr/same.md", same_id)]);
        assert!(
            plan.notes.iter().any(|n| n.contains("PLAN-7")),
            "{:?}",
            plan.notes
        );
        let plan = plan_of(&full, &ALICE, &[write("adr/new.md", new_doc)]);
        assert!(
            plan.lint
                .iter()
                .any(|f| f.rule == "broken-link" && f.message.contains("notes/plan"))
        );
        assert!(plan.lint.iter().any(|f| f.rule == "broken-reference"));
        let plan = plan_of(&full, &ALICE, &[write("terms.md", glossary)]);
        assert!(
            plan.lint.iter().all(|f| f.rule != "duplicate-term"),
            "{:?}",
            plan.lint
        );
        let plan = plan_of(&full, &FIONA, &[write("terms.md", glossary)]);
        assert!(
            plan.lint.iter().any(|f| f.rule == "duplicate-term"),
            "{:?}",
            plan.lint
        );
    }

    /// A corpus with a hidden document `plan.md` (owned by `@finance-lead`), a hidden
    /// directory `vault.md/` and a visible `open.md`, and its twin without the hidden
    /// files.
    fn layout_twins() -> (Repo, Repo) {
        let long = long();
        let visible = vec![
            (
                "KIWIOWNERS".to_owned(),
                "* label=internal\nvault.md/ access=finance owner=@vault-team\n".to_owned(),
            ),
            (
                "open.md".to_owned(),
                format!("---\nid: OPEN-1\n---\n# Open\n\n{long}\n"),
            ),
            // A file Kiwi does not read, in both.
            ("finance/salaries.xlsx".to_owned(), "not markdown".to_owned()),
        ];
        let mut full = visible.clone();
        full.push((
            "plan.md".into(),
            format!("---\nid: PLAN-1\naccess: finance\nowner: \"@finance-lead\"\n---\n# Plan\n\n{long}\n"),
        ));
        full.push(("vault.md/keys.md".into(), format!("# Keys\n\n{long}\n")));
        (repo(&full), repo(&visible))
    }

    #[test]
    fn paths_never_replace_another_file() {
        let (full, twin) = layout_twins();
        let answer = |repo: &Repo, edits: &[Edit]| match plan_try(repo, &PAUL, edits) {
            Ok(plan) => Ok((plan.merge, plan.lint)),
            Err(ProposeError::Refused(message)) => Err(message),
            Err(ProposeError::Failed(e)) => panic!("{edits:?}: {e}"),
        };
        for edits in [
            vec![write("plan.md/new.md", "# New\n")],
            vec![write("vault.md", "# Vault\n")],
            vec![write("open.md/new.md", "# New\n")],
            vec![write("Open.md/new.md", "# New\n")],
            vec![write("KIWIOWNERS/notes.md", "# Notes\n")],
            vec![write("README.md", "# Readme\n")],
            vec![write("readme.md", "# Readme\n")],
        ] {
            assert_eq!(answer(&full, &edits), answer(&twin, &edits), "{edits:?}");
        }
        // Visible or reserved paths are refused, the same way for every corpus.
        for (path, why) in [
            ("open.md/new.md", "is a file"),
            ("KIWIOWNERS/notes.md", "KIWIOWNERS"),
            ("README.md", "README"),
        ] {
            let refusal = refusal(plan_try(&twin, &PAUL, &[write(path, "# X\n")]));
            assert!(refusal.contains(why), "{path}: {refusal}");
        }
        // Over a hidden file or directory, the change is left out of the commit and an
        // owner reviews the proposal.
        for (path, owner) in [("plan.md/new.md", "@finance-lead"), ("vault.md", "@vault-team")] {
            let plan = plan_of(&full, &PAUL, &[write(path, "# X\n")]);
            assert!(plan.changes.is_empty(), "{path}: {:?}", plan.changes);
            assert!(plan.owner_review, "{path}");
            assert_eq!(plan.review_owners, vec![owner.to_owned()], "{path}");
            assert!(plan.notes.iter().any(|n| n.contains("replace another file")));
        }
    }

    #[test]
    fn paths_never_run_into_what_kiwi_does_not_read() {
        let (full, twin) = layout_twins();
        // A symbolic link and a non-Markdown file, tracked by Git.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("open.md", full.root.join("link.md")).unwrap();
            git(&full.root, &["add", "link.md"]);
            git(&full.root, &["commit", "-q", "-m", "link"]);
        }
        let answer = |repo: &Repo, edits: &[Edit]| match plan_try(repo, &PAUL, edits) {
            Ok(plan) => Ok((plan.merge, plan.lint)),
            Err(ProposeError::Refused(message)) => Err(message),
            Err(ProposeError::Failed(e)) => panic!("{edits:?}: {e}"),
        };
        let mut cases = vec!["finance/salaries.xlsx/x.md", "Finance/Salaries.xlsx/x.md"];
        if cfg!(unix) {
            cases.push("link.md");
        }
        for path in cases {
            let edits = [write(path, "# X\n")];
            assert_eq!(answer(&full, &edits), answer(&twin, &edits), "{path}");
            let plan = plan_of(&full, &PAUL, &edits);
            assert!(
                plan.changes.is_empty() && plan.owner_review,
                "{path}: {:?}",
                plan.notes
            );
        }
        // A proposal's own new files must not run into each other.
        for edits in [
            vec![write("a.md", "# A\n"), write("a.md/b.md", "# B\n")],
            vec![write("A.md/b.md", "# B\n"), write("a.md", "# A\n")],
            vec![write("Caf\u{e9}.md", "# C\n"), write("cafe\u{301}.md", "# C\n")],
        ] {
            let refused = refusal(plan_try(&twin, &PAUL, &edits));
            assert!(
                refused.contains("file") || refused.contains("case") || refused.contains("directory"),
                "{refused}"
            );
        }
    }

    #[test]
    fn a_commit_changes_only_what_the_plan_names() {
        let (full, _) = layout_twins();
        let mut plan = plan_of(&full, &PAUL, &[write("new.md", "# New\n")]);
        // A plan that would replace the file `open.md` with a directory.
        plan.changes = vec![Change {
            path: "open.md/x.md".into(),
            content: Some("# X\n".into()),
        }];
        let author = Author {
            principal: "paul".into(),
            agent: "test".into(),
            name: "Paul".into(),
            email: "paul@test".into(),
        };
        let error = open(&target(&full), &plan, "t", "r", &author).unwrap_err();
        assert!(error.to_string().contains("does not name"), "{error}");
        let branches = Command::new("git")
            .args(["branch", "--list", "kiwi/*"])
            .current_dir(&full.root)
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&branches.stdout).trim().is_empty());
    }

    /// A stand-in `gh` that logs its arguments and plays a GitHub that `mode` describes:
    /// `ok`, `created-then-failed` (a draft create makes the pull request, then fails),
    /// `no-drafts` (a draft create is rejected) or `plain`.
    #[cfg(unix)]
    fn stand_in_gh(dir: &Path, mode: &str) -> PathBuf {
        use std::io::Write;
        let script = dir.join("gh");
        let log = dir.join("log");
        let state = dir.join("pr");
        let body = format!(
            r#"#!/bin/sh
echo "$*" >> '{log}'
case "$1 $2" in
  "pr create")
    case "$*" in
      *--draft*)
        case '{mode}' in
          created-then-failed) echo https://github.test/pr/1 > '{state}'; echo "could not add label" >&2; exit 1 ;;
          no-drafts) echo "draft pull requests are not supported" >&2; exit 1 ;;
        esac ;;
    esac
    echo https://github.test/pr/1 > '{state}'; echo https://github.test/pr/1 ;;
  "pr view")
    if [ -f '{state}' ]; then cat '{state}'; else echo "no pull requests found" >&2; exit 1; fi ;;
esac
exit 0
"#,
            log = log.display(),
            state = state.display(),
        );
        // Written by a child shell, never through a descriptor of this process: a test
        // thread forking at that moment would carry the open file into its child, and
        // running the script would then fail with "text file busy".
        let mut writer = Command::new("sh")
            .args(["-c", "cat > \"$0\" && chmod 755 \"$0\""])
            .arg(&script)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        writer.stdin.take().unwrap().write_all(body.as_bytes()).unwrap();
        assert!(writer.wait().unwrap().success());
        script
    }

    #[cfg(unix)]
    #[test]
    fn owner_review_pull_requests_are_drafts_and_never_duplicated() {
        let request = PullRequest {
            base: "main",
            branch: "kiwi/propose/paul/1",
            title: "t",
            body: "b",
            owner_review: true,
            reviewers: vec!["alice", "acme/payments"],
        };
        let run = |mode: &str, request: &PullRequest<'_>| -> (Vec<String>, Vec<String>) {
            let dir = tempfile::tempdir().unwrap();
            let gh = stand_in_gh(dir.path(), mode);
            let (url, failed) = create_pull_request(&gh, dir.path(), request).unwrap();
            assert_eq!(url, "https://github.test/pr/1");
            let log = std::fs::read_to_string(dir.path().join("log")).unwrap();
            (log.lines().map(str::to_owned).collect(), failed)
        };
        let creates = |log: &[String]| log.iter().filter(|l| l.starts_with("pr create")).count();
        // Created as a draft with its label; reviews one by one, never in the create.
        let (log, failed) = run("ok", &request);
        assert!(log[0].starts_with("label create kiwi:owner-review"), "{log:?}");
        assert!(log[1].starts_with("pr create") && log[1].contains("--draft --label kiwi:owner-review"));
        assert!(log.iter().all(|l| !l.contains("--reviewer ")), "{log:?}");
        assert_eq!(creates(&log), 1);
        assert_eq!(
            log[2..],
            [
                "pr edit https://github.test/pr/1 --add-reviewer alice",
                "pr edit https://github.test/pr/1 --add-reviewer acme/payments"
            ]
        );
        assert!(failed.is_empty(), "{failed:?}");
        // A create that failed after making the pull request: found, not made again.
        let (log, failed) = run("created-then-failed", &request);
        assert_eq!(creates(&log), 1, "{log:?}");
        assert!(log.iter().any(|l| l.starts_with("pr view kiwi/propose/paul/1")));
        assert!(log.iter().any(|l| l.ends_with("--undo")));
        assert!(log.iter().any(|l| l.ends_with("--add-label kiwi:owner-review")));
        assert_eq!(failed.len(), 1, "{failed:?}");
        // No drafts on this plan: a plain pull request, then labelled.
        let (log, _) = run("no-drafts", &request);
        assert_eq!(creates(&log), 2, "{log:?}");
        assert!(!log[log.iter().rposition(|l| l.starts_with("pr create")).unwrap()].contains("--draft"));
        assert!(log.iter().any(|l| l.ends_with("--add-label kiwi:owner-review")));
        // Without an owner's review: one plain create and nothing else.
        let plain = PullRequest {
            owner_review: false,
            ..request
        };
        let (log, failed) = run("ok", &plain);
        assert_eq!(log.len(), 1, "{log:?}");
        assert!(!log[0].contains("--draft") && !log[0].contains("--label"));
        assert!(failed.is_empty());
    }

    #[test]
    fn owners_map_to_github_reviewers() {
        assert_eq!(github_reviewer("@alice"), Some("alice"));
        assert_eq!(github_reviewer("@acme/payments"), Some("acme/payments"));
        assert_eq!(github_reviewer("cot-buom@cooly.me"), None);
        assert_eq!(github_reviewer("@"), None);
        assert_eq!(github_reviewer("@a/b/c"), None);
    }

    #[test]
    fn a_cut_never_changes_how_the_visible_text_parses() {
        let long = long();
        // A hidden ATX section ends a paragraph without a blank line, and a setext
        // section follows it.
        let doc = format!(
            "# Doc\n\n{long}\n\n## A {{#a}}\n\nText of A.\n## Secret {{#secret access=finance}}\n\nHidden.\n\nTitle B {{#title-b}}\n-------\n\nText of B.\n"
        );
        let twin_doc = doc.replace("## Secret {#secret access=finance}\n\nHidden.\n\n", "\n");
        let full = repo(&[
            ("KIWIOWNERS".into(), "* label=internal\n".into()),
            ("doc.md".into(), doc.clone()),
        ]);
        let twin = repo(&[
            ("KIWIOWNERS".into(), "* label=internal\n".into()),
            ("doc.md".into(), twin_doc),
        ]);
        let (_, outline) = compile_with_outline(
            "doc.md",
            &doc,
            &kiwi_policy::PathDefaults::access(AccessDecl::new("internal", Vec::new())),
            &crate::KiwiConfig::default(),
        )
        .unwrap();
        let spans: Vec<SectionSpan> = outline
            .iter()
            .map(|s| SectionSpan {
                start: s.start as u32,
                end: s.end as u32,
                chain: s.chain.clone(),
            })
            .collect();
        let stripped = strip_hidden(&doc, &spans, |chain| PAUL.read(chain)).unwrap();
        let (_, viewed) = compile_with_outline(
            "doc.md",
            &stripped,
            &kiwi_policy::PathDefaults::access(AccessDecl::new("internal", Vec::new())),
            &crate::KiwiConfig::default(),
        )
        .unwrap();
        let readable: Vec<bool> = outline.iter().map(|s| PAUL.read(&s.chain).unwrap()).collect();
        assert!(view_holds(&outline, &readable, &viewed), "{stripped}");
        let edit = [section("doc#title-b", "New text of B.")];
        let answer = |repo: &Repo| {
            plan_try(repo, &PAUL, &edit)
                .map(|p| (p.merge, p.lint))
                .map_err(|e| e.to_string())
        };
        assert_eq!(answer(&full), answer(&twin));
        assert!(answer(&full).is_ok());
    }

    #[test]
    fn paths_that_differ_only_in_case_are_refused() {
        let repo = repo(&files());
        let clash = refusal(plan_try(&repo, &PAUL, &[write("Plain.md", "# P\n")]));
        assert!(clash.contains("case"), "{clash}");
        // A directory that differs only in case from one holding a document the caller
        // sees is refused; from one holding only hidden documents, it is noted.
        let dir = refusal(plan_try(&repo, &FIONA, &[write("NOTES/other.md", "# P\n")]));
        assert!(dir.contains("case"), "{dir}");
        let plan = plan_of(&repo, &PAUL, &[write("NOTES/other.md", "# P\n")]);
        assert!(plan.notes.iter().any(|n| n.contains("case")), "{:?}", plan.notes);
        let own = refusal(plan_try(
            &repo,
            &PAUL,
            &[write("a.md", "# A\n"), write("A.md", "# A\n")],
        ));
        assert!(own.contains("case"), "{own}");
        // The path rule applies whatever the case: `Finance/` is finance's.
        let finance = refusal(plan_try(&repo, &PAUL, &[write("Finance/new.md", "# N\n")]));
        assert!(finance.contains("may not propose"), "{finance}");
    }

    #[test]
    fn proposals_read_the_base_revision_not_the_work_tree() {
        let repo = repo(&files());
        // An uncommitted edit moves every line of the file and renames a section.
        let worktree = std::fs::read_to_string(repo.root.join("doc.md"))
            .unwrap()
            .replace("# Doc\n", "# Doc\n\nInserted.\n\nMore.\n")
            .replace("## Open {#open}", "## Opened {#opened}");
        std::fs::write(repo.root.join("doc.md"), worktree).unwrap();
        let plan = plan_of(&repo, &ALICE, &[section("doc#open", "New open.")]);
        let text = content(&plan, "doc.md");
        assert!(!text.contains("Inserted."), "{text}");
        assert!(
            text.contains("## Open {#open}\n\nNew open.\n\n  ## Secret"),
            "{text}"
        );
    }

    #[test]
    fn a_proposal_compiles_only_what_it_changes() {
        let repo = repo(&files());
        let first = plan_of(&repo, &ALICE, &[section("doc#open", "New.")]);
        assert!(
            first.compiled >= files().len() - 1,
            "a cold cache compiles the corpus once"
        );
        let second = plan_of(&repo, &ALICE, &[section("plain#part", "New.")]);
        assert!(second.compiled <= 2, "compiled {}", second.compiled);
    }

    #[test]
    fn frontmatter_keys_are_set_in_place() {
        let text = "---\nid: A # keep\nstatus: approved\ntags:\n  - a\n---\n# Doc\n";
        let out = set_frontmatter(text, "status", "deprecated").unwrap_or_default();
        assert_eq!(
            out,
            "---\nid: A # keep\nstatus: deprecated\ntags:\n  - a\n---\n# Doc\n"
        );
        let out = set_frontmatter(text, "tags", "[\"b\"]").unwrap_or_default();
        assert!(out.contains("tags: [\"b\"]\n---"), "{out}");
        let out = set_frontmatter(text, "review_by", "\"2027-01-01\"").unwrap_or_default();
        assert!(out.contains("review_by: \"2027-01-01\"\n---"));
        assert!(
            set_frontmatter("# Doc\n", "status", "draft")
                .unwrap_or_default()
                .starts_with("---\nstatus: draft\n---")
        );
        assert!(set_frontmatter(text, "Bad-Key", "x").is_err());
    }

    #[test]
    fn frontmatter_keys_keep_line_endings_and_fail_closed() {
        let crlf = "---\r\nid: A\r\nstatus: draft\r\n---\r\n# Doc\r\n\r\nBody.\r\n";
        let out = set_frontmatter(crlf, "status", "approved").unwrap();
        assert_eq!(
            out,
            "---\r\nid: A\r\nstatus: approved\r\n---\r\n# Doc\r\n\r\nBody.\r\n"
        );
        let out = set_frontmatter(crlf, "owner", "\"@x\"").unwrap();
        assert!(
            out.starts_with("---\r\nid: A\r\nstatus: draft\r\nowner: \"@x\"\r\n---\r\n"),
            "{out:?}"
        );
        let bom = "\u{feff}--- \nid: A\n---\n# Doc\n";
        let out = set_frontmatter(bom, "status", "draft").unwrap();
        assert_eq!(out, "\u{feff}--- \nid: A\nstatus: draft\n---\n# Doc\n");
        // A value the line edit cannot place exactly is refused, never half-written.
        let flow = "---\naccess: {label: internal,\n  groups: [eng]}\nid: A\n---\n# Doc\n";
        assert!(set_frontmatter(flow, "access", "finance").is_ok());
        let nested = "---\nmeta:\n  access: public\n---\n# Doc\n";
        assert!(
            set_frontmatter(nested, "access", "finance").is_ok_and(|o| o.contains("\naccess: finance\n"))
        );
        assert!(set_frontmatter("---\nid: A\n# Doc\n", "status", "draft").is_err());
    }

    #[test]
    fn paths_are_checked() {
        assert!(check_path("adr/adr-009.md").is_ok());
        assert!(check_path("../x.md").is_err());
        assert!(check_path(".kiwi/policy.md").is_err());
        assert!(check_path("/abs.md").is_err());
        assert!(check_path("notes.txt").is_err());
    }
}
