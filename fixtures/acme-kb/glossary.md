---
id: GLOSSARY
type: glossary
status: approved
owner: "@team-platform"
review_by: 2027-06-30
summary: Definitions of the terms Acme Market's product and engineering documents use, with the aliases people commonly use for each, so that a search for any alias finds the right term.
tags: [glossary]
---

# Glossary

This glossary defines the words that appear across Acme Market's vision, product requirement
documents, RFCs and ADRs. Each entry starts with the aliases people use for the same idea, so that
a reader who knows the term by another name can still find it. When a document introduces a new
term that others will reuse, add it here in the same pull request.

## Basket {#basket}

Aliases: cart, shopping cart

The list of items a buyer intends to buy, possibly from several sellers. A basket holds the
listing, variant, quantity and the price shown when the item was added; it reserves nothing.
At checkout it is re-priced and split into one order per seller, as PRD-001 describes.

## Capture {#capture}

Aliases: charge capture, payment capture

The step that actually moves money from the buyer's card after an authorisation has reserved it.
Acme Market captures asynchronously as a step of the checkout saga, normally within seconds of
the order being confirmed, as ADR-007 decides.

## GMV {#gmv}

Aliases: gross merchandise value

The total value of goods sold through the marketplace in a period, including item prices and
shipping charged to buyers, before refunds, fees and taxes are removed. GMV measures marketplace
activity, not company revenue; the three-year GMV goal is in the vision.

## Take rate {#take-rate}

Aliases: commission, platform fee

The share of each order that Acme Market keeps as its revenue, deducted from the seller's
proceeds before payout. It is expressed as a percentage of the item subtotal plus a fixed amount
per order. The current figures are owned by finance and are not published in this glossary.

## Seller {#seller}

Aliases: merchant, maker, vendor

A person or small business that lists handmade goods in a shop on Acme Market and fulfils the
orders. A seller can list once their email and phone are confirmed but is paid only after
passing identity verification, as PRD-003 requires.

## Payout {#payout}

Aliases: settlement, disbursement

A transfer of a seller's available balance to their bank account. Payouts run daily, weekly or
monthly at the seller's choice and are computed from the double-entry ledger, never from order
tables. PRD-004 sets out the schedule and holds.

## KYC {#kyc}

Aliases: know your customer, identity verification

The checks that confirm a seller is who they claim to be: a government ID, a selfie match, and
screening against sanctions lists, plus registration and beneficial owners for businesses. Passing
KYC unlocks payouts. The verification service is proposed in RFC-012.

## Saga {#saga}

Aliases: saga pattern, distributed transaction

A way to run a business process that spans several services as a sequence of local transactions,
each with a compensating action that undoes it if a later step fails. Acme Market's checkout is
an orchestrated saga over Kafka, described in RFC-010.

## Idempotency key {#idempotency-key}

Aliases: dedupe key, request key

A unique value the client generates once per logical write operation and sends with every retry
of it. The server stores the key with the response for 24 hours and replays that response for a
repeat, so a retry never creates a second order or charge. ADR-005 makes it mandatory.

## Ledger {#ledger}

Aliases: double-entry ledger, books

The append-only record of every movement of money on the platform, where each entry debits one
account and credits another for the same amount. Seller balances, payouts and reconciliation are
all derived from it. RFC-013 describes its design.

## Quiet hours {#quiet-hours}

Aliases: do not disturb

The period, 22:00 to 08:00 in the recipient's local time by default, during which marketing
notifications are held and non-urgent pushes are deferred. Urgent security messages are still
delivered. PRD-005 defines the rules and RFC-014 enforces them.

## Listing {#listing}

Aliases: product page, item

A single item or product offered for sale in a seller's shop, with its photos, title,
description, materials, price, variants and shipping profile. Listings receive a quality score at
publication that search uses for ranking, as PRD-003 and PRD-002 describe.
