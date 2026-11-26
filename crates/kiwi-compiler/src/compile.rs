//! Compiling one document into atoms. A compiled document depends only on its own
//! bytes, its resolved access and the configuration, so it is cached by those.

use std::collections::{BTreeMap, BTreeSet};

use kiwi_core::model::{AccessDecl, Meta, Tokens};
use kiwi_core::text;
use kiwi_policy::PathDefaults;
use serde::{Deserialize, Serialize};

use crate::config::KiwiConfig;
use crate::error::{CompileError, Result};
use crate::parse::{self, RawSection};
use kiwi_core::date;

const L1_MAX_CHARS: usize = 320;

/// Counts tokens with the `cl100k` vocabulary, as an estimate for budgets.
pub fn count_tokens(text: &str) -> u32 {
    tiktoken_rs::cl100k_base_singleton().encode_ordinary(text).len() as u32
}

/// Typed references declared in frontmatter, unresolved.
#[derive(
    Debug,
    Clone,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub struct Refs {
    /// `implements`.
    pub implements: Vec<String>,
    /// `supersedes`.
    pub supersedes: Vec<String>,
    /// `decided_by`.
    pub decided_by: Vec<String>,
    /// `depends_on`.
    pub depends_on: Vec<String>,
    /// `amends`.
    pub amends: Vec<String>,
    /// `amended_by`.
    pub amended_by: Vec<String>,
    /// `code` globs.
    pub code: Vec<String>,
}

/// Where one section lies in its source and the access chain it carries.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
pub struct SectionSpan {
    /// Byte offset of its heading (or of the body, for a root without a heading).
    pub start: u32,
    /// Byte offset where it ends.
    pub end: u32,
    /// Access chain.
    pub chain: Vec<AccessDecl>,
}

/// One atom of a compiled document.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
pub struct CompiledAtom {
    /// Atom id.
    pub id: String,
    /// Anchor (empty for the root).
    pub anchor: String,
    /// Parent index within the document.
    pub parent: Option<usize>,
    /// Depth (0 for the root).
    pub depth: u8,
    /// BLAKE3 of the content and access chain.
    pub hash: [u8; 32],
    /// Heading text.
    pub title: String,
    /// L0 breadcrumb.
    pub l0: String,
    /// L1 abstract.
    pub l1: String,
    /// Whether L1 came from frontmatter (an enricher never replaces it).
    pub l1_authored: bool,
    /// Token counts.
    pub tokens: Tokens,
    /// Access chain.
    pub chain: Vec<AccessDecl>,
    /// Ids of sections merged into this atom.
    pub aliases: Vec<String>,
    /// Heading line.
    pub line: u32,
    /// L2 Markdown.
    pub body: String,
    /// Plain text for indexers.
    pub plain: String,
    /// Link targets as corpus ids (`path` or `path#anchor`), or `missing:` prefixes for
    /// relative links that resolve nowhere in the corpus layout.
    pub links: Vec<String>,
    /// Words that look like document ids (`adr-005`), resolved at link time.
    pub id_words: Vec<String>,
}

/// A compiled document.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
pub struct CompiledDoc {
    /// Path without `.md`.
    pub path: String,
    /// BLAKE3 of the file bytes.
    pub file_hash: [u8; 32],
    /// Frontmatter id.
    pub doc_id: Option<String>,
    /// Title.
    pub title: String,
    /// Metadata.
    pub meta: Meta,
    /// Frontmatter references.
    pub refs: Refs,
    /// Atoms in document order; the first is the root.
    pub atoms: Vec<CompiledAtom>,
    /// Glossary entries: term, aliases, atom index.
    pub glossary: Vec<(String, Vec<String>, usize)>,
    /// Every section as written, in source order, so the document can be cut to what
    /// one reader may see without parsing it again.
    pub sections: Vec<SectionSpan>,
}

struct Node {
    section: RawSection,
    parent: Option<usize>,
    anchor: String,
    id: String,
    chain: Vec<AccessDecl>,
    aliases: Vec<String>,
    removed: bool,
    part: Option<(usize, usize)>,
    /// Short sections merged into this one: (source index, Markdown, plain text, links,
    /// id), appended in source order once merging is done.
    merged: Vec<(usize, String, String, Vec<String>, String)>,
    /// The node this one was merged into.
    merged_into: Option<usize>,
}

impl Node {
    fn combined_markdown(&self) -> String {
        let mut merged: Vec<&(usize, String, String, Vec<String>, String)> = self.merged.iter().collect();
        merged.sort_by_key(|m| m.0);
        let mut markdown = self.section.markdown.clone();
        for (_, child, _, _, _) in merged {
            markdown.push_str("\n\n");
            markdown.push_str(child);
        }
        markdown
    }
}

/// One section as written, once its anchor and access are resolved: where it lies in
/// the source and which atom carries its text. Proposals edit sections by these spans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionInfo {
    /// The section's id (`path` for the root, else `path#anchor`).
    pub id: String,
    /// Every id that names it: its id and, for a root with `{#anchor}`, `path#anchor`.
    pub ids: Vec<String>,
    /// Heading level (0 for a root without a heading).
    pub level: u8,
    /// Heading text.
    pub title: String,
    /// Access chain.
    pub chain: Vec<AccessDecl>,
    /// Byte offset of its heading (or of the body, for a root without a heading).
    pub start: usize,
    /// Byte offset of its own content.
    pub content_start: usize,
    /// Byte offset where it ends (the next top-level heading).
    pub end: usize,
    /// Its own Markdown (clean heading line and content, without merged sections).
    pub markdown: String,
    /// Link destinations in its own content.
    pub links: Vec<String>,
    /// The atom that carries its text: itself, or the section it merged into.
    pub atom: String,
}

/// Compiles `source` (the file at corpus path `file`, with `.md`) with the access and
/// default owner `KIWIOWNERS` gives it, `defaults`; its frontmatter replaces either.
pub fn compile_doc(
    file: &str,
    source: &str,
    defaults: &PathDefaults,
    config: &KiwiConfig,
) -> Result<CompiledDoc> {
    compile_with_outline(file, source, defaults, config).map(|(doc, _)| doc)
}

/// Compiles a document and returns its sections as written, in source order.
pub fn compile_with_outline(
    file: &str,
    source: &str,
    defaults: &PathDefaults,
    config: &KiwiConfig,
) -> Result<(CompiledDoc, Vec<SectionInfo>)> {
    let parsed = parse::parse(file, source)?;
    let front = parsed.front;
    let path = file.trim_end_matches(".md").to_owned();
    let doc_err = |line: usize, message: String| CompileError::Document {
        path: file.to_owned(),
        line,
        message,
    };
    if let Some(review_by) = &front.review_by
        && !date::is_date(review_by)
    {
        return Err(doc_err(1, format!("review_by `{review_by}` is not YYYY-MM-DD")));
    }
    let doc_access = match &front.access {
        Some(access) => access.to_decl()?,
        None => defaults.access.clone(),
    };
    let doc_type = text::key(front.doc_type.as_deref().unwrap_or("doc"));
    let status = text::key(front.status.as_deref().unwrap_or("draft"));
    if !matches!(status.as_str(), "draft" | "review" | "approved" | "deprecated") {
        return Err(doc_err(
            1,
            format!("status `{status}` is not draft, review, approved or deprecated"),
        ));
    }
    let glossary_doc = doc_type == "glossary";

    // Tree, access chains and anchors. The root's own heading attributes narrow the
    // whole document and its `{#anchor}` names the root.
    //
    // An explicit anchor must be unique in the document. A heading without one gets
    // its slug, numbered (`pricing-1`) only among sections of the same access chain:
    // whoever reads one of those reads them all, so a visible section's id never
    // depends on a heading its reader cannot see. Two sections of different chains
    // that would share an anchor are an error.
    let mut nodes: Vec<Node> = Vec::with_capacity(parsed.sections.len());
    let mut stack: Vec<(u8, usize)> = Vec::new();
    let mut anchors: BTreeMap<String, usize> = BTreeMap::new();
    let mut slugs_by_chain: BTreeMap<Vec<AccessDecl>, BTreeSet<String>> = BTreeMap::new();
    for (index, section) in parsed.sections.into_iter().enumerate() {
        if index == 0 {
            let mut chain = vec![doc_access.clone()];
            if let Some(decl) = &section.decl
                && decl != &doc_access
            {
                chain.push(decl.clone());
            }
            let mut aliases = Vec::new();
            if let Some(anchor) = &section.anchor {
                anchors.insert(anchor.clone(), section.line);
                slugs_by_chain
                    .entry(chain.clone())
                    .or_default()
                    .insert(anchor.clone());
                aliases.push(format!("{path}#{anchor}"));
            }
            stack.push((section_level(&section), 0));
            nodes.push(Node {
                parent: None,
                anchor: String::new(),
                id: path.clone(),
                chain,
                aliases,
                removed: false,
                part: None,
                merged: Vec::new(),
                merged_into: None,
                section,
            });
            continue;
        }
        while stack.len() > 1 && stack.last().is_some_and(|&(level, _)| level >= section.level) {
            stack.pop();
        }
        let parent = stack.last().map(|&(_, i)| i).unwrap_or(0);
        let mut chain = nodes[parent].chain.clone();
        if let Some(decl) = &section.decl
            && chain.last() != Some(decl)
        {
            chain.push(decl.clone());
        }
        let used = slugs_by_chain.entry(chain.clone()).or_default();
        let base = match &section.anchor {
            Some(anchor) => {
                if anchors.contains_key(anchor) {
                    return Err(doc_err(section.line, format!("duplicate anchor `{anchor}`")));
                }
                anchor.clone()
            }
            None => {
                let slug = text::slugify(&section.title);
                let mut candidate = slug.clone();
                let mut n = 1;
                while used.contains(&candidate) {
                    candidate = format!("{slug}-{n}");
                    n += 1;
                }
                if let Some(line) = anchors.get(&candidate) {
                    return Err(doc_err(
                        section.line,
                        format!(
                            "this heading and the one on line {line} would both be `#{candidate}`; give one of them an explicit {{#anchor}}"
                        ),
                    ));
                }
                candidate
            }
        };
        used.insert(base.clone());
        anchors.insert(base.clone(), section.line);
        stack.push((section.level, nodes.len()));
        nodes.push(Node {
            parent: Some(parent),
            id: format!("{path}#{base}"),
            anchor: base,
            chain,
            aliases: Vec::new(),
            removed: false,
            part: None,
            merged: Vec::new(),
            merged_into: None,
            section,
        });
    }
    let mut outline: Vec<SectionInfo> = nodes
        .iter()
        .map(|node| {
            let mut ids = vec![node.id.clone()];
            if node.parent.is_none() {
                ids.extend(node.aliases.iter().cloned());
            }
            SectionInfo {
                id: node.id.clone(),
                ids,
                level: node.section.level,
                title: node.section.title.clone(),
                chain: node.chain.clone(),
                start: node.section.start,
                content_start: node.section.content_start,
                end: node.section.end,
                markdown: node.section.markdown.clone(),
                links: node.section.links.clone(),
                atom: node.id.clone(),
            }
        })
        .collect();

    // Merge short leaf sections into their parent (same access, not glossary terms).
    // A section whose only children carry a different access merges all the same, and
    // those children move up to its parent, so whether a section is its own atom never
    // depends on sections its readers may not see. Merged text keeps source order.
    if !glossary_doc {
        for i in (1..nodes.len()).rev() {
            let Some(parent) = nodes[i].parent else {
                continue;
            };
            let chain_children = nodes
                .iter()
                .any(|n| !n.removed && n.parent == Some(i) && n.chain == nodes[i].chain);
            if chain_children
                || nodes[i].chain != nodes[parent].chain
                || count_tokens(&nodes[i].combined_markdown()) >= config.atoms.min_tokens
            {
                continue;
            }
            let mut moved = std::mem::take(&mut nodes[i].merged);
            moved.push((
                i,
                nodes[i].section.markdown.clone(),
                nodes[i].section.plain.clone(),
                nodes[i].section.links.clone(),
                nodes[i].id.clone(),
            ));
            nodes[i].removed = true;
            nodes[i].merged_into = Some(parent);
            for node in nodes.iter_mut() {
                if node.parent == Some(i) {
                    node.parent = Some(parent);
                }
            }
            nodes[parent].merged.append(&mut moved);
        }
    }
    for node in nodes.iter_mut().filter(|n| !n.removed) {
        let mut merged = std::mem::take(&mut node.merged);
        merged.sort_by_key(|m| m.0);
        for (_, markdown, plain, mut links, id) in merged {
            node.section.markdown.push_str("\n\n");
            node.section.markdown.push_str(&markdown);
            node.section.plain.push('\n');
            node.section.plain.push_str(&plain);
            node.section.links.append(&mut links);
            node.aliases.push(id);
        }
    }
    for (i, info) in outline.iter_mut().enumerate() {
        let mut holder = i;
        while let Some(next) = nodes[holder].merged_into {
            holder = next;
        }
        info.atom = nodes[holder].id.clone();
    }

    // Split long sections at paragraph boundaries.
    let mut expanded: Vec<Node> = Vec::with_capacity(nodes.len());
    let mut remap: Vec<usize> = vec![0; nodes.len()];
    for (i, node) in nodes.into_iter().enumerate() {
        if node.removed {
            remap[i] = usize::MAX;
            continue;
        }
        remap[i] = expanded.len();
        let chunks = split_markdown(&node.section.markdown, config.atoms.max_tokens);
        if chunks.len() <= 1 {
            expanded.push(node);
            continue;
        }
        let total = chunks.len();
        for (k, chunk) in chunks.into_iter().enumerate() {
            let mut section = node.section.clone();
            section.markdown = chunk;
            section.plain = strip_markdown(&section.markdown);
            if k > 0 {
                section.paragraphs = first_paragraphs(&section.markdown);
                section.links = Vec::new();
            }
            expanded.push(Node {
                parent: node.parent,
                anchor: if k == 0 {
                    node.anchor.clone()
                } else {
                    format!("{}~{}", node.anchor, k + 1)
                },
                id: if k == 0 {
                    node.id.clone()
                } else if node.anchor.is_empty() {
                    format!("{}#~{}", node.id, k + 1)
                } else {
                    format!("{}~{}", node.id, k + 1)
                },
                chain: node.chain.clone(),
                aliases: if k == 0 { node.aliases.clone() } else { Vec::new() },
                removed: false,
                part: Some((k + 1, total)),
                merged: Vec::new(),
                merged_into: None,
                section,
            });
        }
    }
    for node in &mut expanded {
        if let Some(parent) = node.parent {
            // A split part keeps the original's parent; parents always precede children,
            // so the first part's index is the remapped one.
            node.parent = Some(remap[parent]);
        }
    }

    let title = front
        .title
        .clone()
        .unwrap_or_else(|| expanded[0].section.title.clone());
    let root_l0 = match &front.id {
        Some(id) => format!("{id} · {title}"),
        None => title.clone(),
    };
    let mut atoms: Vec<CompiledAtom> = Vec::with_capacity(expanded.len());
    let mut glossary = Vec::new();
    let dir = match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    };
    for (i, node) in expanded.iter().enumerate() {
        let section = &node.section;
        let heading = match node.part {
            Some((k, total)) if k > 1 => format!("{} (part {k}/{total})", section.title),
            _ => section.title.clone(),
        };
        let (title_text, l0) = if i == 0 {
            (title.clone(), root_l0.clone())
        } else {
            let mut crumbs = vec![heading.clone()];
            let mut cursor = node.parent;
            while let Some(p) = cursor {
                if p == 0 {
                    crumbs.push(root_l0.clone());
                    break;
                }
                crumbs.push(expanded[p].section.title.clone());
                cursor = expanded[p].parent;
            }
            crumbs.reverse();
            (heading.clone(), crumbs.join(" › "))
        };
        let mut paragraphs = section.paragraphs.clone();
        let mut aliases_for_term = Vec::new();
        if glossary_doc && node.parent == Some(0) {
            if let Some(first) = paragraphs.first()
                && let Some(rest) = strip_aliases_prefix(first)
            {
                aliases_for_term = rest
                    .split(',')
                    .map(|a| a.trim().trim_end_matches('.').to_owned())
                    .filter(|a| !a.is_empty())
                    .collect();
                paragraphs.remove(0);
            }
            glossary.push((section.title.clone(), aliases_for_term, i));
        }
        let (l1, l1_authored) = match (&front.summary, i) {
            (Some(summary), 0) => (text::truncate_chars(summary.trim(), L1_MAX_CHARS), true),
            _ => {
                let source = paragraphs
                    .first()
                    .cloned()
                    .unwrap_or_else(|| section.plain.lines().next().unwrap_or("").to_owned());
                (text::first_sentence(&source, L1_MAX_CHARS), false)
            }
        };
        let body = section.markdown.clone();
        let mut hasher = blake3::Hasher::new();
        hasher.update(body.as_bytes());
        for decl in &node.chain {
            hasher.update(decl.label.as_bytes());
            hasher.update(b"\0");
            hasher.update(decl.groups.join(",").as_bytes());
            hasher.update(b"\n");
        }
        let tokens = Tokens {
            l0: count_tokens(&l0),
            l1: count_tokens(&l1),
            l2: count_tokens(&body),
            id: count_tokens(&node.id),
        };
        let folded = text::fold(&section.plain);
        let mut id_words: Vec<String> = text::words(&folded)
            .into_iter()
            .filter(|w| looks_like_doc_id(w))
            .map(str::to_owned)
            .collect();
        id_words.sort();
        id_words.dedup();
        let mut links: Vec<String> = section
            .links
            .iter()
            .filter_map(|dest| resolve_link(dir, &path, dest))
            .collect();
        links.sort();
        links.dedup();
        atoms.push(CompiledAtom {
            id: node.id.clone(),
            anchor: node.anchor.clone(),
            parent: node.parent,
            depth: depth_of(&expanded, i),
            hash: *hasher.finalize().as_bytes(),
            title: title_text,
            l0,
            l1,
            l1_authored,
            tokens,
            chain: node.chain.clone(),
            aliases: node.aliases.clone(),
            line: section.line as u32,
            body,
            plain: section.plain.clone(),
            links,
            id_words,
        });
    }

    let mut tags: Vec<String> = front.tags.iter().map(|t| text::key(t)).collect();
    tags.sort();
    tags.dedup();
    let doc = CompiledDoc {
        path,
        file_hash: *blake3::hash(source.as_bytes()).as_bytes(),
        doc_id: front.id.clone(),
        title,
        meta: Meta {
            doc_type,
            status,
            owner: front.owner.clone().or_else(|| defaults.owner.clone()),
            review_by: front.review_by.clone(),
            tags,
            summary: front.summary.clone(),
        },
        refs: Refs {
            implements: front.implements.clone(),
            supersedes: front.supersedes.clone(),
            decided_by: front.decided_by.clone(),
            depends_on: front.depends_on.clone(),
            amends: front.amends.clone(),
            amended_by: front.amended_by.clone(),
            code: front.code.clone(),
        },
        atoms,
        glossary,
        sections: outline
            .iter()
            .map(|s| SectionSpan {
                start: s.start as u32,
                end: s.end as u32,
                chain: s.chain.clone(),
            })
            .collect(),
    };
    Ok((doc, outline))
}

fn section_level(section: &RawSection) -> u8 {
    section.level.max(1)
}

fn depth_of(nodes: &[Node], index: usize) -> u8 {
    let mut depth = 0u8;
    let mut cursor = nodes[index].parent;
    while let Some(p) = cursor {
        depth = depth.saturating_add(1);
        cursor = nodes[p].parent;
    }
    depth
}

fn strip_aliases_prefix(paragraph: &str) -> Option<&str> {
    let trimmed = paragraph.trim_start_matches(['*', '_']);
    let lower = trimmed.to_ascii_lowercase();
    for prefix in ["aliases:", "alias:"] {
        if lower.starts_with(prefix) {
            return Some(trimmed[prefix.len()..].trim_start_matches(['*', '_']).trim());
        }
    }
    None
}

/// A word like `adr-005`, `rfc-42` or `prd-001`: letters, a hyphen, digits.
fn looks_like_doc_id(word: &str) -> bool {
    let Some((head, tail)) = word.split_once('-') else {
        return false;
    };
    !head.is_empty()
        && head.len() <= 12
        && head.chars().all(|c| c.is_ascii_alphabetic())
        && !tail.is_empty()
        && tail.chars().all(|c| c.is_ascii_digit())
}

/// Resolves a relative Markdown link to a corpus id. External URLs return `None`.
fn resolve_link(dir: &str, path: &str, dest: &str) -> Option<String> {
    if dest.contains("://") || dest.starts_with("mailto:") || dest.is_empty() {
        return None;
    }
    let (target, anchor) = match dest.split_once('#') {
        Some((t, a)) => (t, Some(a)),
        None => (dest, None),
    };
    if target.is_empty() {
        return anchor.map(|a| format!("{path}#{a}"));
    }
    if !target.ends_with(".md") {
        return None;
    }
    let mut parts: Vec<&str> = if target.starts_with('/') || dir.is_empty() {
        Vec::new()
    } else {
        dir.split('/').collect()
    };
    for part in target.trim_start_matches('/').split('/') {
        match part {
            "." | "" => {}
            ".." => {
                if parts.pop().is_none() {
                    return Some(format!("missing:{dest}"));
                }
            }
            other => parts.push(other),
        }
    }
    let resolved = parts.join("/");
    let resolved = resolved.trim_end_matches(".md");
    Some(match anchor {
        Some(a) if !a.is_empty() => format!("{resolved}#{a}"),
        _ => resolved.to_owned(),
    })
}

fn in_fence(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

/// Splits Markdown into chunks of at most `max_tokens` at blank-line boundaries outside
/// code fences. A single oversized paragraph stays whole.
fn split_markdown(markdown: &str, max_tokens: u32) -> Vec<String> {
    if count_tokens(markdown) <= max_tokens {
        return vec![markdown.to_owned()];
    }
    let mut blocks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut fenced = false;
    for line in markdown.lines() {
        if in_fence(line) {
            fenced = !fenced;
        }
        if line.trim().is_empty() && !fenced {
            if !current.trim().is_empty() {
                blocks.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.trim().is_empty() {
        blocks.push(current);
    }
    let mut chunks: Vec<String> = Vec::new();
    let mut chunk = String::new();
    let mut chunk_tokens = 0u32;
    for block in blocks {
        let tokens = count_tokens(&block);
        if chunk_tokens > 0 && chunk_tokens + tokens > max_tokens {
            chunks.push(std::mem::take(&mut chunk).trim_end().to_owned());
            chunk_tokens = 0;
        }
        if !chunk.is_empty() {
            chunk.push('\n');
        }
        chunk.push_str(&block);
        chunk_tokens += tokens;
    }
    if !chunk.trim().is_empty() {
        chunks.push(chunk.trim_end().to_owned());
    }
    chunks
}

fn strip_markdown(markdown: &str) -> String {
    use pulldown_cmark::{Event, Parser};
    let mut out = String::new();
    for event in Parser::new(markdown) {
        match event {
            Event::Text(t) | Event::Code(t) => out.push_str(&t),
            Event::SoftBreak | Event::HardBreak => out.push(' '),
            Event::End(_) => out.push('\n'),
            _ => {}
        }
    }
    out.trim().to_owned()
}

fn first_paragraphs(markdown: &str) -> Vec<String> {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};
    let mut out = Vec::new();
    let mut current: Option<String> = None;
    for event in Parser::new(markdown) {
        match event {
            Event::Start(Tag::Paragraph) => current = Some(String::new()),
            Event::End(TagEnd::Paragraph) => {
                if let Some(p) = current.take()
                    && !p.trim().is_empty()
                    && out.len() < 2
                {
                    out.push(p.split_whitespace().collect::<Vec<_>>().join(" "));
                }
            }
            Event::Text(t) | Event::Code(t) => {
                if let Some(p) = current.as_mut() {
                    p.push_str(&t);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some(p) = current.as_mut() {
                    p.push(' ');
                }
            }
            _ => {}
        }
    }
    out
}

/// The revision of how documents compile, part of every cache key: a compiler that
/// compiles the same bytes differently never reuses an older compiler's output.
const COMPILE_REVISION: &[u8] = b"kiwi-compile-7";

/// A key for the compile cache: everything a compiled document depends on.
pub fn cache_key(file: &str, bytes: &[u8], defaults: &PathDefaults, config: &KiwiConfig) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(COMPILE_REVISION);
    hasher.update(&config.build_hash());
    hasher.update(file.as_bytes());
    hasher.update(b"\0");
    hasher.update(defaults.access.label.as_bytes());
    hasher.update(b"\0");
    hasher.update(defaults.access.groups.join(",").as_bytes());
    hasher.update(b"\0");
    match &defaults.owner {
        Some(owner) => {
            hasher.update(b"1");
            hasher.update(&(owner.len() as u64).to_le_bytes());
            hasher.update(owner.as_bytes());
        }
        None => {
            hasher.update(b"0");
        }
    }
    hasher.update(bytes);
    hasher.finalize().to_hex().to_string()
}

/// Groups glossary entries by folded key, for duplicate-term lint.
pub fn glossary_keys(docs: &[CompiledDoc]) -> BTreeMap<String, Vec<String>> {
    let mut keys: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for doc in docs {
        for (term, aliases, index) in &doc.glossary {
            let id = doc.atoms[*index].id.clone();
            for name in std::iter::once(term).chain(aliases.iter()) {
                keys.entry(text::key(name)).or_default().push(id.clone());
            }
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn internal() -> PathDefaults {
        PathDefaults::access(AccessDecl::new("internal", Vec::new()))
    }

    #[test]
    fn atoms_ids_chains_and_breadcrumbs() {
        let long = "Words here make a long enough body for this section to stay. ".repeat(8);
        let source = format!(
            "---\nid: PRD-001\ntype: prd\nstatus: approved\nsummary: The checkout PRD.\n---\n# Checkout\n\n{long}\n\n## Pricing {{#pricing access=finance}}\n\n{long}\n\n### Tiers\n\n{long}\n\n## Goals\n\n{long} See ADR-005 and [x](../adr/a.md#decision).\n"
        );
        let doc = compile_doc("product/prd.md", &source, &internal(), &KiwiConfig::default()).unwrap();
        let ids: Vec<&str> = doc.atoms.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "product/prd",
                "product/prd#pricing",
                "product/prd#tiers",
                "product/prd#goals"
            ]
        );
        assert_eq!(doc.atoms[0].l1, "The checkout PRD.");
        assert_eq!(doc.atoms[2].l0, "PRD-001 · Checkout › Pricing › Tiers");
        assert_eq!(
            doc.atoms[2].chain.len(),
            2,
            "tiers inherits the finance narrowing"
        );
        assert_eq!(doc.atoms[3].chain.len(), 1);
        assert_eq!(doc.atoms[2].parent, Some(1));
        assert_eq!(doc.atoms[3].id_words, vec!["adr-005"]);
        assert_eq!(doc.atoms[3].links, vec!["adr/a#decision"]);
    }

    #[test]
    fn short_sections_merge_and_long_ones_split() {
        let long = "A sentence that is long enough to count. ".repeat(10);
        let source = format!("# Doc\n\n{long}\n\n## Tiny\n\nShort.\n");
        let doc = compile_doc("d.md", &source, &internal(), &KiwiConfig::default()).unwrap();
        assert_eq!(doc.atoms.len(), 1);
        assert_eq!(doc.atoms[0].aliases, vec!["d#tiny"]);
        assert!(doc.atoms[0].body.contains("## Tiny"));

        let para = "Paragraph text that goes on for a while with many words. ".repeat(12);
        let big = (0..12).map(|_| para.clone()).collect::<Vec<_>>().join("\n\n");
        let source = format!("# Doc\n\nIntro long enough. {long}\n\n## Big {{#big}}\n\n{big}\n");
        let doc = compile_doc("d.md", &source, &internal(), &KiwiConfig::default()).unwrap();
        let parts: Vec<&CompiledAtom> = doc.atoms.iter().filter(|a| a.id.starts_with("d#big")).collect();
        assert!(parts.len() > 1);
        assert_eq!(parts[1].id, "d#big~2");
        assert!(parts.iter().all(|a| a.tokens.l2 <= 800));
        assert_eq!(parts[1].parent, Some(0));
    }

    #[test]
    fn glossary_terms_and_aliases() {
        let source = "---\ntype: glossary\n---\n# Glossary\n\n## Payout {#payout}\n\nAliases: settlement, disbursement\n\nMoney sent to a seller.\n";
        let doc = compile_doc("glossary.md", source, &internal(), &KiwiConfig::default()).unwrap();
        assert_eq!(doc.glossary.len(), 1);
        assert_eq!(doc.glossary[0].1, vec!["settlement", "disbursement"]);
        assert_eq!(doc.atoms[1].l1, "Money sent to a seller.");
    }

    #[test]
    fn standard_documents_keep_their_type_and_metadata() {
        let source = "---\nid: STD-001\ntype: Standard\nstatus: approved\nowner: \"@team-platform\"\nreview_by: 2027-06-30\nsummary: The quality bar.\ndecided_by: [ADR-005]\n---\n# Quality\n\n## Code review {#code-review}\n\nEvery change merges through a reviewed pull request.\n";
        let doc = compile_doc(
            "standards/quality.md",
            source,
            &internal(),
            &KiwiConfig::default(),
        )
        .unwrap();
        assert_eq!(doc.meta.doc_type, "standard");
        assert_eq!(doc.meta.status, "approved");
        assert_eq!(doc.meta.review_by.as_deref(), Some("2027-06-30"));
        assert_eq!(doc.refs.decided_by, vec!["ADR-005"]);
        assert!(doc.glossary.is_empty(), "only glossary documents define terms");
        assert_eq!(doc.atoms[0].l0, "STD-001 · Quality");

        let undated = "---\ntype: standard\n---\n# Quality\n";
        let doc = compile_doc("standards/q.md", undated, &internal(), &KiwiConfig::default()).unwrap();
        assert_eq!(doc.meta.review_by, None, "no review date is invented");
        assert_eq!(doc.meta.status, "draft");
    }

    #[test]
    fn link_resolution() {
        assert_eq!(resolve_link("a/b", "a/b/c", "../d.md"), Some("a/d".into()));
        assert_eq!(resolve_link("", "c", "#x"), Some("c#x".into()));
        assert_eq!(resolve_link("", "c", "https://e.com/a.md"), None);
        assert_eq!(
            resolve_link("", "c", "../../x.md"),
            Some("missing:../../x.md".into())
        );
        assert!(looks_like_doc_id("adr-005"));
        assert!(!looks_like_doc_id("double-entry"));
    }

    #[test]
    fn rejects_bad_metadata() {
        assert!(
            compile_doc(
                "d.md",
                "---\nstatus: shipped\n---\n# D\n",
                &internal(),
                &KiwiConfig::default()
            )
            .is_err()
        );
        assert!(
            compile_doc(
                "d.md",
                "---\nreview_by: soon\n---\n# D\n",
                &internal(),
                &KiwiConfig::default()
            )
            .is_err()
        );
        assert!(
            compile_doc(
                "d.md",
                "# D\n\n## A {#x}\n\nBody\n\n## B {#x}\n\nBody\n",
                &internal(),
                &KiwiConfig::default()
            )
            .is_err()
        );
    }

    fn ids_and_chains(doc: &CompiledDoc) -> Vec<(String, usize)> {
        doc.atoms.iter().map(|a| (a.id.clone(), a.chain.len())).collect()
    }

    #[test]
    fn an_empty_frontmatter_access_is_an_error() {
        for source in [
            "---\naccess:\n---\n# D\n",
            "---\naccess: null\n---\n# D\n",
            "---\naccess: \"\"\n---\n# D\n",
            "---\naccess: { label: \"\" }\n---\n# D\n",
        ] {
            assert!(
                compile_doc("d.md", source, &internal(), &KiwiConfig::default()).is_err(),
                "{source:?} must not fall back to the path rule"
            );
        }
    }

    #[test]
    fn root_heading_attributes_narrow_the_whole_document() {
        let long = "A sentence that is long enough to count for the size rule. ".repeat(10);
        let source = format!(
            "---\nid: FIN-9\n---\n# Budget {{#budget access=finance}}\n\n{long}\n\n## Detail {{#detail}}\n\n{long}\n"
        );
        let doc = compile_doc("plans/b.md", &source, &internal(), &KiwiConfig::default()).unwrap();
        let finance = AccessDecl::new("confidential", ["finance".to_owned()]);
        assert_eq!(doc.atoms[0].chain, vec![internal().access, finance.clone()]);
        assert_eq!(
            doc.atoms[1].chain,
            vec![internal().access, finance],
            "children inherit it"
        );
        assert_eq!(
            doc.atoms[0].aliases,
            vec!["plans/b#budget"],
            "the root's anchor names it"
        );
        let clash = format!("# Budget {{#budget}}\n\n{long}\n\n## Budget {{#budget}}\n\n{long}\n");
        assert!(compile_doc("b.md", &clash, &internal(), &KiwiConfig::default()).is_err());
        // A declaration equal to the document's own adds nothing.
        let same = format!("# Plain {{label=internal}}\n\n{long}\n");
        let doc = compile_doc("p.md", &same, &internal(), &KiwiConfig::default()).unwrap();
        assert_eq!(doc.atoms[0].chain, vec![internal().access]);
    }

    #[test]
    fn merged_sections_keep_source_order() {
        let long = "A sentence that is long enough to count for the size rule. ".repeat(10);
        let source =
            format!("# Doc\n\n{long}\n\n## Alpha\n\nOne.\n\n## Beta\n\nTwo.\n\n## Gamma\n\nThree.\n");
        let (doc, outline) =
            compile_with_outline("d.md", &source, &internal(), &KiwiConfig::default()).unwrap();
        assert_eq!(doc.atoms.len(), 1);
        let body = &doc.atoms[0].body;
        let (a, b, c) = (
            body.find("## Alpha").unwrap(),
            body.find("## Beta").unwrap(),
            body.find("## Gamma").unwrap(),
        );
        assert!(a < b && b < c, "{body}");
        assert_eq!(doc.atoms[0].aliases, vec!["d#alpha", "d#beta", "d#gamma"]);
        assert!(outline.iter().skip(1).all(|s| s.atom == "d"));
        assert_eq!(outline[2].markdown, "## Beta\n\nTwo.", "each keeps its own text");
    }

    #[test]
    fn visible_ids_do_not_depend_on_hidden_sections() {
        let long = "A sentence that is long enough to count for the size rule. ".repeat(10);
        // A short section merges into its parent whether or not it has a child its
        // readers may not see; the hidden child moves up to the parent.
        let full =
            format!("# Doc\n\n{long}\n\n## Overview\n\nShort.\n\n### Margin {{access=finance}}\n\n{long}\n");
        let stripped = format!("# Doc\n\n{long}\n\n## Overview\n\nShort.\n");
        let config = KiwiConfig::default();
        let full = compile_doc("d.md", &full, &internal(), &config).unwrap();
        let stripped = compile_doc("d.md", &stripped, &internal(), &config).unwrap();
        assert_eq!(
            ids_and_chains(&full),
            vec![("d".into(), 1), ("d#margin".into(), 2)]
        );
        assert_eq!(full.atoms[0].body, stripped.atoms[0].body);
        assert_eq!(full.atoms[0].aliases, stripped.atoms[0].aliases);
        assert_eq!(full.atoms[1].parent, Some(0));
        // Slugs are numbered among sections of one access chain; two chains that would
        // share an anchor are an error rather than a number that reveals the other.
        let same = format!("# Doc\n\n{long}\n\n## Notes\n\n{long}\n\n## Notes\n\n{long}\n");
        let doc = compile_doc("d.md", &same, &internal(), &config).unwrap();
        assert_eq!(doc.atoms[2].id, "d#notes-1");
        let crossed =
            format!("# Doc\n\n{long}\n\n## Pricing {{access=finance}}\n\n{long}\n\n## Pricing\n\n{long}\n");
        let err = compile_doc("d.md", &crossed, &internal(), &config).unwrap_err();
        assert!(err.to_string().contains("explicit"), "{err}");
    }
}
