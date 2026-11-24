---
id: STD-001
type: standard
status: approved
owner: "@team-platform"
review_by: 2027-06-30
summary: The quality bar every Acme service meets before and after it ships - code review, tests, service level objectives, observability, releases and how to get an exception.
decided_by: [ADR-005, ADR-006]
code: ["services/**", "libs/**"]
tags: [platform, quality, engineering]
---

# Engineering quality standard

This standard is the bar every service, library and data pipeline at Acme Market meets. It is
not a decision with alternatives, like an ADR, and not a design, like an RFC: it is a rule
that stays in force until the platform team changes it, and every team is held to it in code
review and in the quarterly service review. Where a rule rests on a decision, the decision is
named, so that changing the decision means revisiting this standard too.

## Scope {#scope}

The standard applies to every service that runs in production and to every shared library
those services import. Prototypes and internal tools without customer data may skip the
service level objective and release rules, but not the review and testing rules. A service
moves into scope the day it serves its first real customer request or stores its first real
customer record, whichever comes first; the owning team then has one quarter to meet every
rule below. Data pipelines that feed finance reporting are in scope even when they serve no
request, because reconciliation depends on them.

## Code review {#code-review}

Every change merges through a pull request approved by at least one engineer who did not write
it; changes to payments, ledger or payout code need two approvals, one of them from the
payments team. Reviewers answer within one working day. A pull request stays under about 400
changed lines so that it can be read whole; larger changes are split behind a feature flag. The
author states how the change was tested and links the ticket or document it implements. Nobody
approves their own change through a second account, and an approval is dismissed when new
commits arrive after it.

## Testing {#testing}

Business rules are covered by unit tests that run in under a minute, and every API and event
schema has a contract test against its consumers. Code that handles money uses the shared
money library and adds property tests for rounding and allocation, as ADR-006 requires. Every
write endpoint has a test that sends the same idempotency key twice and gets the original
response back, as ADR-005 requires. A bug fix starts with a test that fails without the fix.
Flaky tests are quarantined within a day and fixed or deleted within a week; a quarantined
test never blocks a release, and a quarantine older than a week is a review finding.

## Service level objectives {#slos}

Every user-facing service declares service level objectives for availability and latency and
measures them from the caller's side. Checkout targets 99.95 % availability over 28 days and a
p99 latency of 800 ms; search and seller tools target 99.9 % and 1.5 s. When a service spends
its whole error budget, the owning team freezes feature work on it and ships only reliability
fixes until the budget recovers. Objectives are reviewed every quarter with the product owner,
and a target is lowered only with a written reason.

## Observability {#observability}

Every service emits structured logs, request metrics (rate, errors, duration) and traces, and
propagates the trace id through every call and event it sends. Logs never contain card data,
bank details or full addresses. Each service has one dashboard with its objectives at the top.
Alerts page a person only for symptoms a customer would notice, such as errors or slow
responses, never for causes such as high CPU; everything else goes to a ticket queue that the
owning team reads every working day.

## Releases {#releases}

Releases go out through the deployment pipeline only: a canary takes 5 % of traffic for 30
minutes and is rolled back automatically when its error rate or latency is worse than the
running version. Database migrations are backward compatible with the running version, so a
rollback never needs a data fix. Payments services do not deploy on Fridays after 15:00 or
during the holiday freeze. Feature flags are removed within 90 days of reaching 100 %, and a
flag older than that is a review finding.

## Exceptions {#exceptions}

A team that cannot meet a rule asks for an exception in writing, stating which rule, why, the
risk it accepts and the date it will comply. The platform lead approves or refuses it within
a week. An exception expires after 90 days and is renewed only once; a rule that needs a
second renewal is either wrong for that service or a sign the service needs help, and the
platform team takes it to the quarterly service review.
