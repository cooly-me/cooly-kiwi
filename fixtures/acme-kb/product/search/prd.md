---
id: PRD-002
type: prd
status: approved
owner: "@team-search"
review_by: 2027-03-01
summary: Requirements for search and discovery on Acme Market, covering relevance ranking, filters and facets, typo tolerance, latency targets and the metrics used to judge whether buyers find what they want.
decided_by: [ADR-004]
tags: [search, discovery]
---

# Search and discovery

Most buyers start with the search box: 64 percent of sessions that end in a purchase include at
least one search. Handmade goods are hard to search because sellers describe the same thing in
many ways ("ceramic mug", "stoneware cup", "pottery tumbler") and because buyers often search by
occasion or feeling rather than by object. This document sets out what search must do for buyers
and sellers. The indexing pipeline is described in
[RFC-011](../../rfcs/rfc-011-search-indexing.md) and the choice of engine in ADR-004.

## Goals {#goals}

- Increase the share of search sessions that lead to a listing click from 62 percent to 72
  percent within four quarters.
- Make new and updated listings searchable within one minute of the seller saving them, so that
  sellers trust that their edits matter.
- Give small and new sellers a fair chance to be seen: at least 15 percent of first-page results
  should come from shops less than six months old, without lowering click-through.
- Keep the experience fast on mobile networks, as defined in the latency targets below.

Out of scope for this period: image search, personalised home-page recommendations (owned by
@team-growth) and sponsored listings, which the vision rules out as disguised placement.

## Ranking {#ranking}

Ranking happens in two stages. The first stage retrieves up to 1,000 candidates with a lexical
match over title, tags, materials and description, weighted in that order. The second stage
re-scores the candidates with a learned model using about 40 features: text relevance, listing
quality score, the shop's dispatch reliability, conversion rate smoothed for low-traffic
listings, price relative to similar items, and freshness.

Two rules override the model. A listing that violates listing-quality rules (see PRD-003) is
excluded entirely rather than demoted. And no single shop may hold more than three of the first
twenty-four results, so one prolific seller cannot crowd out everyone else. Ranking changes ship
behind an experiment flag and need a two-week A/B test with click-through and purchase rate as
the primary measures before they become the default.

## Filters and facets {#filters}

Buyers can narrow results by price range, category, colour, material, shipping destination,
estimated delivery date, and "ships within 3 days". Facet counts are shown next to each value and
are computed on the filtered result set, so a buyer never clicks a filter that leads to an empty
page.

Colour and material are normalised from free text at indexing time into a fixed vocabulary of 48
colours and about 300 materials, because sellers write "sage", "olive" and "moss" for similar
greens. The original words remain searchable. The shipping-destination filter defaults to the
buyer's country and hides listings that cannot be shipped there, which removes the most common
cause of abandoned baskets after a search. Filters are reflected in the URL so that a filtered
search can be shared and bookmarked.

## Typo tolerance {#typo-tolerance}

Buyers misspell often, especially on phones and for craft words like "macrame" or "crochet".
Search tolerates one edit for query terms of four to seven characters and two edits for terms of
eight characters or more; terms of three characters or fewer must match exactly, because short
typos produce too many false matches. Exact matches always rank above fuzzy matches with the same
other signals.

Beyond edit distance, a synonym list maintained by the search team maps common equivalents, for
example "mug" and "cup", or "earrings" and "studs". Synonyms are one-directional where meaning
narrows: "jewellery" expands to "necklace", but "necklace" does not expand to all jewellery. If a
query returns fewer than five results, we show a "did you mean" suggestion built from the most
frequent successful queries within one edit.

## Latency targets {#latency-slos}

Search must feel instant. The service-level objectives, measured at the search API and excluding
the client network, are:

- p95 below 200 milliseconds and p99 below 400 milliseconds for queries with up to three filters.
- Facet computation adds no more than 50 milliseconds at p95.
- Availability of 99.9 percent per calendar month for the search API.

When the second-stage model is slow or unavailable, search falls back to first-stage lexical
ranking rather than failing; this degraded mode is acceptable for up to one hour before it
counts against the availability objective. Error budgets are reviewed monthly; if more than half
of the budget is used, ranking experiments pause until reliability work brings it back.

## Metrics {#metrics}

- Search click-through: sessions with a search that include a listing click.
- Zero-result rate: searches returning no listings, broken down by query length and language.
- Purchase rate after search: orders within 24 hours attributed to a search session.
- New-shop exposure: share of first-page impressions from shops under six months old.
- Index freshness: time from a listing change to the change being searchable, p50 and p95.
- Latency and availability against the objectives above.

The search team reviews the 200 most frequent zero-result queries every week and either adds
synonyms, fixes normalisation, or files a gap for the seller team when the inventory simply does
not exist.
