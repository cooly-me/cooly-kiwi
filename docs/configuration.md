# Configuration reference

Everything a corpus and a server are configured with: `.kiwi/kiwi.toml`,
`.kiwi/principals.yaml`, `KIWIOWNERS`, the environment and the command line. Every file
is read from the same snapshot as the documents it governs (a commit, or the working
files), and every unknown key is an error. [design.md](design.md) explains why each
rule is what it is.

## Files of a corpus

| Path | Holds |
| --- | --- |
| `.kiwi/kiwi.toml` | the configuration below (every key optional) |
| `.kiwi/policy.cedar` | the Cedar policy (`governance.policy`) |
| `.kiwi/policy.tests.yaml` | its unit tests, run by `kiwi policy test` (`governance.policy_tests`) |
| `.kiwi/principals.yaml` | users, groups and agents (`governance.principals`) |
| `KIWIOWNERS` | access and default owner by path |
| `.kiwi/packs/` | built packs, `CURRENT` and `index.json` |
| `.kiwi/state/` | `audit.log`, its note key `audit.log.key` (mode 600; a looser key stops the server), `feedback.jsonl`, proposals |

Kiwi creates `.kiwi/packs/` and `.kiwi/state/` with a `.gitignore` of `*` (unless one is
there), so packs, logs and keys never reach the corpus's repository and no file of the
corpus is changed.

Section bodies are read with LF line endings whatever the file uses, so texts, hashes
and paragraph checks are the same for LF and CRLF files.

## `.kiwi/kiwi.toml`

| Key | Default | Meaning |
| --- | --- | --- |
| `namespace` | `default` | The corpus's name; ids carry it (`ns:id`) when a server federates several corpora. 1-63 lowercase letters, digits, `-` or `_`; `code` is reserved. |
| `atoms.min_tokens` | `50` | Sections below this merge into their parent (same access only). |
| `atoms.max_tokens` | `800` | Sections above this split at paragraphs. |
| `governance.hidden` | `invisible` | `invisible`: a hidden id answers like an absent one. `stub`: answers carry a `redacted` count and stubs naming the owner to ask. |
| `governance.bm25_stats` | `strict` | `strict`: BM25 statistics over the caller's view only. `global`: over the whole corpus (hidden terms can shift scores). |
| `governance.policy` | `.kiwi/policy.cedar` | The policy file. |
| `governance.policy_tests` | `.kiwi/policy.tests.yaml` | The policy tests. |
| `governance.principals` | `.kiwi/principals.yaml` | The principal directory. |
| `search.budget` | `800` | Token budget of an answer when the request names none (50-32,000). |
| `search.limit` | `10` | Results considered before the budget cut when the request names none (at most 50). |
| `packs.keep` | `10` | Packs kept after each build, the current one included; older pinned citations stop resolving once their pack goes. |
| `forge.kind` | `local` | `local`: a branch and a proposal record. `github`: push the branch and open a pull request with `gh`. |
| `forge.remote` | `origin` | The remote `github` pushes to. |
| `forge.base` | `main` | The base branch of pull requests. |
| `plugins.bm25.enabled` | `true` | Build and use the BM25 segment. |
| `plugins.vec.enabled` | `false` | Build and use the vector segment. |
| `plugins.vec.embedder` | `fastembed` | `fastembed` (a local ONNX model), `openai` (an OpenAI-compatible API) or `hash` (no model). |
| `plugins.vec.model` | `multilingual-e5-small` with `fastembed` | Local: `multilingual-e5-small`, `bge-small-en-v1.5`, `all-minilm-l6-v2`. Required with `openai`. |
| `plugins.vec.url` | none | `openai`: the API base URL (https, or http on loopback). |
| `plugins.vec.api_key_env` | none | `openai`: the variable holding the key; it must be listed in `KIWI_SECRET_ENV`. |
| `plugins.vec.dimensions` | none | `openai`: dimensions to ask for (vectors are kept at 384). |
| `plugins.vec.batch_size` | `64` | `openai`: texts per request. |
| `plugins.vec.timeout_ms` | `30000` | `openai`: limit for one request. |
| `plugins.vec.retries` | `4` | `openai`: retries of a failed request. |
| `plugins.vec.query_prefix`, `passage_prefix` | empty | `openai`: prefixes (`query: ` / `passage: ` for E5 models). |
| `plugins.rerank.enabled` | `false` | Rerank the top of every search. |
| `plugins.rerank.backend` | `local` | `local` (an ONNX cross-encoder) or `http`. |
| `plugins.rerank.model` | `jina-reranker-v2-base-multilingual` locally | Local: also `bge-reranker-base`, `bge-reranker-v2-m3`, `jina-reranker-v1-turbo-en`; `http`: the model name sent with `cohere`. |
| `plugins.rerank.url` | none | `http`: the endpoint. |
| `plugins.rerank.format` | `cohere` | `http`: `cohere` (Cohere, Jina, Voyage, vLLM, Infinity) or `tei`. |
| `plugins.rerank.api_key_env` | none | `http`: the variable holding a bearer token; listed in `KIWI_SECRET_ENV`. |
| `plugins.rerank.top_k` | 10 local, 20 `http` | Results reranked from the top. |
| `plugins.rerank.timeout_ms` | `2000` | Limit for one rerank; past it the fused order stands. |
| `plugins.wasm.components` | none | `[{ name = "…", path = "…" }]`: WebAssembly plugins, governed as `plugin:<name>`. |
| `audit.sinks` | none | `[[audit.sinks]]` tables, below. |

`[[audit.sinks]]`: copies of every audit record (a record's `note` is replaced by
`note_mac`, a keyed BLAKE3 under `audit.log.key`; a quarantine record's hash and length
of the torn bytes by `quarantined.mac`).

| Key | Default | Meaning |
| --- | --- | --- |
| `kind` | required | `stdout`, `stderr`, `http` or `syslog` (`stdout` is refused over stdio). |
| `url` | none | `http`: the collector (https, or http on loopback). |
| `bearer_env` | none | `http`: the variable holding a bearer token; listed in `KIWI_SECRET_ENV`. |
| `batch` | `100` | `http`: records per request (1-10,000). |
| `flush_ms` | `1000` | `http`: the longest a record waits for its batch (10-600,000). |
| `timeout_ms` | `5000` | `http`: limit for one request (100-600,000). |
| `retries` | `5` | `http`: retries of a failed batch (at most 20). |
| `queue` | `10000` | Records held while the sink is slow or down, then dropped and counted (1-1,000,000). |
| `address` | none | `syslog`: `udp://host:port` or `unix:///dev/log`. |
| `facility` | `local0` | `syslog`: `local0`-`local7`, `user` or `auth`. |

## `.kiwi/principals.yaml`

```yaml
users:
  - id: alice                   # the Cedar principal Kiwi::User::"alice"
    name: Alice (engineering)   # display name (commit author of her proposals)
    groups: [eng, staff]        # each a Cedar parent Kiwi::Group::"<group>"
    emails: [alice@acme.test]   # matched against `git config user.email` over stdio
agents:                         # an allow-list of agents (MCP clients)
  - id: claude-code             # no `labels`: no limit beyond the user's own
  - id: summarizer
    labels: [public, internal]  # the most this agent reads for anyone
default_agent:
  labels: []                    # an unlisted agent, or a token naming no client
```

`(unidentified)` is reserved: it is the agent of a token that names no client, and
always gets `default_agent`.

## `KIWIOWNERS`

One rule per line: a path pattern, then declarations. `#` starts a comment.

```text
# pattern          declarations
*                  label=internal  owner=@team-docs
/vision.md         label=public
finance/**         label=confidential groups=finance  owner=cfo@example.com
legal/             access=legal
product/           owner=@team-product
```

- **Patterns** follow CODEOWNERS and match ignoring case. A slash at the start or in
  the middle anchors a pattern to the root (`/hr`, `finance/**`, `docs/hr`); otherwise
  it matches at any depth. A name covers a file of that name and everything under a
  directory of that name; a trailing `/` (`hr/`) covers only directories.
- **Access:** `label=<label>`, `groups=<a,b>` (needs a label), or `access=<a,b>` for
  `label=confidential groups=<a,b>`. A group list must name at least one group.
- **Owner:** `owner=<one owner>`, the default owner of the documents the line covers;
  frontmatter `owner:` replaces it. A comma list is an error.
- A line declares access, an owner or both. Access and owner each come from the last
  matching line that declares them; an owner never changes access. Content no line
  covers is `unlabeled`.
- Any other key, a key given twice, or a line with neither is an error.

Frontmatter `access:` replaces the path rule for one document (an empty `access:` is
an error); heading attributes (`{#anchor access=finance}`) narrow one section.

## Environment

| Variable | Meaning |
| --- | --- |
| `KIWI_PRINCIPAL` | `--principal` of `kiwi serve` over stdio. |
| `KIWI_AGENT` | `--agent` of `kiwi serve`. |
| `KIWI_SECRET_ENV` | Comma-separated variables a corpus configuration may name for a secret (`api_key_env`, `bearer_env`). No other variable is ever read for one, and the variable `--auth-hs256-env` names is refused even when listed. |
| `KIWI_CACHE` | Where local models are cached (default `~/.cache/kiwi`). |
| `HTTPS_PROXY`, `NO_PROXY` | Honoured by outbound HTTP, except to loopback endpoints. |

## Command line

Every command that reads a corpus takes the corpus directory and `--worktree` (the
working files), `--rev <rev>` (a Git revision; default `HEAD` in a repository) and
`--packs <dir>` (default `<corpus>/.kiwi/packs`). `serve`, `query`, `export` and `gaps`
answer from the pack built from the very snapshot their policy is read from, and
refuse when there is none.

| Command | Flags |
| --- | --- |
| `kiwi build <corpus>` | `--embedder <name>` (`hash` needs no model), `--no-bm25`, `--no-vec`, `--as-of <YYYY-MM-DD>`, `--json` |
| `kiwi serve <corpus>...` | several corpora federate by namespace (`--rev` and `--packs` with one only); `--embedder`, `--no-bm25`, `--no-vec`; `--principal` (stdio), `--agent` (default `local-agent`); `--state <dir>`; `--no-audit`; `--watch` (with `--worktree` rebuilds the working files when one changes; otherwise rebuilds `HEAD` when it or a branch moves, so commits stay what is served; not with `--rev`) |
| `kiwi serve` over HTTP | `--http <addr>`; one of `--auth-hs256-env <VAR>`, `--auth-rs256-pem <file>`, `--auth-jwks <file>` (key files are re-read when their content changes; each key is taken on its own; an empty set revokes all keys; a missing or broken file keeps the keys and is reported), or `--insecure-principal <user>` (loopback only, the `insecure` channel); `--issuer <url>` (required), `--resource <url>` (the token audience; default `http://<addr>/mcp`), `--allowed-host <host>` (repeatable; default localhost), `--max-sessions <n>` (1,024) |
| `kiwi query <corpus> <tool> [json]` | `--principal` (default `anonymous`), `--agent` (default `cli`) |
| `kiwi lint <corpus>` | `--json`; exit code 1 on errors |
| `kiwi policy test <corpus>` | runs `.kiwi/policy.tests.yaml` |
| `kiwi gaps <corpus>` | `--principal <writer>`, `--state <dir>`, `--limit <n>` (100); shows the queries of askers whose view when they asked the writer may read entirely now |
| `kiwi export <corpus>` | `--format llms-txt` or `jsonl`, `--principal`, `--agent` (default `cli`) |
| `kiwi gc <corpus>` | `--keep <n>` (default `packs.keep`) |
| `kiwi audit verify <file>` | `--anchor <hash>` (the expected last hash, to catch truncation) |
| `kiwi bench [corpus]` | `--questions`, `--principals`, `--embedder`, `--repeat`, `--synthetic <atoms>`, `--keep-synthetic <dir>`, `--rerank <model>`, `--rerank-top-k <n>`, `--json` |
| `kiwi info` | the version and compiled-in plugins |

## Proposals

`kiwi_propose` needs a server of a commit (not `--worktree`, not a `--watch` of the
working files). Its paths are Markdown files outside `.kiwi/`, never the root
`README.md`, never through `KIWIOWNERS`, never through a file or onto a directory.

A proposal that conflicts with content its author cannot see opens as answered, with
`owner_review: true` in its record (`.kiwi/state/proposals/<id>.json`) and a notice in
its commit and pull request. With `forge.kind = "github"` the pull request is created
as a draft labelled `kiwi:owner-review` (the label is created first if missing) in one
`gh pr create` call; the owners' reviews are then requested one by one with
`gh pr edit --add-reviewer`. After a failed create Kiwi looks up the branch's pull
request (`gh pr view`) before retrying, so it never opens two. **Where drafts are not
available** (private repositories on GitHub's free plan), the pull request is created
plain and then labelled: owner review then rests on the label and branch protection
alone, so the protection below is required, not optional. For review requests, an
owner written `@user` or `@org/team` (frontmatter `owner:` of the file in conflict,
else its `KIWIOWNERS` `owner=`) is that GitHub user or team; an e-mail owner cannot be
requested and is listed in the record's `owner_review_unapplied`, with any step `gh`
failed. These steps are best effort and never change the answer, so the repository must
enforce them: protect the base branch so that a pull request labelled
`kiwi:owner-review` cannot merge (a required status check that fails while the label is
present, or a ruleset on it) until an owner approves, with CODEOWNERS review required
for the paths those owners hold.

## Over HTTP

The server publishes its protected-resource metadata at
`/.well-known/oauth-protected-resource` followed by the resource's path
(`/.well-known/oauth-protected-resource/mcp` for `/mcp`), and its `401` and `403`
challenges name that URL. Tokens need `kiwi:read` or `kiwi:label:<label>` scopes; `sub`
is the user and `client_id` (or `azp`) the agent.

## Pinned citations

`kiwi://atom/{id}@{rev}` names a snapshot in full or by at least seven hex digits (after
`dir:` for a directory snapshot). A server of a commit serves the snapshots of that
commit's ancestors only, never another branch or a directory snapshot; a server of a
directory serves directory snapshots only.
