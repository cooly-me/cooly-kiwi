---
id: ADR-003
type: adr
status: deprecated
owner: "@team-payments"
review_by: 2026-06-30
summary: Records the original decision to capture card payments synchronously inside the checkout request; deprecated and superseded by ADR-007 because of latency and duplicate charges on retries.
code: ["services/checkout/**", "services/payment/**"]
tags: [payments, checkout]
---

# Synchronous payment capture at checkout

This decision is deprecated. It was replaced by
[ADR-007](adr-007-async-capture.md), which moves capture out of the buyer's request and into the
checkout saga described in RFC-010. The text below is kept so that readers understand why the
old checkout behaved as it did and why it was changed. Do not use it as guidance for new work. Engineers looking for
the current behaviour should read ADR-007 and RFC-010 instead.

## Context {#context}

In 2022 checkout was part of the monolith. The payment processor we used at the time supported
authorise-and-capture in a single call, and the team was small. Sellers had complained about
orders whose payment later failed, because the old flow created the order first and charged the
card in a nightly batch, so some sellers made and shipped items that were never paid for.

The team wanted the simplest possible guarantee that an order shown to a seller had been paid:
take the money before the order exists. Order volume was about a tenth of today's, the processor's
response time was usually under a second, and nobody yet ran more than one checkout server per
region, so the risks of a long synchronous call looked small.

## Decision {#decision}

The checkout request authorises and captures the full basket amount in one synchronous call to
the payment processor, inside the HTTP request that the buyer's "Place order" button sends. Only
if the capture succeeds does the checkout create the orders in the same request and return the
confirmation page. If the capture fails or times out, the request returns an error and the buyer
is asked to try again.

The processor call had a 20-second timeout. No idempotency key was sent to the processor, because
the processor did not support one at the time, and the client did not send one either.

## Consequences {#consequences}

Sellers stopped seeing unpaid orders, which solved the original problem. But as volume grew the
decision caused two serious issues.

First, latency: checkout p99 reached 4.2 seconds, dominated by the capture call, and spikes in the
processor's latency translated directly into abandoned checkouts. Second, duplicate charges: when
the capture succeeded but the response was lost or arrived after the client's timeout, the buyer
pressed the button again and was charged a second time. At its worst this produced about 30
duplicate charges per 100,000 orders, each requiring a manual refund and a support contact.

These problems led to the platform-wide idempotency rule in ADR-005 and to the asynchronous
capture design in ADR-007, which supersedes this decision.
