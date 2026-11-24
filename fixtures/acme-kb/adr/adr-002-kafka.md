---
id: ADR-002
type: adr
status: approved
owner: "@team-platform"
review_by: 2027-06-30
summary: Records the decision to use Kafka as the platform's event bus for asynchronous communication between services, with schema-registered events, the outbox pattern for publishing, and at-least-once delivery.
depends_on: [ADR-001]
code: ["infra/kafka/**", "libs/events/**"]
tags: [platform, events, kafka]
---

# Kafka as the event bus

Once each service owned its own database under [ADR-001](adr-001-postgres.md), services needed a
reliable way to tell each other what had happened. This decision chooses the technology and the
rules for that event bus. It underpins the checkout saga (RFC-010), the search indexing pipeline
(RFC-011), the payout ledger (RFC-013) and the notification fan-out (RFC-014). The platform team owns the cluster and the schema
registry.

## Context {#context}

After the split from the monolith, services were calling each other synchronously for everything,
including work that did not need an immediate answer, such as updating search or sending a
receipt. Chains of four or five HTTP calls made checkout fragile: if the email service was slow,
checkout was slow. We also had no durable record of business events, so rebuilding a read model
meant writing a one-off script against several databases.

We needed a messaging system that keeps events durably for days, preserves order per entity,
lets several consumers read the same events independently at their own pace, can replay history
to rebuild a consumer, and handles our peak of about 25,000 events per second during holiday
sales with room to grow. The platform team already had some experience running a streaming
cluster for analytics.

## Decision {#decision}

Kafka is the event bus for all asynchronous communication between services. We run it as a
managed service with three availability zones and a replication factor of three. Topics are named
`<domain>.<event>`, for example `listings.changed`, and keyed by the entity id so that events for
one entity stay ordered.

Every event schema is registered in the schema registry and must evolve compatibly: fields may be
added with defaults but never removed or renamed within a major version. Services publish through
the transactional outbox pattern, writing the event to an outbox table in the same database
transaction as the state change, so an event is published if and only if the change committed.
Delivery is at least once; every consumer must be idempotent. Default retention is 7 days;
money-related topics keep 30 days.

## Consequences {#consequences}

Services are decoupled in time: a slow consumer, such as the notifications service, no longer
slows down the producer. New consumers can be added without changing producers, and read models
can be rebuilt by replaying a topic. The saga in RFC-010 relies on this durability to resume after
crashes.

On the other hand, the system becomes eventually consistent, and engineers must handle duplicate
and out-of-order events everywhere, which is why idempotency is a platform rule in ADR-005.
Debugging spans several services and needs distributed tracing. Kafka itself is a significant
operational dependency: consumer lag must be monitored for every consumer group, and schema
changes need review by the platform team.

## Alternatives considered {#alternatives}

- A traditional message broker with queues. Simpler for point-to-point work, but replaying
  history and having many independent consumers of the same stream are awkward.
- The cloud provider's proprietary queue and notification services. Low operational effort, but
  no replay, limited ordering guarantees, and strong lock-in.
- Keeping synchronous HTTP calls with retries. Rejected because it was the source of the
  cascading failures that motivated this decision.
