---
id: LEGAL-001
type: adr
status: review
owner: "@team-legal"
review_by: 2027-02-28
summary: Records the legal team's position and open points in negotiating a data processing agreement with the Stripe-like processor that handles card payments for Acme Market checkout.
access: { label: confidential, groups: [legal] }
tags: [legal, privacy, payments]
---

# Data processing agreement with the payment processor

Acme Market is renegotiating its data processing agreement with the Stripe-like processor that
handles card authorisation and capture for checkout. The current agreement dates from 2023, before
the asynchronous capture design and before we started paying sellers in 14 countries. This record
sets out our negotiating position, what the processor has offered, and the points still open. It
is confidential to the legal team because it contains our fallback positions.

## Scope and roles {#scope}

Our position is that the processor acts as a processor on our behalf for checkout data (buyer
name, billing address, email, card token and order amounts) and as an independent controller only
for its own fraud screening and regulatory obligations. The processor's first draft claimed
controller status for all transaction data, which would let it use our buyers' data to train
models shared with other merchants.

We have proposed a split: the processor may use aggregated, de-identified signals for fraud
detection across its network, but no buyer-level data leaves our account for any other purpose.
Our fallback, if they refuse, is to accept controller status for fraud screening only, with a
written list of the data fields involved and a commitment that marketing use is excluded.

## International data transfers {#data-transfers}

Buyer data from Europe and the United Kingdom is processed by the processor in the United States.
The agreement must include the standard contractual clauses with the module for controller to
processor transfers, and a transfer impact assessment covering access by public authorities.

The processor has offered regional processing in Europe for card data at no extra cost from the
second half of 2027, but not for its fraud models. We want a commitment that any data processed
outside the region is limited to what fraud screening needs, encrypted in transit and at rest,
and subject to a notification obligation if the processor receives a government request, unless
the law forbids it. Our engineering teams have confirmed that the card token design in PRD-001
means no full card numbers ever pass through our systems, which simplifies our side.

## Sub-processors {#sub-processors}

The processor lists 31 sub-processors, including cloud hosting, identity verification and
customer support vendors. The current agreement allows it to add sub-processors with 10 days'
notice and no right to object. We have asked for 30 days' notice, a right to object on reasonable
data protection grounds, and the right to terminate without penalty if an objection is not
resolved within 60 days.

The processor has accepted 30 days' notice and the right to object, but wants termination to be
limited to the affected service. That is acceptable to us for services other than card processing
itself. We also asked for the sub-processor list to be published at a stable URL with a change
feed, so the privacy team can track changes without relying on email.

## Breach notification and audit {#breach-notification}

The draft requires the processor to notify us of a personal data breach "without undue delay". We
need a fixed outer limit of 36 hours, so that we can meet our own 72-hour obligation to regulators
with time to investigate. The processor has offered 48 hours; our fallback is 48 hours with an
initial notice within 24 hours even if facts are incomplete.

On audit, the processor will provide its annual independent security reports and answer one
questionnaire a year, but refuses on-site audits. We accept that, provided we may commission an
independent audit after a breach affecting our data, at the processor's cost if the audit finds a
material failure. Liability for data protection breaches is capped in the draft at twelve months
of fees; we are asking for a separate, higher cap of three times annual fees.
