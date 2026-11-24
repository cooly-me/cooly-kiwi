---
id: RFC-010
type: rfc
status: approved
owner: "@team-payments"
review_by: 2027-03-01
summary: Designs checkout as an orchestrated saga over Kafka, with inventory reservation, payment authorisation, order creation and asynchronous capture as steps, each with compensation, idempotency, timeouts and tracing.
implements: [PRD-001]
decided_by: [ADR-002, ADR-007]
code: ["services/checkout/**", "services/payment/**"]
tags: [payments, checkout, saga, kafka]
---

# Checkout as a saga over Kafka

This RFC describes how checkout is built to meet the requirements in
[PRD-001](../product/checkout/prd.md). The old checkout captured the payment synchronously inside
the HTTP request, as ADR-003 decided; that made the request slow and made retries dangerous.
ADR-007 replaced it with asynchronous capture, and this document is the design that realises it:
checkout becomes a saga, a sequence of local transactions in separate services coordinated by
messages on the Kafka event bus chosen in [ADR-002](../adr/adr-002-kafka.md).

## Summary {#summary}

A checkout orchestrator in `services/checkout` owns the state of each checkout and drives it
through a fixed sequence of steps. Each step is a command sent to the owning service over Kafka;
each service performs a local transaction in its own PostgreSQL database and replies with an
event. The orchestrator persists its state after every transition, so it can crash and resume at
any point.

The buyer-facing request returns as soon as payment is authorised and the order is created,
typically in under a second. Capture, seller notification and receipts happen after the response.
If any step fails, the orchestrator runs compensating actions for the steps already completed, in
reverse order. The design favours a central orchestrator over choreography because checkout has a
strict order of steps and support needs one place to see where a checkout stands.

## Saga steps {#saga-steps}

The happy path has five steps.

1. Reserve inventory. The inventory service reserves the quantities for each line for 15 minutes
   and replies `InventoryReserved` or `InventoryUnavailable`.
2. Authorise payment. The payment service asks the processor to authorise the basket total
   against the buyer's payment method, including any 3-D Secure challenge, and replies
   `PaymentAuthorised` or `PaymentDeclined`.
3. Create orders. The order service splits the basket into one order per seller, in state
   `confirmed`, and replies `OrdersCreated`. At this point the orchestrator answers the buyer's
   HTTP request with the confirmation.
4. Capture payment. The payment service captures the authorised amount, asynchronously, as ADR-007
   decides, and replies `PaymentCaptured`.
5. Notify. Seller notifications and the buyer's receipt are published as events for the
   notification fan-out in RFC-014 and the ledger in RFC-013 to consume.

Each command and event carries the checkout id, the step number and an attempt counter.

## Compensation {#compensation}

Every step that changes state has a compensating action, and compensation runs in reverse order
of completion.

- If authorisation is declined, the orchestrator sends `ReleaseInventory` and marks the checkout
  `failed`; the buyer sees the decline reason.
- If order creation fails after authorisation, it sends `VoidAuthorisation` and then
  `ReleaseInventory`.
- If capture fails permanently, for example because the authorisation expired, it sends
  `CancelOrders`, which notifies the sellers, and then `ReleaseInventory` for items not yet
  shipped. The buyer is told the order was cancelled and that no money was taken.

Compensating actions must themselves be idempotent and must always succeed eventually; they are
retried indefinitely with backoff and alert after one hour. A checkout never sits in a
half-compensated state without a page to the on-call engineer.

## Idempotency {#idempotency}

Kafka delivers messages at least once, and buyers retry. Both make duplicates normal, so every
participant must tolerate them.

At the edge, the "Place order" request carries the client-generated idempotency key required by
[ADR-005](../adr/adr-005-idempotency-keys.md). The orchestrator uses that key as the checkout id,
so a retried request finds the existing checkout and returns its current result instead of
starting a new one.

Inside the saga, each command carries a deterministic key derived from the checkout id and the
step, for example `chk_01H...:authorise`. Services record processed keys in the same transaction
as their state change and ignore duplicates, replying with the original event. The payment
service passes the same key to the processor on authorise and capture calls, so a duplicate
command cannot reach the card network twice. This is the mechanism that keeps the duplicate
charge rate in PRD-001 at zero.

## Timeouts {#timeouts}

Each step has a deadline, after which the orchestrator treats the step as failed and starts
compensation.

- Inventory reservation: 2 seconds for the reply; the reservation itself expires after 15
  minutes.
- Authorisation: 10 seconds without a challenge; with 3-D Secure, the orchestrator waits up to 10
  minutes for the buyer to complete it.
- Order creation: 2 seconds.
- Capture: retried with exponential backoff from 1 second up to 5 minutes between attempts, for at
  most 72 hours, well inside the processor's authorisation validity.

A timed-out step may still complete later. Because every step is idempotent and compensations
check current state, a late `PaymentAuthorised` after a void is answered by voiding again rather
than by resurrecting the checkout.

## Observability {#observability}

Support and engineering must be able to answer "what happened to this checkout?" in one place.

The orchestrator writes every transition to a `checkout_events` table, which the support tool
reads to show a timeline. All commands and events carry a trace context, so a checkout appears as
one distributed trace spanning the orchestrator, inventory, payment and order services. Metrics
are emitted per step: attempts, successes, failures by reason, and latency histograms.

Alerts fire when the authorisation success rate drops more than one point in an hour, when any
checkout stays in `capturing` for more than 30 minutes, when compensation has been retrying for
more than an hour, and when the Kafka consumer lag of any saga participant exceeds 10,000
messages. Dashboards break latency down by step so that a slow processor is visible immediately.
