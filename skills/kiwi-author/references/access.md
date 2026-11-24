# Access reference

## KIWIOWNERS

```text
# pattern          declarations
*                  label=internal  owner=@team-docs
vision.md          label=public
finance/**         label=confidential groups=finance  owner=cfo@example.com
legal/             access=legal
product/           owner=@team-product
```

- Patterns are globs on the path relative to the corpus root, matched ignoring case.
  As in CODEOWNERS, a slash at the start or in the middle anchors a pattern to the
  root (`/hr`, `finance/**`, `docs/hr`); otherwise it matches at any depth. A name also
  covers everything under a directory of that name (`hr` covers `hr/plan.md`), and a
  trailing `/` means only a directory (`hr/` covers `hr/plan.md` and
  `teams/hr/plan.md`, not a file named `hr`). The last match wins.
- `label=<label>` sets the label; `groups=<a,b>` limits it to members of those groups;
  `access=<a,b>` is shorthand for `label=confidential groups=<a,b>`.
- `owner=<owner>` names the default owner of every document the line covers: one
  team or person (`@team-product`, `cfo@example.com`), like frontmatter `owner:`; a
  comma list is an error. A document's frontmatter `owner:` replaces it. The default
  is what `kiwi_read` shows, what the `owner` filter matches, the owner a stub names
  and what silences the `missing-owner` warning.
- A line declares access, an owner, or both. Each resolves on its own, from the last
  matching line that declares it: `product/ owner=@team-product` keeps the access an
  earlier line gave `product/`, and an access-only line keeps the owner an earlier
  line gave. An owner is never access; it changes no reader's permissions.
- Any other key, or a key given twice, is an error.

## Heading attributes

`## Pricing {#pricing access=finance}` or `## Plan {#plan label=restricted groups=exec}`.
Values cannot contain spaces. A section's access *adds* to its document's: a reader
needs permission for the document and for every narrowing above the section, so a
section can be more restricted than its document, never less. This keeps breadcrumbs
safe: whoever sees a section may see the headings above it.

## The policy

`.kiwi/policy.cedar` is a Cedar policy. Kiwi asks one question per distinct access
declaration: may principal `Kiwi::User::"<id>"` do `Kiwi::Action::"read"` on resource
`Kiwi::Content` with attributes `label` (string) and `groups` (set of strings)?

- The principal has attribute `groups` and is a member of `Kiwi::Group::"<group>"`.
- The context has `agent` (the MCP client id) and `channel` (`stdio` or `http`).
- `Kiwi::Action::"propose"` and `Kiwi::Action::"merge"` decide who may propose changes
  and who may merge without review.
- Plugins are asked about as `Kiwi::User::"plugin:<name>"` with `context.agent ==
  "plugin"` and `context.channel` `local` or `network` (when the plugin declares
  network access).

Example:

```cedar
permit (principal, action == Kiwi::Action::"read", resource)
when { resource.label == "public" };

permit (principal in Kiwi::Group::"staff", action == Kiwi::Action::"read", resource)
when { resource.label == "internal" &&
       (resource.groups.isEmpty() || principal.groups.containsAny(resource.groups)) };

permit (principal, action == Kiwi::Action::"read", resource)
when { resource.label == "confidential" && principal.groups.containsAny(resource.groups) };
```

## Agents and sessions

`principals.yaml` can limit an agent's grant (`labels: [public, internal]`); an OAuth
token can limit a session with `kiwi:label:<label>` scopes. The effective permission is
the intersection of the user's, the agent's and the session's.
