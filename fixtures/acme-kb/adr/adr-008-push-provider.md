---
id: ADR-008
type: adr
status: review
owner: "@team-growth"
review_by: 2026-02-01
summary: Proposes sending all mobile push notifications through a single third-party push provider, with email as the automatic fallback when push is unavailable, instead of integrating each platform's gateway directly.
code: ["services/notifications/**"]
tags: [notifications, growth]
---

# One push provider with email fallback

This decision is in review. It chooses how the notification fan-out in
[RFC-014](../rfcs/rfc-014-notification-fanout.md) delivers push notifications to the iOS and
Android apps, and what happens when push cannot be delivered. It supports the notification
requirements in PRD-005, which is itself still a draft. Its review date has passed, and
the growth team owns bringing it to a decision with the platform team.

## Context {#context}

Today three services send push notifications, each with its own integration: the orders service
talks to the Apple and Google gateways directly, the messaging service uses one third-party
provider, and the marketing tool uses another. Device tokens are stored in three places and drift
apart, so a buyer who reinstalls the app may receive some notifications and not others. About 11
percent of push sends fail with invalid tokens, and nobody falls back to another channel when that
happens, so sellers miss order notices.

The fan-out service will be the only sender, so it needs one way to deliver push. The options are
to integrate the Apple and Google gateways directly, or to use one provider that wraps both and
handles token management, retries and delivery receipts. Volume is about 25 million pushes a month
and growing by roughly 30 percent a year.

## Decision {#decision}

We will send all push notifications through one third-party push provider, chosen through a short
evaluation of delivery-receipt quality, cost at our volume and data residency. The fan-out service
owns the only device-token registry and registers tokens with the provider.

Email is the fallback channel. When the recipient has no valid device, when the provider reports a
token as invalid, or when the provider does not acknowledge a message within 30 seconds, the
fan-out service sends the notification by email instead, if the notification type declares email
as a fallback. When the provider is unavailable, a circuit breaker routes all transactional pushes
to email until it recovers. Marketing pushes never fall back.

## Consequences {#consequences}

One token registry and one integration remove the drift between services and make delivery
measurable in one place. Sellers will receive their order notice by email when push fails, which
addresses the main complaint behind PRD-005. The provider's dashboards and receipts reduce what we
must build ourselves.

We take on a dependency on a single vendor for push, at an estimated cost of 0.0004 USD per
message, about 10,000 USD a month at current volume, which direct integration would avoid. A
provider outage degrades every push at once, mitigated by the email fallback. Moving to another
provider later means re-registering tokens, which the app can do silently on next launch. Review
is overdue: the growth team must confirm the vendor shortlist and the data processing terms before
this can be approved.

## Alternatives considered {#alternatives}

- Integrate the Apple and Google gateways directly. Cheapest per message and no vendor in the
  middle, but we would build token management, retries and receipts ourselves.
- Keep each service's integration and add fallback in each. Rejected because it keeps three token
  registries and three sets of rules.
- SMS as the fallback instead of email. Faster to be noticed, but about 40 times more expensive
  and reserved for security messages by PRD-005.
