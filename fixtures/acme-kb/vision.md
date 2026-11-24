---
id: VISION
type: vision
status: approved
owner: "@exec"
review_by: 2027-06-30
summary: Acme Market's mission, customers, three-year goals, product principles and non-goals, and the measures leadership uses to judge whether the marketplace for handmade goods is succeeding.
tags: [strategy]
---

# Acme Market vision

Acme Market is a two-sided online marketplace where independent makers sell handmade goods to
buyers who care who made the thing they are buying. This document is the compass for product
and engineering decisions for the next three years. It is owned by the executive team and
reviewed twice a year. Product requirement documents such as PRD-001 (checkout) and PRD-004
(seller payouts) should trace their goals back to a section of this page, and any proposal that
conflicts with the principles below needs an explicit exception from the executive team before
work starts.

## Mission {#mission}

Our mission is to make it possible for anyone who makes things by hand to earn a living from
their craft, and for buyers to find objects with a story behind them. Mass-produced marketplaces
optimise for the cheapest unit and the fastest shipping. We optimise for the match between a
maker and a buyer who will value the work. That means we treat sellers as small businesses, not
as inventory sources: we give them predictable fees, fast and honest payouts, and tools that
save them time rather than tools that extract more from them. For buyers it means trust: the
item in the photo is the item that arrives, it was made by the person whose name is on the shop,
and if something goes wrong we resolve it quickly and fairly.

## Our customers {#customers}

We serve two groups and we cannot succeed with one at the expense of the other.

Sellers are mostly sole traders and micro-businesses with one to five people. About 70 percent
run their shop part time, alongside another job. Their main pains are discovery (being found at
all), cash flow (waiting for money), and administration (taxes, identity checks, shipping
labels). The median seller lists 23 items and makes 41 sales a year.

Buyers are typically 25 to 55, buy gifts more often than items for themselves, and are willing
to pay a premium for uniqueness, but they are not willing to tolerate a checkout that fails or a
parcel that never arrives. Repeat buyers account for 58 percent of gross merchandise value, so
the experience after the first purchase matters as much as acquisition. We also serve a growing
segment of small retailers who buy wholesale from makers; they are a secondary customer for now.

## Three-year goals {#three-year-goals}

By the end of 2029 we commit to the following outcomes, measured against the 2026 baseline:

- Gross merchandise value grows from 410 million USD to 1.2 billion USD a year.
- Active buyers (at least one purchase in the trailing twelve months) grow from 3.1 million to
  8 million.
- Active sellers grow from 62,000 to 150,000, with at least 40 percent outside North America.
- Median time from a seller's sign-up to their first sale falls from 21 days to 10 days (see
  PRD-003).
- Checkout conversion on mobile rises from 2.4 percent to 3.5 percent (see PRD-001).
- Sellers receive their money in two business days or less for 90 percent of payouts (see
  PRD-004).

These goals are deliberately balanced between the two sides of the marketplace. A plan that hits
the GMV number by squeezing sellers, or the seller number by flooding search with low-quality
listings, does not meet the goals.

## Product principles {#product-principles}

Five principles settle most product arguments before they start.

1. Money is sacred. A buyer is never charged twice, a seller is never paid the wrong amount, and
   every movement of money can be explained line by line. When reliability and speed of delivery
   conflict on a money path, reliability wins.
2. Makers first in search. Ranking rewards relevance and craft quality, never paid placement
   disguised as organic results.
3. Fewer, better notifications. We contact people when it helps them, not when it helps our
   engagement chart.
4. Boring infrastructure. We prefer proven, well-understood technology that the team can operate
   at three in the morning over novel technology that looks good in a conference talk.
5. Explain decisions. Every significant technical decision is written down as an ADR so that a
   new engineer can learn why the system looks the way it does without asking around.

## What we will not do {#non-goals}

Saying no is part of the strategy. Over the next three years we will not:

- Sell mass-produced or drop-shipped goods, even when a category would grow GMV quickly. Listing
  quality rules exist to keep these out, and the seller team enforces them.
- Run our own warehouses or fulfilment network. Sellers ship their own goods; we help with labels
  and tracking but we do not hold inventory.
- Offer buy-now-pay-later financing from our own balance sheet. If we add instalments, a licensed
  partner carries the credit risk.
- Build a general advertising network that shows third-party ads to buyers.
- Change seller fees without at least 60 days of written notice.

Proposals that cross one of these lines should be raised with the executive team as a change to
this document rather than argued inside a single product requirement document.

## How we measure success {#measuring-success}

We track one north-star metric and a small set of guardrails. The north-star metric is the number
of sellers who earned at least 500 USD in the trailing ninety days, because it captures both
sides at once: it only grows when buyers are buying and sellers are getting paid.

Guardrails that must not get worse while we chase the north star:

- Buyer net promoter score, surveyed after delivery, at or above 55.
- Order defect rate (not delivered, not as described, or charged incorrectly) below 1.5 percent.
- Seller payout complaints below 3 per 10,000 payouts.
- Search sessions that end without a click below 30 percent.

Each team owns at least one input metric that rolls up into the north star, and quarterly
planning starts from those numbers rather than from feature lists.

## Acquisition targets {#acquisition-targets label=restricted groups=exec}

This section is restricted to the executive team. Project Halcyon is the working name for a plan
to acquire Loomwright, a small competitor focused on hand-woven textiles with about 9,000 active
sellers and 38 million USD in annual GMV. Loomwright's sellers overlap with ours by roughly 15
percent, and its textile category is one where our listing quality and search relevance are
weakest.

The board approved exploratory talks in September 2026. The current valuation range under
discussion is 55 to 70 million USD, paid two thirds in cash and one third in stock, with a
retention package for Loomwright's four-person engineering team. Integration would migrate their
sellers onto our onboarding and payout flows within nine months of closing, keeping their brand
as a curated collection inside our search. The main risks are seller churn during migration and
a competing bid. Nobody outside the executive team and the board should be told about Project
Halcyon until a term sheet is signed; do not reference it in tickets, PRDs or RFCs.
