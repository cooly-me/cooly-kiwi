// Acme Market knowledge policy. Default is deny: content is readable only when a
// `permit` below matches and no `forbid` does. Resources are `Kiwi::Content` entities
// with a `label` and a set of `groups`; principals are `Kiwi::User` entities that are
// members of their `Kiwi::Group`s and carry the same groups as an attribute.

// Public content: anyone.
permit (principal, action == Kiwi::Action::"read", resource)
when { resource.label == "public" };

// Internal content: staff; when the content names groups, a member of one of them.
permit (principal in Kiwi::Group::"staff", action == Kiwi::Action::"read", resource)
when {
  resource.label == "internal" &&
  (resource.groups.isEmpty() || principal.groups.containsAny(resource.groups))
};

// Confidential content: members of one of its groups.
permit (principal, action == Kiwi::Action::"read", resource)
when { resource.label == "confidential" && principal.groups.containsAny(resource.groups) };

// Restricted content: the exec team, and only through an authenticated HTTP session.
// Over stdio the user is whoever started the process (an assertion, not a proof), so a
// local session is not the most trusted channel: it never reads restricted content.
permit (principal in Kiwi::Group::"exec", action == Kiwi::Action::"read", resource)
when { resource.label == "restricted" && context.channel == "http" };

// Proposals: staff may propose changes to anything they can read.
permit (principal in Kiwi::Group::"staff", action == Kiwi::Action::"propose", resource);

// Direct merge (no review): only the platform team, and never for confidential or
// restricted content.
permit (principal in Kiwi::Group::"platform", action == Kiwi::Action::"merge", resource)
when { resource.label == "public" || resource.label == "internal" };

// Plugins on this machine may index everything; plugins that reach the network only
// public content.
permit (principal, action == Kiwi::Action::"read", resource)
when { context.agent == "plugin" && context.channel == "local" };

// Agents are an allow-list in principals.yaml: one not listed there reads nothing, and
// each listed agent's grant caps what it reads for anyone (untrusted-bot: public only).
