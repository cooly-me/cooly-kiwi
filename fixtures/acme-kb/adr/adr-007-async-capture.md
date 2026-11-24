---
id: ADR-007
type: adr
status: approved
owner: "@team-payments"
review_by: 2027-06-30
summary: Replaces synchronous capture with authorisation in the buyer's request and asynchronous capture as a later step of the checkout saga, cutting checkout latency and removing duplicate charges.
supersedes: [ADR-003]
depends_on: [ADR-002, ADR-005]
code: ["services/checkout/**", "services/payment/**"]
tags: [payments, checkout, saga]
---

# Asynchronous capture through the checkout saga

This decision supersedes [ADR-003](adr-003-sync-capture.md), which captured payments
synchronously inside the checkout request. It separates authorisation from capture and moves
capture into the checkout saga designed in [RFC-010](../rfcs/rfc-010-checkout-saga.md), so that
the buyer gets a fast confirmation and the money is taken reliably afterwards. It applies to every payment method that supports
separate authorisation and capture, which PRD-001 makes a requirement for new methods.

## Context {#context}

The synchronous capture of ADR-003 made checkout p99 latency 4.2 seconds and produced duplicate
charges when buyers retried after a timeout. PRD-001 sets a target of 1.5 seconds at p99 and fewer
than 2 defective orders per 100,000, which the old design could not meet.

Card networks distinguish two operations. Authorisation checks that the card is valid and reserves
the funds on the buyer's account; capture actually moves the money and can happen later, within
the authorisation's validity of about seven days for most cards. Our current processor supports
separate authorise and capture calls, accepts idempotency keys on both, and notifies us of
results by webhook. Meanwhile ADR-002 gave us a durable event bus and ADR-005 made idempotency a
platform rule, so a multi-step flow had become practical.

## Decision {#decision}

The buyer's "Place order" request performs only the steps needed to guarantee that the seller
will be paid: reserve inventory, authorise the payment, and create the orders. As soon as the
orders exist, the buyer receives the confirmation. Capture runs afterwards as a step of the
checkout saga, asynchronously, normally within seconds.

Sellers see an order only once authorisation has succeeded, which preserves the original intent
of ADR-003: no seller works on an unpaid order. Capture retries with backoff for up to 72 hours.
If capture finally fails, the saga cancels the orders and releases the authorisation, as RFC-010
describes. Every authorise and capture call carries a key derived from the checkout's idempotency
key, so retries at any level cannot charge twice.

## Consequences {#consequences}

Checkout p99 latency dropped from 4.2 seconds to 1.1 seconds after rollout, and duplicate charges
fell from about 30 per 100,000 orders to zero over the following quarter. A slow processor now
delays capture instead of the buyer's page.

There is a new failure mode: an order can be confirmed and later cancelled because capture fails,
for example when the authorisation expires or the issuer declines the capture. This happens for
about 4 in 100,000 orders, and buyers and sellers are told in plain words. Support needs a timeline
of each checkout to explain these cases, which RFC-010's observability section provides. The
system is more complex: the saga, its compensations and the processor webhooks all need testing
and monitoring that the single synchronous call did not.

## Alternatives considered {#alternatives}

- Keep synchronous capture but add idempotency keys. This would fix duplicates but not latency,
  which was the larger cost in lost conversion.
- Capture when the seller ships. Common in marketplaces and better for buyers, but authorisations
  expire before many handmade items ship, given processing times of up to three weeks.
- Capture in a nightly batch. Rejected because it brings back the unpaid-order problem that
  ADR-003 was created to solve.
