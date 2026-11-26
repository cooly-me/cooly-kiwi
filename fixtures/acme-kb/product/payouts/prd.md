---
id: PRD-004
type: prd
status: approved
owner: "@team-payments"
review_by: 2027-03-01
summary: Requirements for paying sellers on Acme Market, covering the payout schedule, holds and reserves, disputes and chargebacks, the ledger the payouts rely on, and the confidential fee schedule.
decided_by: [ADR-006]
tags: [payments, payouts, sellers]
---

# Seller payouts

Getting paid is the moment a seller decides whether Acme Market is worth their time. Today
sellers wait a median of five business days, payouts sometimes arrive split in ways nobody can
explain, and the support team handles about 1,200 "where is my money" tickets a month. This
document defines how payouts must work. The double-entry ledger that records every movement of
money is designed in [RFC-013](../../rfcs/rfc-013-payout-ledger.md), and amounts are stored as
integer minor units following ADR-006.

## Goals {#goals}

- Pay 90 percent of payouts within two business days of the funds becoming available, the
  three-year goal in the vision.
- Make every payout explainable: a seller can open any payout and see the orders, refunds, fees
  and adjustments that add up to it, to the cent.
- Reduce payout-related support tickets by 60 percent.
- Never pay a seller twice for the same order and never lose track of money owed to a seller,
  even when a bank transfer fails.

Non-goals: instant payouts to debit cards (planned next year) and paying sellers in
cryptocurrency.

## Payout schedule {#payout-schedule}

Funds from an order become available to the seller when the order is marked as shipped with a
tracking number, or 7 days after the order date for items without tracking. Available funds are
paid out on a schedule the seller chooses: daily, weekly (every Tuesday, the default), or monthly
(the first business day). A payout runs only if the available balance is at least 10 USD or the
local equivalent; smaller balances roll into the next run.

Payouts go to the seller's verified bank account in their own currency. Sellers in 14 countries
are supported, matching the identity verification coverage in PRD-003, because a seller cannot
receive a payout until verification passes. A payout that the bank rejects, for example because
the account was closed, returns to the seller's balance within three business days and the seller
is asked to update their bank details; we never retry a rejected payout to the same account
automatically.

## Holds and reserves {#holds}

Some money must wait before it is paid out, to protect buyers and the platform against refunds
and chargebacks.

- New sellers: funds from the first 10 orders or the first 30 days, whichever comes later, are
  held until 7 days after delivery.
- Risk holds: if a shop's dispute rate exceeds 1 percent over the trailing 90 days, a rolling
  reserve keeps a share of each payout for 30 days. The seller sees the reserve amount and its
  release date.
- Manual holds: the trust team can freeze payouts during a fraud investigation, with a reason
  shown to the seller and a review within five business days.

Every hold is recorded in the ledger as a movement between accounts, never as a flag on a
payout, so that the balance a seller sees always matches what we owe.

## Disputes {#disputes}

When a buyer opens a dispute with Acme Market, the disputed amount moves from the seller's
available balance into a dispute account until the case is resolved. If the balance is
insufficient, the amount is taken from the next payouts rather than leaving the seller negative
immediately. Sellers have 7 days to respond with evidence such as tracking or messages.

Chargebacks raised through the buyer's card issuer follow the same path, with the processor's
deadlines. If the seller wins, the funds return to their available balance in the next payout.
If the seller loses, the amount and any chargeback fee are debited. Sellers with a verified
tracking number showing delivery are protected against "item not received" claims, and in that
case the platform absorbs the loss. Dispute outcomes are visible on the payout detail page that
includes the affected order.

## Ledger requirements {#ledger-requirements}

Payouts are computed from a ledger, never from order tables. The requirements are:

- Double entry: every movement debits one account and credits another for the same amount, so
  the sum over all accounts is always zero.
- Append-only: entries are never updated or deleted; a correction is a new reversing entry.
- Integer amounts in minor units with an explicit currency on every entry, as ADR-006 decides.
  Conversions between currencies are separate entries recording the rate used.
- Every entry references its cause (order, refund, dispute, payout, fee) and an idempotency key,
  so replaying an event cannot post it twice.
- A seller's balance at any point in time can be reconstructed from the entries alone.
- Daily reconciliation against the processor's settlement reports and the bank statement must
  balance to zero difference or raise an alert.

## Fee schedule {#fee-schedule access=finance}

This section is confidential to finance. Payout fees charged to sellers are:

- Domestic payouts in USD, GBP, EUR, CAD and AUD: free on the weekly and monthly schedules; 0.25
  USD per payout on the daily schedule.
- Payouts to other currencies: 0.50 USD per payout plus the FX margin.
- FX margin: 1.0 percent above the mid-market rate for the major currencies, 1.5 percent for
  others. Our partner's cost averages 0.35 percent, so FX contributes about 3.4 million USD a year
  at 2026 volumes.
- Returned payouts (rejected by the seller's bank): 2.00 USD, waived for the first occurrence.
- Chargeback fee passed to sellers who lose a dispute: 15.00 USD, matching the processor fee.

Finance reviews these fees each January. Any change requires 60 days of notice to sellers, the
same rule that applies to the platform take rate.
