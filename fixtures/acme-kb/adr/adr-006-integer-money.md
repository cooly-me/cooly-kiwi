---
id: ADR-006
type: adr
status: approved
owner: "@team-payments"
review_by: 2027-06-30
summary: Decides that every monetary amount is stored, transmitted and computed as an integer number of minor units with an explicit ISO 4217 currency code, never as a floating-point number.
code: ["libs/money/**", "services/ledger/**", "services/payment/**"]
tags: [payments, money]
---

# Money as integer minor units

This decision fixes how money is represented everywhere on Acme Market: in databases, in APIs, in
events on the bus and in code. It applies to every service and is a precondition for the
double-entry ledger in [RFC-013](../rfcs/rfc-013-payout-ledger.md) and the payout requirements in
PRD-004. Any new service that
handles prices, fees, refunds or balances must use the shared money library from its first
commit.

## Context {#context}

Before this decision, amounts were represented inconsistently. Some tables used floating-point
columns, some used decimal columns with two places, the checkout API sent strings such as
"12.50", and the payouts code multiplied floats by fee percentages. Rounding differences of one
cent appeared in roughly one in 3,000 payouts, and the monthly reconciliation with the processor
regularly showed small unexplained differences that took finance days to trace.

Two decimal places are also wrong for several currencies we pay out in: the Japanese yen has no
minor unit, and some currencies have three decimal places. Fee calculations that round at
different points in different services produced totals that did not add up across a basket split
into several orders.

## Decision {#decision}

All monetary amounts are integers counting the currency's minor unit (cents for USD, pence for
GBP, yen for JPY), stored as 64-bit integers, and always paired with an ISO 4217 currency code.
APIs and events carry an object such as `{"amount": 1250, "currency": "USD"}`; a bare number is
never a valid amount.

Arithmetic happens in a shared money library. Percentage fees are computed with integer maths and
a single, documented rounding rule: round half to even, applied once per line, at the point the
amount is recorded. Allocation of a total across lines, for example a discount over several
items, uses the largest-remainder method so that the parts always sum exactly to the total.
Floating-point types are forbidden for money by a lint rule. Currency conversion produces a new
amount in the target currency and records the rate used; amounts in different currencies are
never added together.

## Consequences {#consequences}

Rounding errors disappeared from payouts, and every total can be reproduced exactly from its
parts, which the ledger relies on to check that each entry balances to zero. Supporting a currency
with no minor unit or with three decimal places required no code change beyond configuration.

The migration took two quarters: every money column, API field and event schema had to change,
with a compatibility period where events carried both formats. Display code must format amounts
per currency and locale instead of printing numbers, and engineers new to the codebase sometimes
forget that 1250 means 12.50 USD; the money library's types make that mistake hard to compile.
Sixty-four-bit integers limit a single amount to about 92 quadrillion minor units, far beyond any
realistic need.

## Alternatives considered {#alternatives}

- Decimal columns and decimal types in code. Precise, but behaviour differs across languages and
  JSON libraries, and it still leaves the number of decimal places per currency implicit.
- Floating-point with careful rounding. Rejected: the bugs that motivated this decision came from
  exactly that approach.
- Strings in APIs with decimals in storage. Avoids precision loss in transit but pushes parsing
  and validation into every client.
