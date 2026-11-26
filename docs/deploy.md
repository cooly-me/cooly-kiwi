# Deploy for a team

This guide runs Kiwi on one Linux server for a team: a knowledge base kept in a GitHub
repository, rebuilt when its main branch moves, served over HTTP to many agents with
signed tokens, writing proposals back as pull requests, auditing every call, and
federated with a second, generated corpus. Nothing restarts when content changes.
Every flag and key named here is in [configuration.md](configuration.md).

The examples use the user `kiwi`, the clone `/srv/kb`, and `/var/lib/kiwi` for packs,
state and keys; adjust them to your system.

## 1. Create the knowledge base

Create a repository with the corpus layout, `.kiwi/kiwi.toml`, `.kiwi/policy.cedar`,
`.kiwi/principals.yaml` and `KIWIOWNERS`, as the
[`kiwi-author`](../skills/kiwi-author/SKILL.md) skill describes, and give it a
`namespace` (here `kb`). Check it before the first push:

```sh
kiwi lint . && kiwi policy test . && kiwi build . --no-vec
```

Users in `principals.yaml` are the `sub` of the tokens in step 5, and agents are their
`azp` (or `client_id`).

## 2. Clone it on the server

Kiwi needs a Git clone, not an export: it serves commits, reads each commit's policy
from the repository, and `kiwi_propose` commits on top of the served commit. A server
of the working files (`--worktree`) takes no proposals.

The user authenticates to GitHub through `gh`, which also serves Git as its credential
helper, so no token ever appears in a URL, a command line or the clone's configuration
(step 4 says which token):

```sh
sudo useradd --system --home-dir /var/lib/kiwi --create-home --shell /usr/sbin/nologin kiwi
sudo install -d -o kiwi -g kiwi /srv/kb /var/lib/kiwi/packs /var/lib/kiwi/state /var/lib/kiwi/gh
as_kiwi() { sudo -u kiwi env HOME=/var/lib/kiwi GH_CONFIG_DIR=/var/lib/kiwi/gh "$@"; }
sudo cat /root/github-token | as_kiwi gh auth login --with-token     # the token, kept by root
as_kiwi gh auth setup-git
as_kiwi git clone --branch main https://github.com/acme/kb.git /srv/kb
as_kiwi /opt/kiwi/kiwi build /srv/kb --packs /var/lib/kiwi/packs/kb --no-vec
```

Install the `kiwi` binary at a fixed path such as `/opt/kiwi/kiwi`; step 8 covers
upgrades.

## 3. Keep it current

A timer fetches main and builds it; the server follows the store with `--follow` and
swaps in each new pack with the policy of its commit, as soon as the build makes it
current. Calls in flight finish on the pack they started with; nothing restarts.

`/etc/systemd/system/kiwi-refresh.service`:

```ini
[Unit]
Description=Fetch and build the knowledge base

[Service]
Type=oneshot
User=kiwi
Environment=HOME=/var/lib/kiwi GH_CONFIG_DIR=/var/lib/kiwi/gh
ExecStart=/usr/bin/git -C /srv/kb pull --quiet --ff-only origin main
ExecStart=/opt/kiwi/kiwi build /srv/kb --packs /var/lib/kiwi/packs/kb --no-vec
```

`/etc/systemd/system/kiwi-refresh.timer`:

```ini
[Unit]
Description=Fetch and build the knowledge base every five minutes

[Timer]
OnBootSec=1min
OnUnitActiveSec=5min

[Install]
WantedBy=timers.target
```

An unchanged commit writes no pack, so a quiet repository costs a fetch. A build that
fails (a malformed declaration, a policy that does not parse) leaves the current pack
serving and the unit failed for your monitoring. Builds of one store take a lock, so
a manual `kiwi build` never races the timer. The server's alternative, `kiwi serve
--watch`, builds inside the server when `HEAD` moves: it still needs the fetch above
and write access to the packs, so `--follow` with a separate builder is the better fit
for a shared server.

## 4. Proposals as pull requests

In `.kiwi/kiwi.toml`:

```toml
[forge]
kind = "github"
remote = "origin"   # the clone's remote the branch is pushed to
base = "main"       # the base of every pull request
```

`kiwi_propose` commits each proposal on a new branch in the clone (the working tree is
never touched), pushes it to `origin` with `git push` and opens the pull request with
`gh pr create`, both as the server's user, with the `gh` login of step 2. Its token
must be able to push branches and open pull requests: a fine-grained personal access
token or a GitHub App installation token with *Contents* and *Pull requests* read and
write on the repository. An installation token lives one hour, so a timer must log in
again with a fresh one before it expires. The server's unit sets the same
`GH_CONFIG_DIR` (step 5).

Who may propose and who may merge without review is the policy's `propose` and
`merge` actions. A proposal that conflicts with content its author cannot see needs
an owner's review: its pull request opens as a draft labelled `kiwi:owner-review`, and
the owners named by `owner:` or `KIWIOWNERS` (`@user` or `@org/team`) are requested as
reviewers. Where drafts are not available (private repositories on GitHub's free
plan), it opens ready and labelled, so **protect `main`**: require pull requests, make
a status check fail while the `kiwi:owner-review` label is present (or use a ruleset),
and require CODEOWNERS review for the owners' paths. Requesting a team needs a token
that can read the organisation's teams. [configuration.md](configuration.md#proposals)
has the details.

## 5. Serve over HTTP with signed tokens

Kiwi validates tokens; it does not issue them. With your own RS256 key:

```sh
sudo install -d -m 755 /etc/kiwi
sudo openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:3072 -out /etc/kiwi/jwt.pem
sudo chmod 600 /etc/kiwi/jwt.pem
sudo sh -c 'openssl pkey -in /etc/kiwi/jwt.pem -pubout > /etc/kiwi/jwt-public.pem'
```

A token per agent and user, minted as root with `openssl` alone
(`mint-token <user> <agent> <days>`):

```sh
#!/bin/sh
set -eu
ISS=urn:example:kiwi-issuer
AUD=http://127.0.0.1:8740/mcp
b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }
now=$(date +%s)
header=$(printf '{"alg":"RS256","typ":"JWT"}' | b64url)
claims=$(printf '{"iss":"%s","aud":"%s","sub":"%s","azp":"%s","scope":"kiwi:read","iat":%s,"exp":%s}' \
  "$ISS" "$AUD" "$1" "$2" "$now" "$((now + 86400 * $3))" | b64url)
signature=$(printf '%s.%s' "$header" "$claims" | openssl dgst -sha256 -sign /etc/kiwi/jwt.pem -binary | b64url)
printf '%s.%s.%s\n' "$header" "$claims" "$signature"
```

`aud` is the resource URL the server is started with, `sub` a user of
`principals.yaml`, `azp` an agent of it, and `scope` either `kiwi:read` or
`kiwi:label:<label>` scopes that narrow every call made with it to those labels. The
agent sends it as `Authorization: Bearer <token>`.

`/etc/systemd/system/kiwi.service`:

```ini
[Unit]
Description=Kiwi knowledge base server
After=network-online.target

[Service]
User=kiwi
Environment=HOME=/var/lib/kiwi GH_CONFIG_DIR=/var/lib/kiwi/gh
ExecStart=/opt/kiwi/kiwi serve /srv/kb --follow \
  --packs /var/lib/kiwi/packs/kb --state /var/lib/kiwi/state \
  --http 127.0.0.1:8740 --auth-rs256-pem /etc/kiwi/jwt-public.pem \
  --issuer urn:example:kiwi-issuer --resource http://127.0.0.1:8740/mcp
ExecReload=/bin/kill -HUP $MAINPID
Restart=on-failure
NoNewPrivileges=yes
ProtectSystem=strict
ReadWritePaths=/srv/kb /var/lib/kiwi/state /var/lib/kiwi/gh

[Install]
WantedBy=multi-user.target
```

`systemctl reload kiwi` makes the server check its stores at once. Behind a reverse
proxy with TLS, use the public URL as `--resource` and in `aud`, and list its host with
`--allowed-host`.

The endpoint is stateless: each request carries its token and is answered on its own,
as one JSON body. There is no session to idle out, so an agent that works for an hour
between two calls is answered like any other, and a restart of the server (a release,
a `Restart=on-failure`) breaks no connected agent. A proxy needs no sticky routing and
no streaming support.

**Answer sizes.** A request may name its own token budget; when it names none, Kiwi
uses these defaults, which suit long research by agents and stay under the
25,000-token tool-output ceiling of common MCP clients. Set them in the corpus's
`.kiwi/kiwi.toml` only to change them:

```toml
[map]
budget = 4000    # tokens of a kiwi_map answer
[search]
budget = 1000    # tokens for each query of a kiwi_search (the answer: this times its queries)
limit = 10       # results for each query
[read]
budget = 12000   # tokens of text of a kiwi_read answer; whole documents, cut section by
                 # section with a `more` that says how to read on
```

Every answer is at most 32,000 tokens, whatever is asked.

**Rotating the key.** The public key file is re-read when its content changes, and
each key in it is tried. Append the new public key to `jwt-public.pem`, sign new
tokens with the new private key, and remove the old public key once its last token
has expired. An empty key file revokes every token at once.

## 6. Several corpora

A second corpus, for example a catalogue generated from a database into its own Git
repository (one small document per record), is served by the same server under its
own namespace, policy and audit log. With several corpora, `--packs` and `--state` hold
one directory per namespace:

```sh
kiwi build /srv/catalogue --packs /var/lib/kiwi/packs/catalogue --no-vec   # its own timer
kiwi serve /srv/kb /srv/catalogue --follow \
  --packs /var/lib/kiwi/packs --state /var/lib/kiwi/state …
```

Each store is followed on its own, so each corpus is rebuilt on its own schedule. When a
single-corpus server becomes a federated one, its state moves too: stop it and move
the files of `/var/lib/kiwi/state` into `/var/lib/kiwi/state/kb` before starting it
with both corpora, so the audit chain continues.
Ids in answers are qualified (`catalogue:items/svc-00123`), and a search can stay in
one corpus with `"filters": {"namespace": ["catalogue"]}`. A generated corpus commits
what it generates (`git add -A && git commit`) before its build, since builds read
commits. [design.md](design.md#results) has search quality and latency on a
10,000-record catalogue.

## 7. Audit

Every call appends a hash-chained record to `audit.log` in the state directory
(`<state>/<namespace>` with several corpora; fields in
[configuration.md](configuration.md#audit-records)). Ship copies with sinks in
`.kiwi/kiwi.toml`:

```toml
[[audit.sinks]]
kind = "http"
url = "https://collector.example.com/kiwi"
bearer_env = "KIWI_AUDIT_TOKEN"     # also listed in KIWI_SECRET_ENV
```

Set `KIWI_AUDIT_TOKEN` and `KIWI_SECRET_ENV=KIWI_AUDIT_TOKEN` in the server's
environment. `stdout`, `stderr` (the journal under systemd) and `syslog` sinks need no
secret. For batch shipping instead, export verified records after the last sequence
number you stored:

```sh
kiwi audit export /var/lib/kiwi/state/audit.log --since 41234 > batch.jsonl
kiwi audit verify /var/lib/kiwi/state/audit.log --anchor <last hash you stored>
```

`kiwi gaps /srv/kb --state /var/lib/kiwi/state --principal <writer>` lists what agents
searched for and reported missing.

## 8. Upgrading Kiwi

A corpus's `.kiwi/kiwi.toml` may use an option only a newer Kiwi knows. Every unknown
key is an error, so an older server's build fails on that commit and the previous pack
keeps serving until the binary is upgraded. Keep the two in step:

- **Pin the version** the server runs, and run the same version in the corpus's CI on
  every pull request:

  ```sh
  kiwi lint . && kiwi policy test . && kiwi build . --no-vec
  ```

  A change that needs a newer Kiwi then fails its pull request instead of the server's
  build.
- **Upgrade the server first**, then merge changes that use new options. To upgrade:
  install the new binary beside the old one, run one build of every corpus with it
  (`systemctl start kiwi-refresh`), switch the path the units run, and restart
  `kiwi.service` once. Check the download's SHA-256 against the release's `.sha256`
  file.
- Earlier packs stay readable for pinned citations (`packs.keep`), and the audit log
  goes on with the same chain.
