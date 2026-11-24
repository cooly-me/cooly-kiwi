---
name: kiwi-author
description: Set up or restructure a Markdown knowledge base so Kiwi compiles it well - corpus layout, document types (vision, prd, rfc, adr, runbook, standard, glossary), frontmatter, heading anchors, L1 summaries, typed links, KIWIOWNERS and access attributes, the Cedar policy and the principal directory. Use when creating a new Kiwi corpus, migrating docs into one, or reviewing why Kiwi answers poorly from existing docs.
---

# Authoring a knowledge base for Kiwi

Kiwi compiles a Git repository of Markdown into a pack of **atoms** (one per heading
section) and serves them to agents with permissions applied per section. How you write
the Markdown decides how well agents find and use it. This skill covers the corpus
layout, the document conventions and the governance files.

## 1. Layout

```text
kb/                          # the corpus root (a repository, or a folder in one)
  .kiwi/
    kiwi.toml                # configuration (optional; defaults are sensible)
    policy.cedar             # who may read which labels (required: default is deny)
    policy.tests.yaml        # unit tests for the policy (`kiwi policy test`)
    principals.yaml          # users, their groups, and agent grants
  KIWIOWNERS                 # access and default owner per path, CODEOWNERS style
  vision.md
  glossary.md
  product/<area>/prd.md
  rfcs/rfc-NNN-<slug>.md
  adr/adr-NNN-<slug>.md
  runbooks/<service>.md
  standards/<topic>.md
```

- File paths become ids (`adr/adr-005-idempotency-keys`), so name files once and keep
  them. Renaming a file changes every id inside it.
- One topic per document. Group by product area or by document type, not by team:
  teams change, topics do not.
- `README.md` files are not compiled; put knowledge in named documents.

## 2. Document types

| `type` | Holds | Typical sections (`{#anchor}`) |
| --- | --- | --- |
| `vision` | why the product exists, goals, principles | mission, customers, goals, principles, non-goals, metrics |
| `prd` | what to build and why | goals, user-stories, requirements, metrics, open-questions |
| `rfc` | how to build it | summary, design, alternatives, rollout, risks |
| `adr` | one decision and its reasons | context, decision, consequences, alternatives |
| `runbook` | how to operate something | symptoms, diagnosis, mitigation, escalation |
| `standard` | a rule or reference that stays in force: quality bars, design systems, writing guides, security guarantees, architecture rules | scope, one section per rule or area, exceptions |
| `glossary` | terms and aliases | one `##` per term |

Lifecycle: `draft -> review -> approved -> deprecated`. `approved` content wins
near ties in search, `draft`/`review` content is flagged as unapproved, and
`deprecated` documents redirect to whatever `supersedes` them. Set `review_by` on
everything; overdue documents are flagged to agents and to `kiwi lint`.

Pick `standard` for anything that tells people how work must be done and stays in
force until someone changes it. Do not file it as an approved `rfc` (an RFC is a design
for one piece of work) or as an `adr` (one decision with alternatives); readers and
agents filtering by type would draw the wrong conclusion. A standard that rests on a
decision names it with `decided_by`, so `kiwi_trace` shows the rule's reasons. Set
`review_by` on standards like on PRDs: a rule nobody re-reads drifts from practice.

See `references/frontmatter.md` for every frontmatter key.

## 3. Sections are atoms

- Every `##`/`###` heading gets an explicit anchor: `## Rollout plan {#rollout}`.
  Without one Kiwi derives a slug from the title, which changes when the title does.
- Aim for 60-500 words per section. Kiwi merges sections under 50 tokens into their
  parent (when access matches) and splits sections over 800 tokens at paragraphs.
- The **first sentence** of each section is its L1 abstract: the line agents see in
  search results and maps. Write it as a summary ("Payouts run daily at 02:00 UTC for
  balances above the minimum."), not as a transition ("As mentioned above, ...").
- The document's `summary` frontmatter is the L1 of the document itself.
- Do not repeat the document title in every heading; the breadcrumb already carries it
  (`ADR-005 · Idempotency keys › Decision`).

## 4. Links and the graph

- Typed links go in frontmatter: `implements`, `decided_by`, `depends_on`,
  `supersedes`, `amends` / `amended_by` (document ids; an amendment changes part of a
  document without replacing it, and both sides declare it), and `code` (path globs).
  They power `kiwi_trace`.
- Mentions in the text (`see ADR-005`) and relative Markdown links
  (`[ADR-005](../adr/adr-005-idempotency-keys.md#decision)`) become `mentions` edges.
- `kiwi lint` fails on links and references that resolve nowhere.

## 5. The glossary

One document with `type: glossary`; one `##` section per term; the first line of each
term is `Aliases: a, b, c`. Kiwi compiles terms and aliases into an exact-match
dictionary: an agent searching for an alias finds the term, and the term's other names
are added to its query. Define each term once (`kiwi lint` catches duplicates).

## 6. Access

Access is declared from coarse to fine (see `references/access.md`):

1. `KIWIOWNERS`: `finance/** label=confidential groups=finance` (last match wins). A
   bare name such as `hr` or `hr/` covers a directory of that name at any depth, as in
   CODEOWNERS (`/hr/` only at the root); matching ignores case. `owner=@team-finance` on a line gives its documents a default owner
   (frontmatter `owner:` replaces it); owners never change access.
2. Frontmatter `access: { label: internal, groups: [eng, product] }` replaces the path
   rule for one document.
3. Heading attributes narrow a section: `## Pricing {#pricing access=finance}`; on the
   H1 they narrow the whole document.

Declarations fail closed: frontmatter that is not closed, unknown frontmatter or
attribute keys, attributes that do not end the heading line and access on headings
inside quotes or lists stop the build. Give two headings with the same title but
different access explicit anchors.

Labels are words your policy understands (`public`, `internal`, `confidential`,
`restricted` is a common set). Groups come from `principals.yaml` (or an identity
provider in front of Kiwi). Content no rule covers is `unlabeled` and invisible until
the policy allows it: default deny.

Write `policy.cedar` once, test it with `policy.tests.yaml` (`kiwi policy test` in CI),
and remember that an agent never sees more than the person it acts for: the effective
permission is user ∩ agent grant ∩ session scope.

The policy governs plugins too. A plugin that sends text off the machine (an embedding
API with `embedder = "openai"`, a reranker with `backend = "http"`) is the principal
`plugin:vec` or `plugin:rerank` on the channel `network`, and receives only the atoms
your policy lets it read there. Write that rule on purpose, for example
`permit (principal, action == Kiwi::Action::"read", resource) when { context.agent ==
"plugin" && context.channel == "network" && resource.label == "public" };`.

## 7. Checklist for a new corpus

1. Create the layout above; copy `policy.cedar` and `principals.yaml` from an example
   and adapt labels and groups.
2. Write `KIWIOWNERS` with a catch-all first line (`* label=internal`) and narrower
   rules after it; add `owner=` where a directory has one owner, so its documents need
   no `owner:` of their own.
3. Convert documents: frontmatter, anchors on every heading, first sentences as
   summaries, typed links.
4. `kiwi lint <corpus>` until there are no errors; read the warnings.
5. `kiwi policy test <corpus>`.
6. `kiwi build <corpus>`, then ask a few real questions through
   `kiwi query <corpus> kiwi_search '{"queries":[...]}' --principal <user>` and check
   that each principal sees what they should.
7. Commit; builds read the committed tree, not your working copy (`--worktree` reads
   uncommitted files while drafting).
8. Give the corpus its own `namespace` in `.kiwi/kiwi.toml` (lowercase letters, digits,
   `-`, `_`) if one server will federate it with others (`kiwi serve kb-a kb-b`). Each
   keeps its own policy, principals and audit log; ids become `namespace:id`.
