//! Where the corpus comes from: a Git commit (the source of truth) or a directory.
//!
//! A Git source reads blobs straight from the commit's tree, without a checkout, and
//! pins the snapshot to the commit hash. A directory source reads the working files and
//! pins the snapshot to a hash of their content.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{CompileError, Result};
use kiwi_core::date;

/// A corpus snapshot: every file under the corpus root, by relative path.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// `git` or `dir`.
    pub kind: &'static str,
    /// The commit hash, or `dir:<hash>`.
    pub id: String,
    /// The snapshot date (`YYYY-MM-DD`): the commit date, or today for a directory.
    pub as_of: String,
    /// Files by path relative to the corpus root (forward slashes).
    pub files: BTreeMap<String, Vec<u8>>,
    /// The repository work tree and the corpus path inside it, for Git sources.
    pub repo: Option<RepoLocation>,
}

/// Where a Git-backed corpus lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoLocation {
    /// The repository's `.git` directory or work tree.
    pub repo_path: PathBuf,
    /// The corpus directory relative to the repository root (empty for the root).
    pub prefix: String,
    /// The commit the snapshot was read from.
    pub commit: String,
}

impl Snapshot {
    /// The Markdown documents (`*.md`, outside dot-directories).
    pub fn markdown(&self) -> impl Iterator<Item = (&String, &Vec<u8>)> {
        self.files.iter().filter(|(path, _)| {
            path.ends_with(".md")
                && !path.split('/').any(|part| part.starts_with('.'))
                && !path.eq_ignore_ascii_case("README.md")
        })
    }

    /// A file by relative path.
    pub fn file(&self, path: &str) -> Option<&[u8]> {
        self.files.get(path).map(Vec::as_slice)
    }
}

/// How to read the corpus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceMode {
    /// Git when the corpus is inside a repository, else the directory.
    Auto,
    /// The directory, even inside a repository (uncommitted changes included).
    Worktree,
    /// A Git revision (`HEAD`, a branch, a commit).
    Git(String),
}

/// Reads a snapshot of the corpus at `root`.
pub fn read(root: &Path, mode: &SourceMode) -> Result<Snapshot> {
    match mode {
        SourceMode::Worktree => read_dir(root),
        SourceMode::Git(rev) => read_git(root, rev),
        SourceMode::Auto => match gix::discover(root) {
            Ok(_) => read_git(root, "HEAD"),
            Err(_) => read_dir(root),
        },
    }
}

/// Whether commit `ancestor` is `descendant` or one of its ancestors, in the repository
/// of the corpus at `root` (both full commit hashes).
pub fn is_ancestor(root: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let parse = |hex: &str| gix::ObjectId::from_hex(hex.as_bytes()).map_err(CompileError::git);
    let (ancestor, descendant) = (parse(ancestor)?, parse(descendant)?);
    if ancestor == descendant {
        return Ok(true);
    }
    let repo = gix::discover(root).map_err(CompileError::git)?;
    let walk = repo.rev_walk([descendant]).all().map_err(CompileError::git)?;
    for info in walk {
        if info.map_err(CompileError::git)?.id == ancestor {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The Git directories of the repository holding the corpus at `root`: its `.git`
/// directory and, for a linked work tree, the common directory that holds the branches.
pub fn git_dirs(root: &Path) -> Result<Vec<PathBuf>> {
    let repo = gix::discover(root).map_err(CompileError::git)?;
    let mut dirs = vec![repo.git_dir().to_owned()];
    let common = repo.common_dir().to_owned();
    if !dirs.contains(&common) {
        dirs.push(common);
    }
    Ok(dirs)
}

/// Whether `mode` reads the corpus at `root` from Git (else from the directory).
pub fn uses_git(root: &Path, mode: &SourceMode) -> bool {
    match mode {
        SourceMode::Worktree => false,
        SourceMode::Git(_) => true,
        SourceMode::Auto => gix::discover(root).is_ok(),
    }
}

/// Reads only the named files (paths relative to the corpus root) of a snapshot, for
/// callers that need the configuration or the policy without the whole corpus. The
/// snapshot id is not computed for directory sources.
pub fn read_paths(root: &Path, mode: &SourceMode, paths: &[&str]) -> Result<Snapshot> {
    if !uses_git(root, mode) {
        let mut files = BTreeMap::new();
        for path in paths {
            let full = root.join(path);
            match std::fs::read(&full) {
                Ok(bytes) => {
                    files.insert((*path).to_owned(), bytes);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(CompileError::source(full, e)),
            }
        }
        return Ok(Snapshot {
            kind: "dir",
            id: String::new(),
            as_of: date::today(),
            files,
            repo: None,
        });
    }
    let rev = match mode {
        SourceMode::Git(rev) => rev.as_str(),
        _ => "HEAD",
    };
    let root = root.canonicalize().map_err(|e| CompileError::source(root, e))?;
    let repo = gix::discover(&root).map_err(CompileError::git)?;
    let workdir = repo
        .workdir()
        .ok_or_else(|| CompileError::git("bare repositories need a work tree path"))?
        .canonicalize()
        .map_err(|e| CompileError::source(root.as_path(), e))?;
    let prefix = relative(&workdir, &root)?;
    let commit = repo
        .rev_parse_single(rev)
        .map_err(|e| CompileError::git(format!("revision `{rev}`: {e}")))?
        .object()
        .map_err(CompileError::git)?
        .peel_to_commit()
        .map_err(CompileError::git)?;
    let tree = commit.tree().map_err(CompileError::git)?;
    let mut files = BTreeMap::new();
    for path in paths {
        let full = if prefix.is_empty() {
            (*path).to_owned()
        } else {
            format!("{prefix}/{path}")
        };
        if let Some(entry) = tree.lookup_entry_by_path(&full).map_err(CompileError::git)? {
            let object = entry.object().map_err(CompileError::git)?;
            files.insert((*path).to_owned(), object.data.clone());
        }
    }
    Ok(Snapshot {
        kind: "git",
        id: commit.id().to_string(),
        as_of: date::from_unix(commit.time().map_err(CompileError::git)?.seconds),
        files,
        repo: None,
    })
}

/// Reads the working files under `root`.
pub fn read_dir(root: &Path) -> Result<Snapshot> {
    let mut files = BTreeMap::new();
    walk(root, root, &mut files)?;
    let mut hasher = blake3::Hasher::new();
    for (path, bytes) in &files {
        hasher.update(path.as_bytes());
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    let hash = hasher.finalize().to_hex();
    Ok(Snapshot {
        kind: "dir",
        id: format!("dir:{}", &hash[..16]),
        as_of: date::today(),
        files,
        repo: None,
    })
}

const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

fn wanted(rel: &str) -> bool {
    rel.ends_with(".md")
        || rel == "KIWIOWNERS"
        || rel.starts_with(".kiwi/") && !rel.starts_with(".kiwi/packs/") && !rel.starts_with(".kiwi/state/")
}

fn walk(root: &Path, dir: &Path, files: &mut BTreeMap<String, Vec<u8>>) -> Result<()> {
    let entries = std::fs::read_dir(dir).map_err(|e| CompileError::source(dir, e))?;
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries {
        paths.push(entry.map_err(|e| CompileError::source(dir, e))?.path());
    }
    paths.sort();
    for path in paths {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let meta = std::fs::symlink_metadata(&path).map_err(|e| CompileError::source(&path, e))?;
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            if name == ".git" || name == "target" || name == "node_modules" {
                continue;
            }
            walk(root, &path, files)?;
            continue;
        }
        let rel = relative(root, &path)?;
        if !wanted(&rel) {
            continue;
        }
        if meta.len() > MAX_FILE_BYTES {
            return Err(CompileError::source(&path, "file larger than 8 MiB"));
        }
        let bytes = std::fs::read(&path).map_err(|e| CompileError::source(&path, e))?;
        files.insert(rel, bytes);
    }
    Ok(())
}

fn relative(root: &Path, path: &Path) -> Result<String> {
    let rel = path
        .strip_prefix(root)
        .map_err(|e| CompileError::source(path, e))?;
    let parts: Vec<&str> = rel
        .components()
        .map(|c| c.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| CompileError::source(path, "path is not valid UTF-8"))?;
    Ok(parts.join("/"))
}

/// Reads the corpus at `root` from Git revision `rev`, without a checkout.
pub fn read_git(root: &Path, rev: &str) -> Result<Snapshot> {
    let root = root.canonicalize().map_err(|e| CompileError::source(root, e))?;
    let repo = gix::discover(&root).map_err(CompileError::git)?;
    let workdir = repo
        .workdir()
        .ok_or_else(|| CompileError::git("bare repositories need a work tree path"))?
        .canonicalize()
        .map_err(|e| CompileError::source(root.as_path(), e))?;
    let prefix = relative(&workdir, &root)?;
    let id = repo
        .rev_parse_single(rev)
        .map_err(|e| CompileError::git(format!("revision `{rev}`: {e}")))?;
    let commit = id
        .object()
        .map_err(CompileError::git)?
        .peel_to_commit()
        .map_err(CompileError::git)?;
    let seconds = commit.time().map_err(CompileError::git)?.seconds;
    let commit_id = commit.id().to_string();
    let tree = commit.tree().map_err(CompileError::git)?;
    let subtree = if prefix.is_empty() {
        tree
    } else {
        let entry = tree
            .lookup_entry_by_path(&prefix)
            .map_err(CompileError::git)?
            .ok_or_else(|| CompileError::git(format!("`{prefix}` is not in commit {commit_id}")))?;
        entry
            .object()
            .map_err(CompileError::git)?
            .peel_to_tree()
            .map_err(CompileError::git)?
    };
    let records = subtree
        .traverse()
        .breadthfirst
        .files()
        .map_err(CompileError::git)?;
    let mut files = BTreeMap::new();
    for record in records {
        if !record.mode.is_blob() {
            continue;
        }
        let rel = record.filepath.to_string();
        if !wanted(&rel) {
            continue;
        }
        let object = repo.find_object(record.oid).map_err(CompileError::git)?;
        if object.data.len() as u64 > MAX_FILE_BYTES {
            return Err(CompileError::source(&rel, "file larger than 8 MiB"));
        }
        files.insert(rel, object.data.clone());
    }
    Ok(Snapshot {
        kind: "git",
        id: commit_id.clone(),
        as_of: date::from_unix(seconds),
        files,
        repo: Some(RepoLocation {
            repo_path: workdir,
            prefix,
            commit: commit_id,
        }),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(["-c", "user.email=t@test", "-c", "user.name=T"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn a_linked_work_tree_has_its_own_and_the_common_git_directory() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        std::fs::create_dir_all(&main).unwrap();
        std::fs::write(main.join("a.md"), "# A\n").unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        git(&main, &["add", "a.md"]);
        git(&main, &["commit", "-q", "-m", "a"]);
        let linked = dir.path().join("linked");
        git(
            &main,
            &["worktree", "add", "-q", linked.to_str().unwrap(), "-b", "other"],
        );
        let canonical = |p: &Path| p.canonicalize().unwrap();
        let dirs: Vec<PathBuf> = git_dirs(&linked).unwrap().iter().map(|d| canonical(d)).collect();
        assert_eq!(dirs.len(), 2, "{dirs:?}");
        assert_eq!(dirs[1], canonical(&main.join(".git")));
        assert_eq!(git_dirs(&main).unwrap().len(), 1);
    }
}
