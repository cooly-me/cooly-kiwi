//! Access declarations from the three sources, coarse to fine: `KIWIOWNERS` per
//! directory, `access:` in frontmatter per document, and heading attributes per section.
//! `KIWIOWNERS` also names the default owner of the documents it covers.
//!
//! A document's access is the last matching `KIWIOWNERS` rule that declares access,
//! replaced by its frontmatter `access:` when present. A section's heading attributes do not replace
//! the document's access; they narrow it: the section is visible only to readers who may
//! see the document *and* the section declaration. Narrowing keeps every breadcrumb safe,
//! since a reader of a section can always read the headings above it.

use globset::{GlobBuilder, GlobMatcher};
use kiwi_core::model::AccessDecl;
use serde::Deserialize;

/// The label used when no rule matches. No shipped policy permits it, so unmatched
/// content is denied by default.
pub const UNLABELED: &str = "unlabeled";

/// The label implied by `access=<groups>` on a heading.
pub const CONFIDENTIAL: &str = "confidential";

/// Errors in access declarations.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AccessError {
    /// A `KIWIOWNERS` line could not be parsed.
    #[error("KIWIOWNERS line {line}: {message}")]
    Owners {
        /// 1-based line number.
        line: usize,
        /// What is wrong.
        message: String,
    },
    /// A heading attribute or frontmatter `access` value is invalid.
    #[error("invalid access declaration: {0}")]
    Invalid(String),
}

struct Rule {
    matchers: Vec<GlobMatcher>,
    decl: Option<AccessDecl>,
    owner: Option<String>,
}

/// What `KIWIOWNERS` gives one document before its frontmatter is read: its access
/// ([`unlabeled`] when no rule declares one) and its default owner. Frontmatter
/// `access:` replaces the access and frontmatter `owner:` replaces the owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathDefaults {
    /// The document's access before frontmatter.
    pub access: AccessDecl,
    /// The document's owner when its frontmatter names none.
    pub owner: Option<String>,
}

impl PathDefaults {
    /// Defaults with `access` and no owner.
    pub fn access(access: AccessDecl) -> Self {
        Self { access, owner: None }
    }
}

/// A parsed `KIWIOWNERS` file: glob rules in CODEOWNERS style, the last match wins.
///
/// A line declares access (`label=`, `groups=`, `access=`), an owner (`owner=`), or
/// both. Each is resolved on its own: a document's access comes from the last matching
/// line that declares access, its default owner from the last matching line that names
/// an owner. An owner is never access: a line with only `owner=` leaves every access
/// decision exactly as it was. `owner=` takes one owner (a handle or an address), as
/// frontmatter `owner:` does; a comma list is an error.
///
/// As in CODEOWNERS, a pattern names files or directories: `hr` (anywhere) or `/hr`
/// (at the root) covers a file named `hr` and everything under a directory named `hr`,
/// and `hr/` (anywhere) or `/hr/` (at the root) only the directory. A slash at the start
/// or in the middle (`finance/**`, `docs/hr`) anchors a pattern to the root. Matching ignores case, so `Finance/new.md` falls under
/// `finance/**` exactly as `finance/new.md` does.
///
/// ```text
/// # pattern            declarations
/// *                    label=internal  owner=@team-docs
/// vision.md            label=public
/// finance/**           label=confidential groups=finance  owner=cfo@example.com
/// ```
#[derive(Default)]
pub struct KiwiOwners {
    rules: Vec<Rule>,
}

impl std::fmt::Debug for KiwiOwners {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KiwiOwners")
            .field("rules", &self.rules.len())
            .finish()
    }
}

impl KiwiOwners {
    /// Parses the file content.
    pub fn parse(source: &str) -> Result<Self, AccessError> {
        let mut rules = Vec::new();
        for (index, raw) in source.lines().enumerate() {
            let line = index + 1;
            let content = raw.split('#').next().unwrap_or("").trim();
            if content.is_empty() {
                continue;
            }
            let mut parts = content.split_whitespace();
            let Some(pattern) = parts.next() else {
                continue;
            };
            let error = |message: String| AccessError::Owners { line, message };
            let mut owner: Option<String> = None;
            let mut attrs: Vec<(&str, Option<&str>)> = Vec::new();
            for part in parts {
                let (key, value) = match part.split_once('=') {
                    Some((key, value)) => (key, Some(value)),
                    None => (part, None),
                };
                match key {
                    "owner" => {
                        let value = parse_owner(value).map_err(error)?;
                        if owner.replace(value).is_some() {
                            return Err(error("`owner` is given twice".into()));
                        }
                    }
                    "label" | "groups" | "access" => attrs.push((key, value)),
                    other => {
                        return Err(error(format!(
                            "unknown attribute `{other}`; a KIWIOWNERS line declares `label=`, `groups=`, `access=` or `owner=`"
                        )));
                    }
                }
            }
            let decl = decl_from_attrs(&attrs).map_err(|e| error(e.to_string()))?;
            if decl.is_none() && owner.is_none() {
                return Err(error("a rule needs label=, access= or owner=".into()));
            }
            let matchers = compile_pattern(pattern).map_err(error)?;
            rules.push(Rule {
                matchers,
                decl,
                owner,
            });
        }
        Ok(Self { rules })
    }

    /// The access of a file (path relative to the corpus root, with extension): the
    /// last matching rule that declares access.
    pub fn resolve(&self, path: &str) -> Option<&AccessDecl> {
        self.matching(path).find_map(|rule| rule.decl.as_ref())
    }

    /// The default owner of a file: the last matching rule that names an owner.
    pub fn owner(&self, path: &str) -> Option<&str> {
        self.matching(path).find_map(|rule| rule.owner.as_deref())
    }

    /// The access and default owner of a file, [`unlabeled`] when no rule declares
    /// access.
    pub fn defaults(&self, path: &str) -> PathDefaults {
        PathDefaults {
            access: self.resolve(path).cloned().unwrap_or_else(unlabeled),
            owner: self.owner(path).map(str::to_owned),
        }
    }

    /// The rules matching `path`, last first.
    fn matching<'s, 'p>(&'s self, path: &'p str) -> impl Iterator<Item = &'s Rule> + use<'s, 'p> {
        self.rules
            .iter()
            .rev()
            .filter(move |rule| rule.matchers.iter().any(|m| m.is_match(path)))
    }

    /// Number of rules.
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// True when the file has no rules.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// The matchers of one pattern: the pattern itself and, unless it already names
/// everything under a directory, the directory it may name (`pattern/**`).
fn compile_pattern(pattern: &str) -> Result<Vec<GlobMatcher>, String> {
    let mut glob = pattern.trim_start_matches('/').to_owned();
    if glob.is_empty() {
        return Err(format!("bad pattern `{pattern}`"));
    }
    // As in CODEOWNERS, a slash at the start or in the middle anchors a pattern to the
    // root; a trailing slash only says it names a directory.
    let anchored = pattern.trim_end_matches('/').contains('/');
    if glob.ends_with('/') {
        glob.push_str("**");
    }
    if !anchored && glob != "*" && glob != "**" {
        glob = format!("**/{glob}");
    }
    if glob == "*" {
        glob = "**".into();
    }
    let mut globs = vec![glob.clone()];
    if !glob.ends_with("**") {
        globs.push(format!("{glob}/**"));
    }
    globs
        .iter()
        .map(|glob| {
            GlobBuilder::new(glob)
                .literal_separator(true)
                .case_insensitive(true)
                .build()
                .map(|g| g.compile_matcher())
                .map_err(|e| format!("bad pattern `{pattern}`: {e}"))
        })
        .collect()
}

/// One owner: a non-empty value without a comma, since a document has one owner.
fn parse_owner(value: Option<&str>) -> Result<String, String> {
    let value = value
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "`owner` needs a value".to_owned())?;
    if value.contains(',') {
        return Err(format!(
            "`owner={value}` names more than one owner; a document has one owner, so give one"
        ));
    }
    Ok(value.to_owned())
}

/// The groups of a comma list; a list that names no group (`groups=,`) is an error, so
/// a declaration never loses its groups to a typo.
fn split_groups(key: &str, value: &str) -> Result<Vec<String>, AccessError> {
    let groups: Vec<String> = value
        .split(',')
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .map(str::to_owned)
        .collect();
    if groups.is_empty() {
        return Err(AccessError::Invalid(format!("`{key}` names no group")));
    }
    Ok(groups)
}

/// Builds a declaration from `label=`, `groups=` and the shorthand `access=`.
///
/// `access=finance,legal` means `label=confidential groups=finance,legal`; an explicit
/// `label=` wins over the shorthand's label. Returns `None` when none of the three keys
/// is present. Any other key, or a key given twice, is an error: a misspelt
/// `acess=finance` must not leave a section readable under its parent's access.
pub fn decl_from_attrs(attrs: &[(&str, Option<&str>)]) -> Result<Option<AccessDecl>, AccessError> {
    let mut label: Option<String> = None;
    let mut groups: Option<Vec<String>> = None;
    let mut shorthand: Option<Vec<String>> = None;
    for &(key, value) in attrs {
        let value = || {
            value
                .filter(|v| !v.is_empty())
                .ok_or_else(|| AccessError::Invalid(format!("`{key}` needs a value")))
        };
        let slot_taken = match key {
            "label" => label.replace(value()?.to_owned()).is_some(),
            "groups" => groups.replace(split_groups(key, value()?)?).is_some(),
            "access" => shorthand.replace(split_groups(key, value()?)?).is_some(),
            other => {
                return Err(AccessError::Invalid(format!(
                    "unknown attribute `{other}`; access is declared with `label=`, `groups=` or `access=`"
                )));
            }
        };
        if slot_taken {
            return Err(AccessError::Invalid(format!("`{key}` is given twice")));
        }
    }
    if label.is_none() && groups.is_none() && shorthand.is_none() {
        return Ok(None);
    }
    let label = match (&label, &shorthand) {
        (Some(label), _) => label.clone(),
        (None, Some(_)) => CONFIDENTIAL.to_owned(),
        (None, None) => {
            return Err(AccessError::Invalid("`groups` needs a `label`".into()));
        }
    };
    if !label
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(AccessError::Invalid(format!("bad label `{label}`")));
    }
    let mut all = groups.unwrap_or_default();
    all.extend(shorthand.unwrap_or_default());
    Ok(Some(AccessDecl::new(label, all)))
}

/// The frontmatter form: `access: { label: internal, groups: [eng, product] }`, or the
/// shorthand `access: finance` (confidential to `finance`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum FrontmatterAccess {
    /// A group shorthand.
    Group(String),
    /// A full declaration.
    Full(FullAccess),
}

/// `access: { label: …, groups: […] }`. Any other key is an error.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FullAccess {
    /// The label.
    pub label: String,
    /// The groups.
    #[serde(default)]
    pub groups: Vec<String>,
}

impl FrontmatterAccess {
    /// Converts to a declaration.
    pub fn to_decl(&self) -> Result<AccessDecl, AccessError> {
        let attrs: Vec<(String, String)> = match self {
            FrontmatterAccess::Group(group) => vec![("access".into(), group.clone())],
            FrontmatterAccess::Full(FullAccess { label, groups }) => {
                let mut attrs = vec![("label".to_owned(), label.clone())];
                if !groups.is_empty() {
                    attrs.push(("groups".into(), groups.join(",")));
                }
                attrs
            }
        };
        let borrowed: Vec<(&str, Option<&str>)> = attrs
            .iter()
            .map(|(k, v)| (k.as_str(), Some(v.as_str())))
            .collect();
        decl_from_attrs(&borrowed)?.ok_or_else(|| AccessError::Invalid("empty access declaration".into()))
    }
}

/// The access of a document that no rule covers.
pub fn unlabeled() -> AccessDecl {
    AccessDecl::new(UNLABELED, Vec::new())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn last_rule_wins() {
        let owners = KiwiOwners::parse(
            "# comment\n*  label=internal\nvision.md label=public\nfinance/** label=confidential groups=finance\n",
        )
        .unwrap();
        assert_eq!(owners.len(), 3);
        assert_eq!(owners.resolve("adr/adr-001.md").unwrap().label, "internal");
        assert_eq!(owners.resolve("vision.md").unwrap().label, "public");
        let finance = owners.resolve("finance/budget/2027.md").unwrap();
        assert_eq!(finance.label, "confidential");
        assert_eq!(finance.groups, vec!["finance"]);
    }

    #[test]
    fn directory_patterns_and_errors() {
        let owners = KiwiOwners::parse("legal/ access=legal\n").unwrap();
        assert_eq!(owners.resolve("legal/x.md").unwrap().groups, vec!["legal"]);
        assert!(owners.resolve("other.md").is_none());
        let err = KiwiOwners::parse("*.md\n").unwrap_err();
        assert!(matches!(err, AccessError::Owners { line: 1, .. }));
        let err = KiwiOwners::parse("* label=internal grops=eng\n").unwrap_err();
        assert!(matches!(err, AccessError::Owners { line: 1, .. }), "{err}");
    }

    #[test]
    fn bare_names_cover_directories_and_matching_ignores_case() {
        let owners =
            KiwiOwners::parse("* label=internal\nhr label=confidential groups=hr\n/ops access=ops\n")
                .unwrap();
        for path in [
            "hr/salaries.md",
            "hr/2027/plan.md",
            "teams/hr/notes.md",
            "HR/x.md",
            "hr",
        ] {
            assert_eq!(owners.resolve(path).unwrap().groups, vec!["hr"], "{path}");
        }
        assert_eq!(owners.resolve("hrm/x.md").unwrap().label, "internal");
        assert_eq!(owners.resolve("ops/run.md").unwrap().groups, vec!["ops"]);
        assert_eq!(owners.resolve("Ops/Run.md").unwrap().groups, vec!["ops"]);
        assert_eq!(owners.resolve("teams/ops/run.md").unwrap().label, "internal");
        let dirs =
            KiwiOwners::parse("* label=internal\nhr/ access=hr\n/legal/ access=legal\ndocs/ops access=ops\n")
                .unwrap();
        for path in ["hr/a.md", "teams/hr/a.md", "a/b/HR/c.md"] {
            assert_eq!(dirs.resolve(path).unwrap().groups, vec!["hr"], "{path}");
        }
        assert_eq!(
            dirs.resolve("hr").unwrap().label,
            "internal",
            "`hr/` names only a directory"
        );
        assert_eq!(dirs.resolve("legal/a.md").unwrap().groups, vec!["legal"]);
        assert_eq!(dirs.resolve("teams/legal/a.md").unwrap().label, "internal");
        assert_eq!(dirs.resolve("docs/ops/a.md").unwrap().groups, vec!["ops"]);
        assert_eq!(dirs.resolve("x/docs/ops/a.md").unwrap().label, "internal");
        let finance = KiwiOwners::parse("finance/** access=finance\n").unwrap();
        assert_eq!(finance.resolve("Finance/new.md").unwrap().groups, vec!["finance"]);
    }

    #[test]
    fn owners_resolve_on_their_own_and_the_last_match_wins() {
        let owners = KiwiOwners::parse(
            "*  label=internal  owner=@team-docs\n\
             product/  label=public  owner=cot-buom@cooly.me\n\
             product/pricing.md  access=finance\n\
             ops/  owner=@sre\n\
             ops/secret/  label=restricted\n",
        )
        .unwrap();
        assert_eq!(owners.len(), 5);
        assert_eq!(owners.owner("adr/a.md"), Some("@team-docs"));
        assert_eq!(owners.owner("product/prd.md"), Some("cot-buom@cooly.me"));
        // An access-only line keeps the owner of the line before it, and the reverse.
        assert_eq!(owners.owner("product/pricing.md"), Some("cot-buom@cooly.me"));
        assert_eq!(
            owners.resolve("product/pricing.md").unwrap().groups,
            vec!["finance"]
        );
        assert_eq!(owners.owner("ops/run.md"), Some("@sre"));
        assert_eq!(owners.resolve("ops/run.md").unwrap().label, "internal");
        assert_eq!(owners.owner("ops/secret/keys.md"), Some("@sre"));
        assert_eq!(owners.resolve("ops/secret/keys.md").unwrap().label, "restricted");
        assert_eq!(
            owners.defaults("product/prd.md"),
            PathDefaults {
                access: AccessDecl::new("public", Vec::new()),
                owner: Some("cot-buom@cooly.me".into()),
            }
        );
        let only_owner = KiwiOwners::parse("docs/ owner=@docs\n").unwrap();
        assert_eq!(only_owner.resolve("docs/a.md"), None, "an owner is not access");
        assert_eq!(
            only_owner.defaults("docs/a.md"),
            PathDefaults {
                access: unlabeled(),
                owner: Some("@docs".into()),
            }
        );
        assert_eq!(only_owner.owner("other.md"), None);
    }

    #[test]
    fn owner_errors_fail_closed() {
        for bad in [
            "* label=internal owner=\n",
            "* label=internal owner\n",
            "* label=internal owner=@a owner=@b\n",
            "* label=internal owner=@a,@b\n",
            "* label=internal ownr=@a\n",
            "* groups=eng owner=@a\n",
        ] {
            let err = KiwiOwners::parse(bad).unwrap_err();
            assert!(matches!(err, AccessError::Owners { line: 1, .. }), "{bad}: {err}");
        }
        assert!(
            decl_from_attrs(&[("label", Some("internal")), ("owner", Some("@a"))]).is_err(),
            "a heading declares access only"
        );
    }

    #[test]
    fn heading_attribute_forms() {
        let decl = decl_from_attrs(&[("access", Some("finance"))]).unwrap().unwrap();
        assert_eq!(decl, AccessDecl::new("confidential", ["finance".to_owned()]));
        let decl = decl_from_attrs(&[("label", Some("restricted")), ("groups", Some("exec,board"))])
            .unwrap()
            .unwrap();
        assert_eq!(decl.groups, vec!["board", "exec"]);
        assert_eq!(decl_from_attrs(&[]).unwrap(), None);
        assert!(
            decl_from_attrs(&[("class", Some("x"))]).is_err(),
            "unknown keys fail closed"
        );
        assert!(decl_from_attrs(&[("acess", Some("finance"))]).is_err());
        assert!(decl_from_attrs(&[("access", Some("a")), ("access", Some("b"))]).is_err());
        assert!(decl_from_attrs(&[("groups", Some("eng"))]).is_err());
        assert!(decl_from_attrs(&[("label", Some("bad label!"))]).is_err());
        for empty in [",", " , ", ", ,"] {
            assert!(decl_from_attrs(&[("access", Some(empty))]).is_err(), "{empty:?}");
            assert!(
                decl_from_attrs(&[("label", Some("internal")), ("groups", Some(empty))]).is_err(),
                "{empty:?}"
            );
        }
        let blank: FrontmatterAccess =
            serde_yaml_ng::from_str("{ label: internal, groups: [\" \", \"\"] }").unwrap();
        assert!(blank.to_decl().is_err(), "a list of blank groups");
        let spaces: FrontmatterAccess = serde_yaml_ng::from_str("\"  \"").unwrap();
        assert!(spaces.to_decl().is_err());
    }

    #[test]
    fn frontmatter_forms() {
        let full: FrontmatterAccess =
            serde_yaml_ng::from_str("{ label: internal, groups: [eng, product] }").unwrap();
        assert_eq!(
            full.to_decl().unwrap(),
            AccessDecl::new("internal", ["eng".to_owned(), "product".to_owned()])
        );
        let short: FrontmatterAccess = serde_yaml_ng::from_str("legal").unwrap();
        assert_eq!(short.to_decl().unwrap().label, "confidential");
        assert!(
            serde_yaml_ng::from_str::<FrontmatterAccess>("{ label: internal, group: [eng] }").is_err(),
            "a misspelt key fails closed"
        );
    }
}
