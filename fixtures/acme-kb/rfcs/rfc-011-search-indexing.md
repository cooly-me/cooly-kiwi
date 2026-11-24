---
id: RFC-011
type: rfc
status: approved
owner: "@team-search"
review_by: 2027-03-01
summary: Designs the pipeline that keeps the OpenSearch product index in step with listings, using change events from Kafka, an enrichment stage, zero-downtime reindexing through aliases, and freshness monitoring.
implements: [PRD-002]
decided_by: [ADR-004]
depends_on: [ADR-002]
code: ["services/search-indexer/**", "services/search-api/**"]
tags: [search, indexing, kafka]
---

# Search indexing pipeline

This RFC describes how listings get from the seller's "Save" button into the search index within
the one-minute freshness goal of [PRD-002](../product/search/prd.md). The index lives in
OpenSearch, as [ADR-004](../adr/adr-004-opensearch.md) decided. The pipeline consumes change
events from the Kafka event bus (ADR-002), so the listing service never writes to the search
cluster directly and a search outage cannot block sellers from editing their shops.

## Summary {#summary}

The pipeline has three stages. Change capture turns every committed listing change into an event
on the `listings.changed` topic. An enrichment stage in `services/search-indexer` loads the full
listing, joins shop-level signals such as dispatch reliability, normalises colours and materials,
and computes the quality score features. A writer stage sends bulk requests to OpenSearch.

The same code path serves incremental updates and full rebuilds: a rebuild simply replays every
listing through enrichment into a new index. That keeps one definition of what a document in the
index looks like, which was the main source of bugs in the previous nightly batch job, where the
batch and the live updater had drifted apart.

## Change capture {#change-capture}

Listing changes are captured with the transactional outbox pattern. When the listing service
commits a change to PostgreSQL, the same transaction inserts a row into an outbox table. A relay
process reads the outbox in commit order and publishes to `listings.changed`, keyed by listing id
so that all changes to one listing land on the same partition and stay ordered.

Shop-level changes, such as a shop going on holiday or being suspended, are published to
`shops.changed`; the indexer fans them out to every listing of the shop. Order events from the
checkout saga feed a separate, slower signal stream for conversion rates, aggregated every 15
minutes, because conversion does not need second-level freshness. Events carry only ids and a
version number; enrichment always reads the current state, which makes out-of-order or duplicate
events harmless.

## Reindexing {#reindexing}

Mapping changes, analyser changes and new features require a full rebuild. Searches always go to
an alias, `listings-live`, never to a concrete index. A rebuild creates a new index such as
`listings-2026-11-20`, replays all 4.6 million active listings through enrichment at about 6,000
documents per second, which takes roughly 15 minutes, and meanwhile also applies live changes to
both the old and new index.

When the new index has caught up, an automated check compares document counts and runs a sample
of 2,000 recorded queries against both indexes, flagging any query whose top ten results change
by more than half. If the check passes, the alias flips atomically. The old index is kept for 48
hours for rollback. Rebuilds run on dedicated indexing nodes so that search latency is unaffected.

## Freshness and failure {#freshness}

Freshness is measured end to end: every event carries the commit time, and the writer records the
time at which OpenSearch acknowledged the document. The p95 target is 60 seconds; an alert fires
if p95 exceeds 3 minutes for 10 minutes.

When OpenSearch rejects writes or is unavailable, the writer stops committing Kafka offsets and
retries with backoff, so no change is lost; the topic retains 7 days of events, far longer than
any expected outage. Documents that fail enrichment repeatedly, for example because of corrupt
data, go to a dead-letter topic after five attempts and appear on a dashboard for the search team.
A nightly job compares listing versions in PostgreSQL with those in the index and re-enqueues any
listing that drifted, as a safety net.

## Rollout {#rollout}

The pipeline replaces the nightly batch job in three phases. First, it runs in shadow mode,
writing to a separate index that no search reads, while we compare freshness and document
equality with the existing index for two weeks. Second, 10 percent of search traffic reads from
the new index, with click-through and latency compared in an A/B test as PRD-002 requires for
ranking changes. Third, all traffic moves over and the batch job is deleted.

The main risk is load on the listing database from enrichment reads during rebuilds. We mitigate
it by reading from a replica and by rate-limiting the rebuild. A second risk is Kafka partition
skew from very large shops; listing-level keys keep this manageable.
