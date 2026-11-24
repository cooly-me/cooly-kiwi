---
id: RFC-012
type: rfc
status: review
owner: "@team-sellers"
review_by: 2027-02-15
summary: Proposes a separate KYC verification service that runs seller identity checks through an external vendor, isolates personal documents from the main database, and unblocks payouts when a seller passes.
implements: [PRD-003]
depends_on: [ADR-001, ADR-002]
code: ["services/kyc/**"]
tags: [sellers, kyc, compliance]
---

# KYC verification service

[PRD-003](../product/sellers/prd.md) requires that every seller passes an identity check before
receiving money, with 80 percent decided automatically within 10 minutes. Today verification is a
set of tables and scripts inside the seller service, with document images stored in the same
bucket as listing photos. This RFC proposes a dedicated verification service that owns the whole
process and the sensitive data it collects, and that tells the rest of the platform only whether a
seller is verified.

## Summary {#summary}

A new service in `services/kyc` owns verification cases. The seller dashboard starts a case by
calling the service, which creates a session with an external verification vendor and returns a
link to the vendor's capture flow, where the seller photographs their ID and takes a selfie. The
vendor calls back with a result; the service stores the decision and publishes a
`SellerVerified` or `SellerVerificationFailed` event on Kafka.

The payout service listens for these events and keeps its own copy of each seller's verification
status, which it checks before every payout run. No other service reads personal documents or
the raw vendor result. The service has its own PostgreSQL database, following ADR-001, on a
separate cluster with stricter access controls than the rest of the platform.

## Verification flow {#verification-flow}

A case moves through `created`, `submitted`, `in_review`, `approved`, `rejected` or `expired`.

1. The seller starts verification from the onboarding checklist. The service creates a case and a
   vendor session valid for 24 hours.
2. The seller completes capture with the vendor. The vendor runs document authenticity checks,
   face matching, and sanctions and politically exposed person screening.
3. The vendor sends a webhook with a decision: approve, reject, or refer. Approve and reject are
   final for that attempt. Refer moves the case to `in_review` for a human reviewer, with a target
   of one business day.
4. On rejection the seller sees which check failed, mapped to plain language, and may retry twice;
   the third failure escalates to the trust team.

For business sellers the case also collects the registration number and beneficial owners above
25 percent ownership, each of whom goes through the same individual check.

## Vendor integration {#vendor-integration}

We will integrate one vendor behind an adapter interface, so that a second vendor can be added
for countries the first covers poorly. The adapter has three operations: create a session, fetch
a result, and delete a subject's data. Webhooks are authenticated with an HMAC signature and
processed idempotently on the vendor's event id, since vendors retry deliveries.

The service never trusts a webhook body alone: after receiving one, it fetches the result from
the vendor's API before recording a decision. If the vendor is unavailable, sessions cannot be
created and the onboarding checklist shows that verification is temporarily unavailable; sellers
can continue listing, because "can list" and "can be paid" are separate in PRD-003. Vendor cost is
about 1.40 USD per check, so a seller who abandons and restarts reuses the open session rather
than creating a new paid one.

## Data retention {#data-retention}

Identity documents and selfies are the most sensitive data the platform holds. The service stores
images only in a dedicated encrypted bucket with a per-object key, readable only by the service
and by reviewers through an audited viewer. Images are deleted 30 days after a final decision;
afterwards we keep only the decision, the checks performed, the document type and country, and
the vendor's reference.

Regulations require keeping verification records for five years after the seller relationship
ends, which the retained metadata satisfies. A seller who closes their shop and asks for deletion
has their personal data removed except for what that retention rule requires, and the vendor's
delete operation is called. Access to the service's database is limited to the sellers team's
on-call engineers, with every query logged.

## Open questions {#open-questions}

- Do we re-verify sellers periodically, for example every three years, or only when their bank
  details or legal name change?
- Should reviewers be in-house, or should we use the vendor's review service at about 4 USD per
  referred case?
- How do we handle sellers in countries the vendor does not support? A manual document review is
  possible but would not meet the 10-minute target.
- Is a single vendor an acceptable dependency for launch, given that an outage blocks payouts for
  new sellers?

This RFC remains in review until the seller team and legal agree on retention and periodic
re-verification.
