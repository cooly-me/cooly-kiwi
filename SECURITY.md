# Security policy

Kiwi enforces permissions on knowledge, so leaks are security bugs. That
includes any way for a caller to learn something about content it may not read:
returned ids, titles, text or scores, ranking changes, counts, timing that
depends on hidden content, error messages that distinguish hidden from absent atoms,
or audit-log tampering that verification does not catch.

## Supported versions

Only the latest commit on `main` is supported before 1.0.

## Reporting a vulnerability

Report privately to the maintainers through GitHub's private vulnerability reporting
on this repository (Security → Report a vulnerability). Please do not open a public
issue for a vulnerability.

Include the commit, the corpus or a minimal reproduction (a few Markdown files, the
`KIWIOWNERS`, `policy.cedar` and `principals.yaml`), the principal and the tool calls
that show the problem. We aim to acknowledge a report within three working days and to
agree on a disclosure date with you once a fix is ready.

## What Kiwi does and does not protect

- Kiwi filters every query to the caller's permitted atoms before ranking. BM25 reads
  only the posting partitions of the caller's permission classes and computes its
  statistics from them (`bm25_stats = "strict"`, the default), so hidden content
  changes neither scores nor the postings a query reads. The leak and non-interference
  tests in `crates/kiwi-cli/tests/leak.rs` (one corpus) and `federation.rs` (two) are
  the contract.
- A federated server treats a corpus the caller can see nothing of exactly like a
  namespace that does not exist: it is not listed, adds nothing to answers, and its ids
  are reported as absent.
- Plugins that send text off the machine (an embeddings API, an HTTP reranker) receive
  only the atoms the policy lets `plugin:<name>` read on the `network` channel; a build
  with such a plugin refuses to run without the corpus policy. Their endpoints must be
  `https` (or `http` on loopback), their secrets come from environment variables, and
  they follow no redirects. Query text you send to Kiwi is sent to the embeddings API
  when vectors come from one.
- The audit log and its sinks hold, for each call: sequence, time, who, which agent and
  channel, which tool, corpus and snapshot, the status (`ok`, `refused`, `failed` and
  its reference), the stable ids and document paths of the atoms returned (or named by a
  feedback, or changed by a proposal) with their pack uids, how many items and how many
  withheld matches, the response's tokens, the budget, the server time, and the chain's
  `prev` and `hash`; never content. Ids and documents are only ever of atoms the caller
  can see (the leak suite checks every record), and withheld matches only a count. What
  the caller asked for (queries, read ids, scopes, the paths and sections a proposal
  names) is written, to the log and every sink, only when the corpus sets
  `audit.record_requests = true`: turn it on knowing that query text can itself be
  personal or confidential, and that it then leaves the machine with every sink.
  Contents, rationales, filters and feedback notes are never recorded. A record's
  `note`, which can name content hidden from the caller (why a proposal was refused, its
  conflicts with hidden documents), stays in the local log: sinks receive only a keyed
  BLAKE3 of it (`note_mac`), under a key kept beside the local log (refused when others
  may read it), so a receiver cannot test a guess of the note; quarantine records carry
  a keyed hash of the torn bytes' hash, not the hash or its size. A sink that is down
  drops records after its queue fills; the local log stays complete.
- Over stdio, the caller is a trusted local user: the identity comes from the command
  line or the Git e-mail, asserted, not proven. Anyone who can run `kiwi serve` locally
  can choose its `--principal` and can read the corpus's files; stdio governance keeps
  an agent within its user's view and is for a person's own machine, not a shared
  service. Policies should not trust the `stdio` channel more than `http`.
- Agents are an allow-list: one `principals.yaml` does not list, or a token naming no
  client, reads only what `default_agent` grants (nothing by default).
- Over HTTP, Kiwi is an OAuth 2.1 resource server: it validates bearer tokens (HS256
  secret, RSA / EC public keys or a JWKS file; signature, issuer, audience, expiry and
  not-before), requires a `kiwi:` scope, and does not issue tokens. Run it behind TLS.
- Internal errors reach MCP clients as a reference only; details (paths, tool output)
  go to the server's standard error. A federated corpus the caller cannot see never
  makes a call fail.
- A pinned citation (`kiwi://atom/{id}@{rev}`) needs the atom to be readable now and
  under the access it had then (by the current policy), and none of its paragraphs to
  be held now only by content hidden from the caller. Only snapshots of the served
  source's lineage are served (a commit's ancestors, never another branch), named in
  full or by at least seven hex digits.
- Every command that answers serves only the pack built from the snapshot its policy
  was read from (`--rev`, `HEAD`, or the files of a directory as they are); without
  that pack it refuses.
- The audit chain detects edits, reordering and deletions inside the log. Truncation of
  the tail is only detected against a saved anchor (`kiwi audit verify --anchor`). A
  line a crash left half written is moved aside and recorded as a break in the chain,
  which `kiwi audit verify` reports. `kiwi audit export` writes nothing from a log that
  does not verify, and exports records exactly as sinks receive them (notes as their
  keyed MACs); it needs the key file only for records that carry a note. The local
  `note` of a refused or failed proposal may quote the paths the caller named.
- A corpus configuration may read secrets only from the environment variables the
  operator lists in `KIWI_SECRET_ENV`, and never the HS256 token secret, which is
  reserved before any corpus opens.
- Everything `kiwi_propose` tells the caller (accepted or refused, messages, compile
  errors, lint, `merge`) is decided on the corpus stripped of what the caller cannot
  see, every change before any real file is built. Hidden sections are then put back
  by their own spans; a conflict with them never changes the answer, it marks the
  proposal for an owner's review (the pull request says so without naming hidden
  content; the audit note has the details). A file that would replace a hidden file is
  left out of the commit, and a commit that would change any path its plan does not
  name is refused.
- `owner_review` is advisory unless the repository enforces it: on GitHub the pull
  request is created as a draft labelled `kiwi:owner-review`, with the owners' reviews
  requested after, and branch protection must keep such a pull request from merging
  until an owner approves (see docs/configuration.md, "Proposals"). Where drafts are
  not available (private repositories on GitHub's free plan) the label and branch
  protection are the only guard.
- A proposal's file that would replace any entry of the base tree the author cannot
  see (a hidden document or directory, a non-Markdown file, a symbolic link, a
  submodule) is left out of the commit; paths are compared after Unicode NFC
  normalisation and case folding.
- `--insecure-principal` disables HTTP authentication for local development, only
  binds to loopback addresses, and acts on the `insecure` channel, never `http`.
- `kiwi gaps` shows a writer a query only if the writer may read everything its asker
  could read when asking.
