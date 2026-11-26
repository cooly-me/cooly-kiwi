---
id: ADR-004
type: adr
status: approved
owner: "@team-search"
review_by: 2027-06-30
summary: Records the decision to run product search on a self-managed OpenSearch cluster instead of PostgreSQL full-text search or a hosted search service, and what that commits the search team to operate.
code: ["services/search-api/**", "infra/opensearch/**"]
tags: [search]
---

# OpenSearch for product search

This decision chooses the engine behind product search on Acme Market. It was taken when the
search requirements in PRD-002 outgrew the full-text search built into the listing database, and
it determines how the indexing pipeline in RFC-011 is designed. The search team owns the cluster, its capacity
planning and its upgrades, and is on call for it alongside the search API.

## Context {#context}

Search originally used PostgreSQL full-text search on the listings table. With 4.6 million active
listings it could no longer meet the latency targets in PRD-002: queries with three filters and
facet counts took 600 to 900 milliseconds at p95, and facet counts in particular required
expensive aggregate queries on the primary database. It also lacked good typo tolerance, custom
analysers for craft vocabulary, and a way to plug in a learned re-ranking stage.

We needed an engine with inverted indexes and fast aggregations, fuzzy matching with configurable
edit distance, synonym support, the ability to re-rank the top results with an external model,
and zero-downtime reindexing. Cost mattered: search traffic is about 1,800 queries per second at
peak, and hosted search services priced per request would cost several times more than running
our own cluster at that volume.

## Decision {#decision}

Product search runs on OpenSearch, on a cluster managed by the search team on our cloud
provider's managed OpenSearch service. The cluster has dedicated master nodes, data nodes across
three availability zones, and separate indexing nodes used during rebuilds. Searches go through
the `services/search-api` service, never directly from clients, and always query an alias so that
indexes can be swapped.

The index is fed only by the event-driven pipeline in RFC-011; no service writes to OpenSearch
directly. Learned re-ranking runs in the search API over the top candidates returned by
OpenSearch rather than inside the cluster, so that ranking models can be deployed independently
of the cluster.

## Consequences {#consequences}

Search latency fell to about 120 milliseconds at p95 in load tests, inside the PRD-002 targets,
and facet counts no longer touch the primary database. Typo tolerance and synonyms are handled by
analysers the search team controls.

The search team now operates a stateful cluster: capacity planning, version upgrades, shard
sizing and snapshot management are its responsibility, with on-call coverage. Search results are
eventually consistent with the listing database, typically by under a minute, which sellers notice
when they edit a listing and search for it immediately. Two systems hold listing data, so the
pipeline must include drift detection, which RFC-011 provides.

## Alternatives considered {#alternatives}

- Keep PostgreSQL full-text search with more replicas. Cheapest, but it could not meet the facet
  and typo-tolerance requirements.
- A hosted search-as-a-service product. Excellent relevance tooling and no operations, but at our
  query volume the cost was estimated at three to four times a self-run cluster, and custom
  re-ranking was limited.
- A vector database for semantic search. Promising for discovery by mood or occasion, but not a
  replacement for lexical search and filters; it may be added later alongside OpenSearch.
