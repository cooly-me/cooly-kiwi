---
id: RFC-014
type: rfc
status: draft
owner: "@team-growth"
review_by: 2027-01-31
summary: Draft design for a single notification fan-out service that turns domain events into push, email and SMS messages, applying preferences, quiet hours, rate limits and the email fallback for push.
implements: [PRD-005]
decided_by: [ADR-008]
depends_on: [ADR-002]
code: ["services/notifications/**"]
tags: [notifications, growth, kafka]
---

# Notification fan-out

[PRD-005](../product/notifications/prd.md) asks for one service through which every notification
passes, so that preferences, quiet hours and limits apply consistently. This draft describes that
service. It consumes domain events from Kafka, decides who should be told what on which channel,
and hands messages to channel providers. The push provider and its email fallback follow
[ADR-008](../adr/adr-008-push-provider.md), which is still in review. The
service will be built by the growth team and operated by the platform team.

## Summary {#summary}

Producers stop sending notifications themselves. Instead they publish domain events they already
emit, such as `OrdersCreated` from the checkout saga or `PayoutSent` from the payouts service, and
notification rules map events to notification types. The fan-out service in
`services/notifications` evaluates the rules, loads the recipient's preferences and devices,
applies quiet hours and rate limits, renders the message from a template, and enqueues one job
per channel.

Channel workers deliver the jobs to the push provider, the email provider and the SMS provider,
record delivery status, and feed failures back for fallback. Every decision, including messages
suppressed by preferences or quiet hours, is written to a notification log so support can see
what was and was not sent.

## Fan-out design {#fan-out}

The service is split into a planner and channel workers. The planner consumes events, and for
each event produces a plan: the recipients, the notification type, and the ordered list of
channels to try. Large fan-outs, such as "a shop you favourite has new items", which can reach
200,000 followers, are expanded in pages of 1,000 recipients so that one event cannot block the
queue for minutes.

Plans are written to a per-channel queue partitioned by recipient id, which keeps one person's
messages ordered and makes per-recipient rate limiting cheap. The default limits are three
marketing pushes per day and one marketing email per day per person, with no limit on
transactional messages. Templates are versioned and rendered in the recipient's language at plan
time, so a template change never alters a message already queued.

## Quiet hours enforcement {#quiet-hours-enforcement}

Quiet hours are enforced by the planner, not by producers. For each recipient and channel the
planner computes the local time from the device time zone (push) or the account's address
(email and SMS). If the message is marketing and falls inside the recipient's quiet window,
22:00 to 08:00 by default, the job is scheduled for the end of the window, and dropped if its
expiry passes first.

Urgent transactional messages, such as sign-in codes, bypass quiet hours. Non-urgent
transactional messages go out immediately by email but are deferred on push, as PRD-005
specifies. Deferred jobs sit in a delay queue keyed by release time. To avoid a burst at 08:00 in
large time zones, release times are spread randomly over the first 30 minutes after the window
ends.

## Fallback to email {#email-fallback}

ADR-008 proposes one push provider with email as the fallback. A push job falls back to email when
the recipient has no registered device, when the provider reports the token as invalid, or when
the provider has not acknowledged the message within 30 seconds. Invalid tokens are removed from
the device registry so that we stop trying them.

Fallback applies only to notification types that declare email as a later channel. Marketing
pushes do not fall back, because sending an email someone did not ask for is worse than sending
nothing. When the push provider is down entirely, a circuit breaker opens after 5 percent of
requests fail in a minute, and transactional pushes route straight to email until the provider
recovers.

## Open questions {#open-questions}

- Should the planner read preferences from the account service on every message, or keep a local
  copy updated by events? The first is simpler; the second survives an account service outage.
- How long do we keep the notification log? Support wants 13 months; storage cost suggests 90
  days for marketing and 13 months for transactional.
- Do sellers need a digest mode for new orders during busy periods, or is one push per order still
  right?
- Who owns the templates once product teams no longer send notifications directly?

This draft needs PRD-005 to gain an owner and ADR-008 to be approved before it moves to review.
