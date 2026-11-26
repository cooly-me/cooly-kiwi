---
id: RFC-013
type: rfc
status: approved
owner: "@team-payments"
review_by: 2027-03-01
summary: Designs the append-only double-entry ledger from which seller balances and payouts are computed, including the chart of accounts, the payout run, reconciliation with the processor and bank, and failure modes.
implements: [PRD-004]
decided_by: [ADR-006]
depends_on: [ADR-001, ADR-002]
code: ["services/ledger/**", "services/payouts/**"]
tags: [payments, payouts, ledger]
---

# Double-entry payout ledger

[PRD-004](../product/payouts/prd.md) requires that every payout can be explained to the cent and
that money owed to a seller is never lost. Today seller balances are computed by summing order
tables at payout time, which breaks whenever a refund, dispute or fee is recorded in a different
table. This RFC introduces a double-entry ledger as the only source for balances and payouts.
Amounts are integers in minor units, as [ADR-006](../adr/adr-006-integer-money.md) decides.

## Summary {#summary}

The ledger is a service in `services/ledger` with its own PostgreSQL database. It consumes money
events from Kafka (captures from the checkout saga in RFC-010, refunds, dispute changes, fee
assessments, payout results) and turns each into a balanced journal entry. It exposes balances
and entry history through an API used by the payout service, the seller dashboard and finance.

Entries are append-only: there is no update or delete path in the code or in the database grants.
A mistake is fixed with a reversing entry and a new correct entry, both pointing at the original.
Each journal entry carries the idempotency key of its source event, with a unique constraint, so
replaying a Kafka topic cannot post the same money twice.

## Accounts and entries {#accounts-and-entries}

Each seller has a set of accounts per currency: `pending` (captured but not yet available),
`available`, `reserved` (risk holds), `in_dispute`, and `in_transit` (payout sent, not yet
confirmed by the bank). The platform has accounts for processor receivables, platform revenue,
FX revenue, chargeback losses and bank cash.

A capture of a 100.00 USD order with the platform fee posts one entry: debit processor
receivable 10000, credit seller pending for the subtotal net of fee, and credit platform revenue
for the fee, in minor units. When the order ships, an entry moves the amount from pending to
available. A journal entry is rejected unless its debits and credits sum to zero per currency.
Currency conversion is two entries, one per currency, linked by a conversion id and the rate.

## Payout run {#payout-run}

The payout service runs a payout job per schedule (daily, weekly on Tuesday, monthly). For each
eligible seller it reads the available balance as of the job's cut-off time, checks that the
seller is verified and that no manual hold applies, and creates a payout with an idempotency key
of seller id, currency and cut-off date.

Creating the payout posts an entry from `available` to `in_transit` before the bank instruction
is sent, so the money cannot be paid twice even if the job crashes and restarts. The bank
instruction is then submitted in a batch file. When the bank confirms, `in_transit` moves to bank
cash; if the bank rejects the payout, a reversing entry returns the amount to `available`, which
is the behaviour PRD-004 requires for rejected payouts.

## Reconciliation {#reconciliation}

The ledger is only trustworthy if it agrees with the outside world every day. Reconciliation
compares three sources: the ledger, the payment processor's settlement reports, and the bank
statement. Each daily run produces a report of matched items, unmatched items on each side, and
the net difference per currency. The target difference is zero; any unexplained difference above
1.00 USD opens a ticket for the payments on-call engineer, and any difference above 1,000 USD
pages them. The two subsections below describe the internal and external comparisons.

### Internal reconciliation {#internal-reconciliation}

Internal reconciliation checks the ledger against the systems that feed it. Every capture,
refund and dispute event recorded by the payment service must have exactly one journal entry,
matched on the idempotency key. A nightly job also verifies invariants: the sum of all accounts is
zero per currency, no seller's `available` balance is negative for more than 24 hours, and every
payout in `in_transit` older than five business days is flagged. These checks run against a
replica so that they never slow down posting.

### Bank reconciliation {#bank-reconciliation access=finance}

This subsection is confidential to finance. Payouts and platform cash are held at Northwind Bank,
our partner bank for USD, CAD and EUR settlement. Northwind Bank delivers a statement file every
business day at 06:00 UTC over SFTP and an intraday file at 14:00 UTC. Each payout batch is
matched on the batch reference we send; individual payouts are matched on amount and the
seller-specific end-to-end id.

Northwind Bank charges 0.08 USD per domestic payout line and holds a 2 million USD minimum
balance requirement in the settlement account, which treasury tops up from the processor
settlements every morning. Unmatched bank lines older than three days are escalated to the bank's
relationship manager. The contract renews in March 2027 and finance is evaluating a second partner
bank to reduce concentration risk.

## Failure modes {#failure-modes}

- Kafka redelivers an event: the unique idempotency key rejects the duplicate entry.
- The ledger is unavailable: producers keep publishing to Kafka and the ledger catches up from
  its last committed offset; payout runs are delayed rather than run on stale balances.
- The payout job crashes mid-run: on restart, payouts already moved to `in_transit` are found by
  their idempotency key and not created again.
- The bank file is late: payouts remain `in_transit`, the seller sees "on its way", and an alert
  fires after 24 hours.
- A posting bug creates wrong entries: they are corrected with reversing entries, never edited,
  so the audit trail shows both the mistake and the fix.
