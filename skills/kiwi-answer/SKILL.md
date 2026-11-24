---
name: kiwi-answer
description: Answer questions from a Kiwi knowledge base through its MCP tools (kiwi_map, kiwi_search, kiwi_read, kiwi_trace, kiwi_feedback) with few tokens and exact citations. Use when the kiwi_* tools are connected and the user asks about product requirements, designs, decisions (PRD, RFC, ADR), terms, runbooks or "why" something is the way it is.
---

# Answering from Kiwi

Kiwi serves knowledge compiled from Markdown in Git. Every section of every document is
an **atom** with a stable id (`path#anchor`, or the bare `path` for a document's
introduction) and three zoom levels: **L0** (title and breadcrumb, ~10 tokens), **L1**
(a one-sentence abstract, ~40-60 tokens) and **L2** (the full text). Every answer is
pinned to a **snapshot** (a commit). Kiwi has already removed everything the user may
not see; never try to work around that.

A server may federate several knowledge bases. Its ids then carry a **namespace**
(`acme:adr/adr-005-idempotency-keys#decision`): copy ids exactly as responses give
them, pass `filters.namespace` to search only some corpora, and expect `kiwi_map`
without a scope to list one node per corpus you can read (`acme:`). A bare id works
only when it names an atom in a single corpus; otherwise you are asked to qualify it.

The principle: look at the map first, open only what you need, and spend tokens on
reading, not on searching.

## The loop

1. **Name what you are looking for.** Before calling anything, write down 2-4 short
   queries yourself: the key nouns, one or two synonyms, and any id you know
   (`ADR-005`, `RFC-010`, `product/checkout/prd`). If you can, add a one-line
   hypothetical answer ("idempotency keys stored for 24 hours replay the response").
   Kiwi does not rewrite queries for you; your variants are the query expansion.

2. **Search.** Call `kiwi_search` with all variants in one call:

   ```json
   {"queries": ["double charge prevention", "idempotency key retry", "ADR-005"]}
   ```

   - The default budget is 800 tokens. High-ranked results come with an L1 `summary`,
     lower ones with only the L0 `title`. Raise `budget` only when the first page is
     clearly relevant but incomplete; prefer a second, sharper search.
   - Narrow with `filters` when the question is structural: `{"type": ["adr"],
     "status": ["approved"]}`, `{"owner": ["@team-payments"]}`, `{"path":
     "product/"}`. Filters with no queries list matching atoms.
   - Pick the type from the kind of question: a rule that must be followed (quality
     bar, design system, writing guide, security guarantee, architecture rule) lives
     in a `standard`; one decision and its reasons in an `adr`; a design in an `rfc`;
     what to build in a `prd`; how to operate something in a `runbook`.
   - `why` tells you how each result matched: `exact` (an id, title or heading you
     named), `term` (a glossary term or alias), `scoped` (text inside a document you
     named), `bm25`, `vec`, `graph` (one typed hop from an exact match), `rerank`
     (scored by the server's cross-encoder), `redirect`/`successor` (a deprecated atom
     replaced by its successor).

3. **Orient when the search is not enough.** If you do not know where something lives,
   call `kiwi_map` on a scope (`{"scope": "rfcs/", "depth": 2}`) and pick sections
   from the titles and abstracts. `omitted` tells you how much more exists.

4. **Read at the right zoom.** Call `kiwi_read` with the 1-3 ids you actually need.
   - `level: "L1"` to confirm relevance cheaply; `L2` (default) to quote or reason.
   - `context: "children"` lists the subsections of a document root;
     `"siblings"` the neighbouring sections; `"parent"` the enclosing one.
   - Reading a document id (`ADR-005`) gives its introduction only. Its decision is a
     separate atom (`adr/adr-005-idempotency-keys#decision`): read that.
   - `links` shows typed edges (`implements`, `decided_by`, `supersedes`,
     `depends_on`, `mentions`, `code:`); `~rel` marks incoming ones.

5. **Explain "why" with the graph.** For questions about reasons, lineage or impact,
   call `kiwi_trace`:
   - `{"id": "PRD-001", "depth": 2}` walks the declared relations (implements,
     decided_by, depends_on, supersedes, code): PRD -> RFC -> ADR -> code paths.
   - `{"id": "ADR-007", "relation": "supersedes", "direction": "out"}` finds what a
     decision replaced; `"direction": "in"` finds what replaced it.
   - Start with `depth: 1` and go to 2 only when needed; `relation: "any"` also follows
     text mentions, which multiplies the result.
   Trace returns L0 titles only; read the nodes you need.

6. **Answer and cite.** State the answer in your own words and cite every atom you
   used as `kiwi://atom/{id}@{snapshot}` (the `cite` field of `kiwi_read`, or the
   `snapshot` of any response; on a federated server, the namespace's entry in
   `snapshots`). Quote sparingly. If atoms disagree, say so and prefer
   the approved and newer one.

7. **Record feedback.** After answering, call `kiwi_feedback` once:
   - `useful` with the ids that answered the question;
   - `wrong` with ids that were misleading (say why in `note`);
   - `missing` when the knowledge base does not contain the answer. This files a
     knowledge gap for the writers; it is the most valuable feedback you can give. On a
     federated server, add `namespace` to say which corpus should hold it.

## Reading the envelope

Every response carries `snapshot` (on a federated server also `snapshots`, one per
corpus you can read), and may carry:

- `warnings`:
  - `review_overdue`: the document is past its `review_by` date. Use it, but tell the
    user it may be stale.
  - `unapproved`: a `draft` or `review` document. Treat it as a proposal, not a
    decision.
  - `deprecated` / `redirected`: the atom was replaced. Prefer the replacement; mention
    the old decision only for history.
- `redacted` (only when the server runs with stub mode): how many matches were hidden
  from the user. Hidden sections appear in `hidden` with an owner to ask. Never guess
  their content and never try other queries to reveal them; tell the user the
  information exists and who can grant access.
- `not_found` in `kiwi_read`: the id does not exist *or* the user may not see it. Do
  not speculate which.
- `degraded` in `kiwi_search`: one retrieval tier (usually vectors) was unavailable,
  so results come from the others. Add keyword and id queries, or search again later.

## Budget rules of thumb

| Step | Typical cost |
| --- | --- |
| One `kiwi_search` (default budget) | 400-800 tokens |
| `kiwi_map` of one directory, depth 2 | 300-1,500 tokens |
| `kiwi_read` of one section at L2 | 150-800 tokens |
| `kiwi_trace`, depth 1 / depth 2 | 200-500 / 500-1,200 tokens |

A good answer usually costs one search, one or two reads and, for "why" questions, one
trace. If you have spent three searches without a relevant hit, stop, answer with what
you have, and record `missing`.

## Do not

- Do not read whole documents section by section "to be safe"; read what answers.
- Do not paste large L2 texts back to the user; summarise and cite.
- Do not invent ids. Ids come from responses.
- Do not use `kiwi_propose` to answer questions; it writes (see the kiwi-write skill).
