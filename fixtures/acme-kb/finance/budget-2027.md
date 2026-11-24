---
id: FIN-001
type: prd
status: draft
owner: "@team-finance"
review_by: 2027-01-31
summary: Draft 2027 operating budget for Acme Market, covering the headcount plan, infrastructure spend, marketing spend and the main financial risks to the plan.
tags: [finance, budget]
---

# Budget 2027

This is the draft operating budget for 2027, prepared by finance for the January board meeting.
It assumes GMV grows from 410 million USD in 2026 to 560 million USD in 2027, on the path to the
1.2 billion USD three-year goal, and that net revenue reaches 41 million USD. Total operating
expenses are planned at 46 million USD, so the company remains loss-making in 2027 by about 5
million USD, with break-even planned for the third quarter of 2028. Figures are in USD.

## Headcount plan {#headcount-plan}

Headcount grows from 212 at the end of 2026 to 268 at the end of 2027, a net increase of 56. Fully
loaded personnel cost is budgeted at 27.4 million USD, 60 percent of operating expenses.

- Engineering grows from 96 to 124. The largest additions are payments (+8, for the payout ledger
  and instant payouts), search (+6, including two machine learning engineers for ranking), and
  platform (+5, for Kafka and database operations). Sellers and growth add 4 each, checkout adds 1.
- Product and design grow from 22 to 28.
- Trust, safety and support grow from 54 to 66, mostly support agents in two new time zones.
- Finance, legal and people grow from 20 to 26, including a second treasury analyst and an
  in-house privacy lawyer.
- Marketing grows from 20 to 24.

Hiring is weighted to the first half: 60 percent of new roles must be filled by the end of June.
Any role not filled by September is released back to the budget pool.

## Infrastructure spend {#infrastructure-spend}

Cloud and third-party infrastructure is budgeted at 7.8 million USD, up from 6.1 million USD in
2026, an increase of 28 percent against GMV growth of 37 percent.

- Compute for services: 2.9 million USD, assuming the move to newer instance families saves 12
  percent from April.
- Managed PostgreSQL, about 30 databases with replicas: 1.6 million USD.
- Kafka and the schema registry: 0.7 million USD.
- The OpenSearch cluster: 0.9 million USD, including the dedicated indexing nodes for rebuilds.
- Observability (metrics, logs, traces): 0.8 million USD, with a target to cut log volume by 25
  percent through sampling.
- Third-party services, including the KYC vendor at about 1.40 USD per check, the push provider
  and email delivery: 0.9 million USD.

Infrastructure cost per order should fall from 0.21 USD to 0.19 USD.

## Marketing spend {#marketing-spend}

Marketing is budgeted at 9.5 million USD, up from 7.2 million USD.

- Paid acquisition of buyers: 5.2 million USD, with a blended cost per new active buyer capped at
  14 USD and a payback period under nine months. Channels are search ads (55 percent), social
  (35 percent) and affiliates (10 percent).
- Seller acquisition: 1.8 million USD, focused on Europe and Australia, where seller growth is
  furthest behind the 40 percent outside North America goal. The target cost per seller who
  makes a first sale is 120 USD.
- Brand and content, including the maker stories programme and two seasonal campaigns: 1.6
  million USD.
- Lifecycle and retention tools: 0.9 million USD.

Spend is released quarterly against results; if cost per new active buyer exceeds 16 USD for two
consecutive months, paid acquisition is cut by a quarter until it recovers.

## Risks {#risks}

The main risks to this budget are:

- GMV growth below 30 percent. Every 5 points of shortfall reduces net revenue by about 1.5 million
  USD; the mitigation is the quarterly release of marketing spend and the hiring deadline.
- Payment processing costs. The processor contract renews in the second half; a rise of 0.1 points
  in the blended rate would cost about 0.6 million USD.
- Currency movements. About 35 percent of GMV is outside the USD zone; a 10 percent stronger dollar
  reduces revenue by about 1.4 million USD.
- Seller churn from fee changes. Any change to seller fees must be announced 60 days in advance and
  could reduce new seller sign-ups.
- Regulatory cost. New marketplace reporting rules in Europe may require additional seller data
  collection and an estimated 0.4 million USD of engineering and compliance work.
