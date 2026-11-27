---
name: kiwi-answer
description: Answer questions and do research from a Kiwi knowledge base through its MCP tools (kiwi_map, kiwi_search, kiwi_read, kiwi_trace, kiwi_feedback), with every fact cited to a snapshot. Use when the kiwi_* tools are connected and the user asks about requirements, designs, decisions (PRD, RFC, ADR), standards, terms, runbooks or "why" something is the way it is, or needs a long piece of work grounded in that knowledge.
---

# Answering from Kiwi

Kiwi serves knowledge compiled from Markdown in Git. Every section of every document is
an **atom** with a stable id (`path#anchor`; the bare `path` is the document) at three
zoom levels: **L0** (title), **L1** (a one-sentence abstract), **L2** (the full text).
Every answer is pinned to a **snapshot** (a commit), and Kiwi has already removed what
the user may not see.

**Kiwi is the only source.** Take facts from Kiwi's answers, never from a clone, a copy
of the files or memory: only Kiwi's answers are governed and citable. If Kiwi cannot
answer, say so; do not read the files instead.

## Quick answer

1. **Search once, with several queries.** Write 2-5 short queries yourself (key nouns,
   synonyms, any id you know, a one-line hypothetical answer) and pass them in one
   `kiwi_search`. Each query gets its own results, so variants cost nothing in depth.
   Narrow with `filters` when the question is structural (`{"type": ["adr"]}`,
   `{"path": "product/"}`): a rule lives in a `standard`, one decision in an `adr`, a
   design in an `rfc`, what to build in a `prd`, how to operate in a `runbook`.
2. **Read what answers.** `kiwi_read` the ids that matter. A section comes with its
   subsections; `level: "L1"` checks relevance cheaply.
3. **Why questions:** `kiwi_trace` from the atom (`depth: 1`, then 2) walks
   implements, decided_by, supersedes, amends and code links; read the nodes you need.

## Find every mention

Ranked search answers what a text is about and stops at `limit`. To list every place an
exact string appears (a name, a code, a phrase to quote or rename), pass `literal`
instead of `queries`: `{"literal": "cap_reached", "ignore_case": true}`, with `filters`
to narrow it. Each match is `{id, line, text}`, `line` counted in the section as
`kiwi_read` gives it. When the answer has `next`, call `kiwi_search` with it, until no
`next` comes back: then the list is complete. Never grep a copy of the files for this.

## Deep research (long reports, posts, designs)

1. **Map.** `kiwi_map` the areas involved (`{"scope": "engineering/"}`, or no scope for
   everything) to see what exists. Raise `budget` if `omitted` is large.
2. **Search** each sub-question as above; a batch of up to eight queries is one call.
3. **Read whole documents.** `kiwi_read` the document ids (or sections) you will rely
   on; each comes whole, with all its sections, up to the read budget. Pass a larger
   `budget` (up to the maximum in the tool description) when you need several long
   documents in one call.
4. **Read on when cut.** An answer with `more` was cut by its budget: `more.left_out`
   names what is missing and `more.next` is the exact argument object of the
   `kiwi_read` that continues. Call it until no `more` comes back, or read only the
   `left_out` ids you need. An answer without `more` is complete.

Budgets are parameters, not limits on what you may know: when an answer is too small,
call again with a larger `budget` (or `limit` for search) rather than going elsewhere.

## Cite

Cite every atom you use as `kiwi://atom/{id}@{snapshot}`: a read's `cite` field, or a
section id from its `sections` list with the answer's `snapshot` (on a federated server,
ids are `namespace:id` and each corpus's snapshot is in `snapshots`). State answers in
your own words; quote sparingly. If atoms disagree, say so and prefer the approved and
newer one.

## Warnings and gaps

- `review_overdue`: may be stale; say so. `unapproved`: a draft, not a decision.
  `deprecated` / `redirected`: prefer the replacement.
- `not_found`: the id does not exist or the user may not see it; do not guess which.
  `hidden` / `redacted` (stub mode): the information exists; name the owner to ask and
  never probe for it.
- `degraded`: a retrieval tier was down; add keyword and id queries.
- After answering, call `kiwi_feedback` once: `useful` with the ids that answered,
  `wrong` with misleading ones (say why), `missing` when the knowledge base lacks the
  answer, which files a gap for the writers.

## On errors

Kiwi answers each call on its own: there is no session to expire. If a call fails
(a network error, a 5xx, a timeout), call it again once. If it fails again, report
the failure to the user and stop that line of work; do not fall back to reading files.
An error that names your arguments (an unknown id, a bad `after`) is yours to fix:
copy ids exactly from responses, and `more.next` as given.
