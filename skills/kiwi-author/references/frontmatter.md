# Frontmatter reference

Every key is optional except where noted. Any other key is an error, so a misspelt
`access` can never leave a document under a broader rule.

| Key | Type | Meaning |
| --- | --- | --- |
| `id` | string | Document id, unique in the corpus (`RFC-042`). Agents and other documents refer to it. Recommended. |
| `type` | string | `vision`, `prd`, `rfc`, `adr`, `runbook`, `standard`, `glossary` or your own type (folded to lowercase). Default `doc`. Filterable (`filters.type`). |
| `status` | string | `draft` (default), `review`, `approved` or `deprecated`. Anything else fails the build. |
| `owner` | string | Team or person (`"@team-payments"`). Replaces the `owner=` of the `KIWIOWNERS` rule; a document with neither is a lint warning. Filterable. |
| `review_by` | date | `YYYY-MM-DD`. Past this date (relative to the snapshot's date) agents get a `review_overdue` warning. |
| `summary` | string | One sentence; the document root's L1. Missing summaries are a lint warning. |
| `title` | string | Overrides the first H1 as the document title. |
| `implements` | list of ids | Requirements this document implements (RFC -> PRD). |
| `decided_by` | list of ids | Decisions this document rests on. |
| `depends_on` | list of ids | Documents this one depends on. |
| `supersedes` | list of ids | Documents this one replaces; mark those `deprecated`. |
| `amends` | list of ids | Documents this one changes in part without replacing them (an ADR amending an earlier ADR). The amended document lists this one in `amended_by`. |
| `amended_by` | list of ids | Later documents that amend this one; each must list this one in `amends`. A side declared without the other is the lint warning `amendment-unmatched`. |
| `code` | list of globs | Code paths the document governs (`services/payment/**`). |
| `tags` | list | Free tags. Filterable (`filters.tags`). |
| `access` | map or string | `{ label: internal, groups: [eng] }`, or a group name as shorthand for `{ label: confidential, groups: [<name>] }`. Replaces the `KIWIOWNERS` rule for this document. |

Lists accept a single value too (`implements: PRD-001`).
