//! Markdown parsing: frontmatter, heading-bounded sections, heading attributes, links.
//!
//! Declarations fail closed: a document whose frontmatter or access attributes could be
//! read two ways is an error, never a document served under a broader rule.

use std::ops::Range;
use std::sync::OnceLock;

use kiwi_core::model::AccessDecl;
use kiwi_policy::FrontmatterAccess;
use kiwi_policy::access::decl_from_attrs;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use serde::{Deserialize, Deserializer};

use crate::error::{CompileError, Result};

fn one_or_many<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Option::<OneOrMany>::deserialize(deserializer)? {
        None => Vec::new(),
        Some(OneOrMany::One(value)) => vec![value],
        Some(OneOrMany::Many(values)) => values,
    })
}

fn scalar_string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    let value = Option::<serde_yaml_ng::Value>::deserialize(deserializer)?;
    Ok(match value {
        None | Some(serde_yaml_ng::Value::Null) => None,
        Some(serde_yaml_ng::Value::String(s)) => Some(s),
        Some(serde_yaml_ng::Value::Number(n)) => Some(n.to_string()),
        Some(serde_yaml_ng::Value::Bool(b)) => Some(b.to_string()),
        Some(_) => return Err(serde::de::Error::custom("expected a scalar")),
    })
}

/// A present `access:` key: `access:` with no value (`null`) is an error, never the
/// absence of a declaration, so the document cannot fall back to its path rule.
fn present_access<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<FrontmatterAccess>, D::Error> {
    match Option::<FrontmatterAccess>::deserialize(deserializer)? {
        Some(access) => Ok(Some(access)),
        None => Err(serde::de::Error::custom(
            "`access:` has no value; declare one or remove the key",
        )),
    }
}

/// The frontmatter keys Kiwi reads. Any other key is an error, so a misspelt `access`
/// can never leave a document under a broader path rule.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Frontmatter {
    /// Document id, e.g. `RFC-042`.
    #[serde(deserialize_with = "scalar_string")]
    pub id: Option<String>,
    /// Document type.
    #[serde(rename = "type", deserialize_with = "scalar_string")]
    pub doc_type: Option<String>,
    /// Lifecycle status.
    #[serde(deserialize_with = "scalar_string")]
    pub status: Option<String>,
    /// Owner; replaces the default owner `KIWIOWNERS` gives the document.
    #[serde(deserialize_with = "scalar_string")]
    pub owner: Option<String>,
    /// Review deadline `YYYY-MM-DD`.
    #[serde(deserialize_with = "scalar_string")]
    pub review_by: Option<String>,
    /// One-sentence summary (the root's L1).
    #[serde(deserialize_with = "scalar_string")]
    pub summary: Option<String>,
    /// Title override.
    #[serde(deserialize_with = "scalar_string")]
    pub title: Option<String>,
    /// Documents this one implements.
    #[serde(deserialize_with = "one_or_many")]
    pub implements: Vec<String>,
    /// Documents this one replaces.
    #[serde(deserialize_with = "one_or_many")]
    pub supersedes: Vec<String>,
    /// ADRs that decided this one.
    #[serde(deserialize_with = "one_or_many")]
    pub decided_by: Vec<String>,
    /// Documents this one depends on.
    #[serde(deserialize_with = "one_or_many")]
    pub depends_on: Vec<String>,
    /// Documents this one changes in part without replacing them.
    #[serde(deserialize_with = "one_or_many")]
    pub amends: Vec<String>,
    /// Later documents that amend this one (each must name this one in `amends`).
    #[serde(deserialize_with = "one_or_many")]
    pub amended_by: Vec<String>,
    /// Code globs this document governs.
    #[serde(deserialize_with = "one_or_many")]
    pub code: Vec<String>,
    /// Tags.
    #[serde(deserialize_with = "one_or_many")]
    pub tags: Vec<String>,
    /// Access override for the whole document.
    #[serde(deserialize_with = "present_access")]
    pub access: Option<FrontmatterAccess>,
}

/// One heading-bounded section as written.
#[derive(Debug, Clone, PartialEq)]
pub struct RawSection {
    /// Heading level (1–6); 0 for a document without an H1.
    pub level: u8,
    /// Heading text.
    pub title: String,
    /// Explicit `{#anchor}`.
    pub anchor: Option<String>,
    /// Access declared on the heading.
    pub decl: Option<AccessDecl>,
    /// 1-based line of the heading.
    pub line: usize,
    /// Byte offset in the source where the section starts: its heading, or for a root
    /// without a heading the first byte after the frontmatter.
    pub start: usize,
    /// Byte offset where the section's own content starts (after its heading).
    pub content_start: usize,
    /// Byte offset where the section ends: the next top-level heading, or the end.
    pub end: usize,
    /// The section's Markdown: a clean heading line (attributes removed) and the body.
    pub markdown: String,
    /// Plain text of the body (for indexing).
    pub plain: String,
    /// Text of the first two paragraphs.
    pub paragraphs: Vec<String>,
    /// Link destinations in the body.
    pub links: Vec<String>,
}

/// A parsed document.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedDoc {
    /// Frontmatter (default when absent).
    pub front: Frontmatter,
    /// Sections in order; the first is the document root.
    pub sections: Vec<RawSection>,
}

/// The title of the heading `text` (its own source lines) parsed alone: its text and
/// code, without its attributes. `None` when `text` alone is no heading (one inside a
/// quote, whose lines need their markers).
fn heading_title(text: &str, options: Options) -> Option<String> {
    let mut title: Option<String> = None;
    for event in Parser::new_ext(text, options) {
        match event {
            Event::Start(Tag::Heading { .. }) if title.is_none() => title = Some(String::new()),
            Event::End(TagEnd::Heading(_)) => break,
            Event::Text(t) | Event::Code(t) => {
                if let Some(title) = title.as_mut() {
                    title.push_str(&t);
                }
            }
            _ => {}
        }
    }
    title
}

struct HeadingSpan {
    level: u8,
    start: usize,
    end: usize,
    title: String,
    anchor: Option<String>,
    attrs: Vec<(String, Option<String>)>,
    /// Inside a block quote, list item or footnote: part of the body, not a boundary.
    nested: bool,
}

/// The frontmatter of a source: the byte range of its YAML (between the `---` lines),
/// and the offset where the Markdown body starts. Offsets count a leading UTF-8 byte
/// order mark, which is skipped.
///
/// A first line of `---` (trailing whitespace allowed) opens frontmatter and a later
/// line of `---` closes it. An opener without a close, or a first non-blank line that
/// starts with `---` anywhere else (indented, after blank lines, `----`), is an error:
/// such a document could be read with or without its declarations.
pub fn split_frontmatter(source: &str) -> std::result::Result<(Option<Range<usize>>, usize), String> {
    let start = if source.starts_with('\u{feff}') {
        '\u{feff}'.len_utf8()
    } else {
        0
    };
    let text = &source[start..];
    let first_end = text.find('\n').map_or(text.len(), |i| i + 1);
    if text[..first_end].trim_end() == "---" {
        let mut offset = first_end;
        for line in text[first_end..].split_inclusive('\n') {
            if line.trim_end() == "---" {
                return Ok((
                    Some(start + first_end..start + offset),
                    start + offset + line.len(),
                ));
            }
            offset += line.len();
        }
        return Err("frontmatter opened with `---` is never closed by a `---` line".into());
    }
    if text
        .lines()
        .find(|line| !line.trim().is_empty())
        .is_some_and(|line| line.trim_start().starts_with("---"))
    {
        return Err("a document that starts with `---` must open its frontmatter on the first line".into());
    }
    Ok((None, start))
}

/// Parses frontmatter YAML: empty (or only comments) is the default, otherwise a
/// mapping of known keys.
fn parse_frontmatter(yaml: &str) -> std::result::Result<Frontmatter, serde_yaml_ng::Error> {
    let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml)?;
    if value.is_null() {
        return Ok(Frontmatter::default());
    }
    serde_yaml_ng::from_value(value)
}

/// Text left in a heading title that looks like an access attribute pulldown-cmark did
/// not parse (`## Pricing {access=finance} draft`): the declaration would silently not
/// apply, so it is an error.
fn unparsed_attributes(title: &str) -> bool {
    static PATTERN: OnceLock<Option<regex::Regex>> = OnceLock::new();
    PATTERN
        .get_or_init(|| regex::Regex::new(r"\{[^}]*\b(access|label|groups)\s*=").ok())
        .as_ref()
        .is_none_or(|re| re.is_match(title))
}

fn line_of(line_starts: &[usize], offset: usize) -> usize {
    match line_starts.binary_search(&offset) {
        Ok(index) => index + 1,
        Err(index) => index,
    }
}

/// The Markdown extensions Kiwi parses with.
pub fn markdown_options() -> Options {
    Options::ENABLE_HEADING_ATTRIBUTES
        | Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
}

/// Parses one Markdown file. `path` is used for error messages only.
pub fn parse(path: &str, source: &str) -> Result<ParsedDoc> {
    let doc_err = |line: usize, message: String| CompileError::Document {
        path: path.to_owned(),
        line,
        message,
    };
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(source.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let (yaml, body_start) = split_frontmatter(source).map_err(|message| doc_err(1, message))?;
    let front: Frontmatter = match yaml {
        Some(range) => parse_frontmatter(&source[range.clone()]).map_err(|e| {
            let first = line_of(&line_starts, range.start);
            doc_err(
                first + e.location().map_or(0, |l| l.line().saturating_sub(1)),
                format!("frontmatter: {e}"),
            )
        })?,
        None => Frontmatter::default(),
    };
    let body = &source[body_start..];
    let options = markdown_options();

    // Pass 1: heading spans. Only top-level headings bound sections; a heading inside
    // a block quote, list item or footnote stays in its section's body.
    let mut headings: Vec<HeadingSpan> = Vec::new();
    let mut current: Option<HeadingSpan> = None;
    let mut containers = 0usize;
    for (event, range) in Parser::new_ext(body, options).into_offset_iter() {
        match event {
            Event::Start(Tag::BlockQuote(_) | Tag::Item | Tag::FootnoteDefinition(_)) => containers += 1,
            Event::End(TagEnd::BlockQuote(_) | TagEnd::Item | TagEnd::FootnoteDefinition) => {
                containers = containers.saturating_sub(1);
            }
            Event::Start(Tag::Heading { level, id, attrs, .. }) => {
                current = Some(HeadingSpan {
                    level: level as u8,
                    start: range.start,
                    end: range.end,
                    title: String::new(),
                    anchor: id.map(|i| i.to_string()),
                    attrs: attrs
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v.map(|v| v.to_string())))
                        .collect(),
                    nested: containers > 0,
                });
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(span) = current.take() {
                    headings.push(span);
                }
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some(span) = current.as_mut() {
                    span.title.push_str(&text);
                }
            }
            _ => {}
        }
    }

    // A heading's title is read from its own lines alone: a link reference or footnote
    // definition elsewhere in the document (perhaps in a section some readers cannot
    // see) never changes it.
    for span in &mut headings {
        if let Some(title) = body
            .get(span.start..span.end)
            .and_then(|text| heading_title(text, options))
        {
            span.title = title;
        }
    }
    for span in &headings {
        let line = line_of(&line_starts, body_start + span.start);
        if unparsed_attributes(&span.title) {
            return Err(doc_err(
                line,
                "heading attributes must close the heading line, as in `## Title {#anchor access=group}`"
                    .into(),
            ));
        }
        if span.nested && (span.anchor.is_some() || !span.attrs.is_empty()) {
            return Err(doc_err(
                line,
                "a heading inside a quote, list or footnote cannot carry an anchor or access".into(),
            ));
        }
    }
    headings.retain(|h| !h.nested);
    // A section starts at the beginning of its heading's line, indentation included.
    for span in &mut headings {
        let line_start = body[..span.start].rfind('\n').map_or(0, |i| i + 1);
        if body[line_start..span.start].trim().is_empty() {
            span.start = line_start;
        }
    }

    // Section boundaries: the root is the first H1 when the document starts with one,
    // else a synthetic root holding the preamble.
    let mut sections = Vec::new();
    let first_is_h1 = headings.first().is_some_and(|h| h.level == 1);
    let preamble_end = headings.first().map(|h| h.start).unwrap_or(body.len());
    if !first_is_h1 {
        let fallback = front.title.clone().unwrap_or_else(|| {
            path.rsplit('/')
                .next()
                .unwrap_or(path)
                .trim_end_matches(".md")
                .to_owned()
        });
        sections.push(build_section(
            path,
            body,
            body_start,
            (0, 0, preamble_end),
            0,
            fallback,
            None,
            None,
            1,
            options,
        )?);
    }
    for (i, span) in headings.iter().enumerate() {
        let end = headings.get(i + 1).map(|h| h.start).unwrap_or(body.len());
        let attrs: Vec<(&str, Option<&str>)> = span
            .attrs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_deref()))
            .collect();
        let line = line_of(&line_starts, body_start + span.start);
        let decl = decl_from_attrs(&attrs).map_err(|e| doc_err(line, e.to_string()))?;
        let title = span.title.trim().to_owned();
        sections.push(build_section(
            path,
            body,
            body_start,
            (span.start, span.end, end),
            span.level,
            title,
            span.anchor.clone(),
            decl,
            line,
            options,
        )?);
    }
    Ok(ParsedDoc { front, sections })
}

/// Builds one section from `body[content_start..end]`; `spans` are (heading start,
/// content start, end) within `body`, which starts at `body_start` in the source.
#[allow(clippy::too_many_arguments)]
fn build_section(
    path: &str,
    body: &str,
    body_start: usize,
    spans: (usize, usize, usize),
    level: u8,
    title: String,
    anchor: Option<String>,
    decl: Option<AccessDecl>,
    line: usize,
    options: Options,
) -> Result<RawSection> {
    if title.is_empty() && level > 0 {
        return Err(CompileError::Document {
            path: path.to_owned(),
            line,
            message: "empty heading".into(),
        });
    }
    let (start, content_start, end) = spans;
    // Line endings are normalised once here, so a section's text (and every hash and
    // paragraph taken from it) is the same whether the file uses LF or CRLF.
    let content = body.get(content_start..end).unwrap_or("").replace("\r\n", "\n");
    let content = content.trim_matches('\n');
    let markdown = if level == 0 {
        content.to_owned()
    } else if content.is_empty() {
        format!("{} {}", "#".repeat(level as usize), title)
    } else {
        format!("{} {}\n\n{}", "#".repeat(level as usize), title, content)
    };
    let mut plain = String::new();
    let mut paragraphs: Vec<String> = Vec::new();
    let mut paragraph: Option<String> = None;
    let mut links = Vec::new();
    for event in Parser::new_ext(content, options) {
        match event {
            Event::Start(Tag::Paragraph) => paragraph = Some(String::new()),
            Event::End(TagEnd::Paragraph) => {
                if let Some(text) = paragraph.take() {
                    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
                    if paragraphs.len() < 2 && !text.is_empty() {
                        paragraphs.push(text);
                    }
                }
                plain.push('\n');
            }
            Event::Start(Tag::Link { dest_url, .. }) => links.push(dest_url.to_string()),
            Event::Text(text) | Event::Code(text) => {
                plain.push_str(&text);
                if let Some(p) = paragraph.as_mut() {
                    p.push_str(&text);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                plain.push(' ');
                if let Some(p) = paragraph.as_mut() {
                    p.push(' ');
                }
            }
            Event::End(TagEnd::Item | TagEnd::TableCell | TagEnd::CodeBlock | TagEnd::Heading(_)) => {
                plain.push('\n');
            }
            _ => {}
        }
    }
    Ok(RawSection {
        level,
        title,
        anchor,
        decl,
        line,
        start: body_start + start,
        content_start: body_start + content_start,
        end: body_start + end,
        markdown,
        plain: plain.trim().to_owned(),
        paragraphs,
        links,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const DOC: &str = "---\nid: RFC-042\ntype: rfc\nstatus: approved\nreview_by: 2027-03-01\nimplements: PRD-017\ncode: [\"services/payment/**\"]\naccess: { label: internal, groups: [eng] }\n---\n\n# Saga payments\n\nIntro text. More.\n\n## Pricing {#pricing access=finance}\n\nSecret [link](../prd.md) here.\n\n### Sub\n\nChild body.\n\n```\n# not a heading\n```\n";

    #[test]
    fn parses_frontmatter_and_sections() {
        let doc = parse("rfcs/rfc-042.md", DOC).unwrap();
        assert_eq!(doc.front.id.as_deref(), Some("RFC-042"));
        assert_eq!(doc.front.review_by.as_deref(), Some("2027-03-01"));
        assert_eq!(doc.front.implements, vec!["PRD-017"]);
        assert_eq!(doc.sections.len(), 3);
        let root = &doc.sections[0];
        assert_eq!(root.title, "Saga payments");
        assert_eq!(root.paragraphs[0], "Intro text. More.");
        let pricing = &doc.sections[1];
        assert_eq!(pricing.anchor.as_deref(), Some("pricing"));
        assert_eq!(pricing.decl.as_ref().unwrap().groups, vec!["finance"]);
        assert!(pricing.markdown.starts_with("## Pricing\n\nSecret"));
        assert_eq!(pricing.links, vec!["../prd.md"]);
        assert_eq!(pricing.line, 15);
        let sub = &doc.sections[2];
        assert_eq!(sub.level, 3);
        assert!(sub.markdown.contains("# not a heading"));
    }

    #[test]
    fn documents_without_h1_get_a_root() {
        let doc = parse("notes/plain.md", "Just text.\n\n## Part {#part}\n\nBody.\n").unwrap();
        assert_eq!(doc.sections[0].title, "plain");
        assert_eq!(doc.sections[0].level, 0);
        assert_eq!(doc.sections[1].anchor.as_deref(), Some("part"));
    }

    #[test]
    fn bad_frontmatter_is_located() {
        let err = parse("x.md", "---\nid: [unclosed\n---\n# T\n").unwrap_err();
        assert!(matches!(err, CompileError::Document { .. }));
        let err = parse("x.md", "# T\n\n## S {#s groups=eng}\n\nBody\n").unwrap_err();
        assert!(err.to_string().contains("x.md:3"));
    }

    fn access_of(source: &str) -> Option<String> {
        let doc = parse("d.md", source).unwrap();
        doc.front.access.map(|a| a.to_decl().unwrap().groups.join(","))
    }

    #[test]
    fn a_title_is_read_from_its_own_heading() {
        let doc = "# D\n\n## See [g] and [^n] {#see}\n\nText.\n\n## Defs {#defs access=finance}\n\n[g]: https://example.com\n\n[^n]: A note.\n";
        let without = "# D\n\n## See [g] and [^n] {#see}\n\nText.\n";
        let title = |source: &str| {
            parse("d.md", source)
                .unwrap()
                .sections
                .iter()
                .find(|s| s.anchor.as_deref() == Some("see"))
                .unwrap()
                .title
                .clone()
        };
        assert_eq!(title(doc), title(without));
        assert_eq!(title(doc), "See [g] and [^n]");
    }

    #[test]
    fn line_endings_do_not_change_a_section() {
        let lf = "---\nid: D\n---\n# D\n\nOne.\nTwo.\n\nThree.\n\n## S {#s}\n\nFour.\n";
        let crlf = lf.replace('\n', "\r\n");
        let (a, b) = (parse("d.md", lf).unwrap(), parse("d.md", &crlf).unwrap());
        let texts =
            |doc: &ParsedDoc| -> Vec<String> { doc.sections.iter().map(|s| s.markdown.clone()).collect() };
        assert_eq!(texts(&a), texts(&b));
        assert!(texts(&b).iter().all(|t| !t.contains('\r')));
    }

    #[test]
    fn frontmatter_declarations_fail_closed() {
        // Read as written: a byte order mark, trailing whitespace on the fences, CRLF.
        assert_eq!(
            access_of("\u{feff}---\naccess: finance\n---\n# D\n").as_deref(),
            Some("finance")
        );
        assert_eq!(
            access_of("--- \naccess: finance\n---\t\n# D\n").as_deref(),
            Some("finance")
        );
        assert_eq!(
            access_of("---\r\naccess: finance\r\n---\r\n# D\r\n").as_deref(),
            Some("finance")
        );
        assert_eq!(access_of("---\n# only a comment\n---\n# D\n"), None);
        // Anything that could be read with or without its declarations is an error.
        for source in [
            "---\naccess: finance\n# D\n\nNever closed.\n",
            "\n---\naccess: finance\n---\n# D\n",
            "  ---\naccess: finance\n---\n# D\n",
            "\u{feff}\n---\naccess: finance\n---\n# D\n",
            "----\naccess: finance\n---\n# D\n",
            "---\nacess: finance\n---\n# D\n",
            "---\naccess: { label: internal, group: [eng] }\n---\n# D\n",
            "---\naccess: finance\naccess: public\n---\n# D\n",
            "---\n- a list\n---\n# D\n",
            "---\naccess:\n---\n# D\n",
            "---\naccess: null\n---\n# D\n",
            "---\naccess: ~\n---\n# D\n",
        ] {
            assert!(parse("d.md", source).is_err(), "{source:?} must not parse");
        }
    }

    #[test]
    fn heading_attributes_fail_closed() {
        for source in [
            "# D\n\n## S {#s acess=finance}\n\nBody\n",
            "# D\n\n## S {#s access=finance} draft\n\nBody\n",
            "# D\n\n## S {access = finance}\n\nBody\n",
            "# D\n\n> ## Quoted {access=finance}\n\nBody\n",
            "# D\n\n- item\n\n  ## In a list {#x}\n",
        ] {
            assert!(parse("d.md", source).is_err(), "{source:?} must not parse");
        }
        // A heading inside a quote without attributes is body text, not a section.
        let doc = parse("d.md", "# D\n\n> ## Quoted\n> text\n\n## S {#s}\n\nBody\n").unwrap();
        assert_eq!(doc.sections.len(), 2);
        assert!(doc.sections[0].markdown.contains("> ## Quoted"));
        // Setext headings carry attributes like ATX ones.
        let doc = parse(
            "d.md",
            "# D\n\nPricing {#pricing access=finance}\n---\n\nSecret.\n",
        )
        .unwrap();
        assert_eq!(doc.sections[1].anchor.as_deref(), Some("pricing"));
        assert_eq!(doc.sections[1].decl.as_ref().unwrap().groups, vec!["finance"]);
    }

    #[test]
    fn sections_carry_their_byte_spans() {
        let source = "\u{feff}---\nid: X\n---\nIntro.\n\n## One {#one}\n\nFirst.\n\n   ## Two\n\nSecond.\n\nThree\n=====\n\nThird.\n";
        let doc = parse("d.md", source).unwrap();
        let spans: Vec<(&str, &str)> = doc
            .sections
            .iter()
            .map(|s| (&source[s.start..s.content_start], &source[s.content_start..s.end]))
            .collect();
        assert_eq!(spans[0], ("", "Intro.\n\n"));
        assert!(spans[1].0.starts_with("## One"), "{spans:?}");
        assert_eq!(spans[1].1.trim(), "First.");
        assert!(spans[2].0.starts_with("   ## Two"), "{spans:?}");
        assert_eq!(spans[2].1.trim(), "Second.");
        assert!(spans[3].0.starts_with("Three\n====="), "{spans:?}");
        assert_eq!(&source[doc.sections[3].end..], "");
        for pair in doc.sections.windows(2) {
            assert_eq!(pair[0].end, pair[1].start, "sections are contiguous");
        }
    }
}
