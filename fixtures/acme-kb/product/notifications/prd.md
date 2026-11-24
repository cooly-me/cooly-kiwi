---
id: PRD-005
type: prd
status: draft
review_by: 2026-01-15
summary: Draft requirements for buyer and seller notifications on Acme Market across push, email and SMS, including quiet hours, user preferences and which messages may never be switched off.
tags: [notifications, growth]
---

# Notifications

Acme Market sends about 40 million notifications a month across email, push and SMS, from six
different services that each decide on their own when to contact people. Buyers complain about
volume, sellers miss order notices that matter, and nobody can answer the question "what did we
send this person last week?". This draft proposes one notifications platform with shared rules.
The delivery design is in RFC-014 and the choice of push provider in ADR-008. This document has
no owner yet; it was started by the growth team and needs a product owner before review.

## Goals {#goals}

- Every notification goes through one service that applies the same preferences, quiet hours and
  rate limits, whichever team triggered it.
- Reduce marketing notifications per active buyer by 30 percent while keeping repeat purchase
  rate flat, in line with the vision's principle of fewer, better notifications.
- Deliver order notices to sellers within 60 seconds of the order being confirmed, at p95.
- Give support a timeline of every notification sent to an account, with channel and status.

Non-goals for the first version: in-app inbox redesign, WhatsApp as a channel, and
personalised send-time optimisation.

## Channels {#channels}

Three channels are in scope.

- Push to the iOS and Android apps, for time-sensitive messages: new order for a seller, order
  shipped, a message from a buyer or seller.
- Email, for everything that needs a record: receipts, payout statements, policy changes, and
  the weekly digest. Email is also the fallback when push is not available or fails.
- SMS, only for security and critical account messages: sign-in codes, a payout destination
  change, and a suspended account. SMS costs about 40 times as much as email per message and is
  never used for marketing.

Each notification type declares its channels in priority order and whether it is transactional
or marketing. A type that tries to use a channel it has not declared is rejected at send time.

## Quiet hours {#quiet-hours}

Nobody wants a "your favourite shop has new items" push at two in the morning. By default, quiet
hours run from 22:00 to 08:00 in the recipient's local time zone, taken from the device for push
and from the account address otherwise. During quiet hours marketing notifications are held and
delivered at the end of the window, or dropped if they have expired by then.

Transactional notifications have two classes. Urgent ones, such as sign-in codes and fraud
alerts, are always delivered immediately. Non-urgent ones, such as "your order has shipped", are
delivered by email immediately but held for push until quiet hours end. Sellers may shorten
quiet hours for new-order pushes, since many want to know at once. Quiet hours are enforced in
the fan-out service, not by each sender, as RFC-014 describes.

## Preferences {#preferences}

People choose what they receive per category and per channel: order updates, messages, shop
updates from favourited sellers, price drops on saved items, recommendations, and Acme Market
news. Defaults differ by role: sellers get order and message notices on every channel they have
enabled, buyers get order updates and messages by push and email but recommendations only by
email, at most twice a week.

Security notices and legally required messages, such as a change to the terms, cannot be turned
off. Every marketing email includes a one-click unsubscribe that takes effect within one minute
across all channels for that category. Preferences are stored once and read by the fan-out
service at send time, so a change applies to messages already queued.

## Open questions {#open-questions}

- Who owns this document? Growth started it, but the platform team will run the service, and
  sellers' order notices are critical to checkout.
- Should the weekly digest replace most individual marketing pushes, and how do we measure the
  effect on repeat purchases?
- Do we respect the device's own focus modes in addition to our quiet hours, or rely on the
  operating system for that?
- What is the budget for SMS in countries where it is expensive, and do we offer sign-in codes by
  an authenticator app instead?

The review date for this draft has passed; it needs an owner and an updated plan.
