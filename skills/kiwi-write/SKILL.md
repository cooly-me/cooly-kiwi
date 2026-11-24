---
name: kiwi-write
description: Change a Kiwi knowledge base through kiwi_propose, which opens a branch and a pull request instead of writing to the main branch. Use when the user asks to record a decision, fix or update a document, add a glossary term, deprecate a decision, or fill a knowledge gap in a Kiwi-served corpus.
---

# Writing knowledge through Kiwi

Kiwi never lets an agent write to the main branch. `kiwi_propose` commits your changes
on a new branch (`kiwi/propose/<user>/<id>`) on top of the snapshot you read, opens a
pull request (or a local proposal record), lints the proposed corpus and tells you
whether the policy lets the user merge directly (`merge: "direct"`) or a reviewer must
approve (`merge: "review"`). Proposals are checked against the same permissions as
reads: the user may only change what they can read.

Each change takes one of four forms:

| Form | Use it to |
| --- | --- |
| `{"section": "adr/adr-005-idempotency-keys#decision", "content": "..."}` | rewrite the body of one section; its heading line, anchor and access attributes are kept |
| `{"path": "adr/adr-003-sync-capture.md", "set": {"status": "deprecated"}}` | set frontmatter keys (status, review_by, owner, supersedes...) without touching the rest |
| `{"path": "adr/adr-009-retry-budgets.md", "content": "---\nid: ADR-009..."}` | create a document, or replace a whole file |
| `{"path": "notes/old.md"}` | delete a file (rarely right: prefer `deprecated`) |

Prefer `section` and `set` changes for existing documents: they are small for reviewers
and leave everything else in the file as written. A `section` change replaces that
section's own text, from its heading to the next heading; a short section merged into
its parent is changed by its own id. New files and access changes always need review.
Kiwi answers as if the document held only what the user can see; sections the user
cannot see are put back where they were, and when they conflict with the change the
proposal still opens, for an owner to review.

On a server that federates several knowledge bases, each proposal changes one of them:
set `"namespace": "acme"` on the request. Paths stay relative to that corpus
(`adr/adr-009-retry-budgets.md`); section ids may be given as returned
(`acme:adr/...#decision`) or bare.

## Before writing

1. **Find what exists.** Search (`kiwi_search`) and read the documents you are about to
   change, so your proposal starts from the current text and does not duplicate a
   section that already exists under another name. Note the snapshot.
2. **Read what you will change.** Read the target sections at L2 (`kiwi_read`), and the
   document root with `context: "children"` to see its structure. For a section change
   you only need that section; to add a section to an existing document, rewrite the
   body of the section it should follow and append the new `## Heading {#anchor}` and
   its text at the end of that body. A body, and a whole file, must close every code
   block and HTML block they open.
3. **Decide the kind of change:**
   - a fix inside one section;
   - a new section in an existing document;
   - a new document (new decision, new term, new runbook, new standard);
   - a replacement of a decision (new ADR that `supersedes` the old one, which becomes
     `status: deprecated`).

## Writing atoms

Kiwi splits documents at headings. Each section becomes one atom, so write sections an
agent can use alone:

- One idea per section, 60-500 words. Shorter sections merge into their parent;
  longer ones are split at paragraphs and lose their heading context.
- Give every `##` and `###` heading an explicit anchor: `## Retry policy {#retry-policy}`.
  Anchors are the stable ids other documents and agents cite; never change an existing
  one.
- Make the **first sentence** of every section a self-contained summary. It becomes the
  section's L1 abstract that agents rank and read first.
- Refer to other documents by id in the text ("as ADR-005 decides") or with relative
  links (`[ADR-005](../adr/adr-005-idempotency-keys.md)`); Kiwi turns both into graph
  edges. A link to a missing file fails lint.

## Frontmatter

Every document starts with:

```yaml
---
id: ADR-009                 # unique; used in text and in typed links
type: adr                   # vision | prd | rfc | adr | runbook | standard | glossary
status: draft               # draft | review | approved | deprecated
owner: "@team-platform"     # who answers questions about it
review_by: 2027-06-01       # when it must be checked again
summary: One sentence that says what the document decides or requires.
implements: [PRD-001]       # an RFC implements a PRD
decided_by: [ADR-005]       # documents that rest on an ADR
depends_on: [ADR-002]
supersedes: [ADR-003]       # this replaces an older document
amends: [ADR-004]           # this changes part of an older one, which keeps standing
code: ["services/payment/**"]
tags: [payments]
---
```

New documents proposed by an agent start as `status: draft` unless the user says the
decision is already agreed. Always set `summary` and `review_by`, and `owner` unless
the directory's `KIWIOWNERS` line names one (`kiwi_read` shows a document's owner).

## Common changes

- **Record a decision.** New ADR with sections `Context {#context}`, `Decision
  {#decision}`, `Consequences {#consequences}`, optionally `Alternatives considered
  {#alternatives}`. Link the RFC or PRD it decides with `decided_by` on *their*
  frontmatter, or mention them by id.
- **Replace a decision.** New ADR (a `path` + `content` change) with
  `supersedes: [ADR-003]`, and in the same proposal
  `{"path": "adr/adr-003-sync-capture.md", "set": {"status": "deprecated"}}`. Do not delete the old ADR; agents
  are redirected to the new one and the history stays traceable (`kiwi_trace`).
- **Amend a decision.** When a new ADR changes part of an older one that otherwise
  stands, give the new ADR `amends: [ADR-004]` and, in the same proposal, set
  `{"path": "adr/adr-004-opensearch.md", "set": {"amended_by": ["ADR-010"]}}`; each side
  without the other is a lint warning.
- **Record a rule.** A rule or reference that stays in force (a quality bar, a design
  system, a writing guide, a security guarantee) is a `type: standard` document, not an
  approved RFC: start with `Scope {#scope}`, give each rule or area its own section,
  and end with how to get an exception. Name the decisions it rests on with
  `decided_by`. To change a rule, rewrite its section; the history stays in Git.
- **Add a glossary term.** Add `## Term {#term}` to the glossary document with a first
  line `Aliases: alias one, alias two` and a 2-4 sentence definition. A term or alias
  defined twice fails lint.
- **Fill a knowledge gap.** Write the missing section where a reader would look for it
  (`kiwi_map` the area first), then mention the gap's question in the rationale.
- **Restrict a section.** Add heading attributes: `## Pricing {#pricing access=finance}`
  (confidential to the `finance` group) or `{#x label=restricted groups=exec}`. Section
  access can only narrow the document's access. Directory-wide access lives in
  `KIWIOWNERS`, which agents cannot change through proposals.

## Calling kiwi_propose

```json
{
  "changes": [
    {"path": "adr/adr-009-retry-budgets.md", "content": "---\nid: ADR-009\ntype: adr\n..."},
    {"path": "adr/adr-003-sync-capture.md", "set": {"status": "deprecated"}},
    {"section": "rfcs/rfc-010-checkout-saga#timeouts", "content": "Each step has a 2-second timeout..."}
  ],
  "rationale": "Agents asked three times how retries are bounded (knowledge gap). This records the decision from the platform review.",
  "title": "docs(adr): ADR-009 retry budgets"
}
```

- Paths are relative to the corpus root and end in `.md`; `.kiwi/` and `KIWIOWNERS`
  cannot be changed through proposals.
- `set` values are JSON: strings, numbers, booleans or lists (`{"tags": ["payments"]}`).
- The rationale is what reviewers read: say what changed, why, and the source of the
  facts (a meeting, an incident, a gap).

## After proposing

- Report the `link` and `branch` to the user, and whether review is needed.
- Fix every lint finding of severity `error` (broken link, broken reference, duplicate
  term) with a new proposal; mention warnings.
- Do not tell the user the knowledge base changed: it changes when the pull request is
  merged and the next build compiles the new commit.
