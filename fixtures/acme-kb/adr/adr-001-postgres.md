---
id: ADR-001
type: adr
status: approved
owner: "@team-platform"
review_by: 2027-06-30
summary: Records the decision to use PostgreSQL as the system of record for every service, with one database per service, managed hosting, and strict rules for schema changes and cross-service access.
code: ["infra/postgres/**", "services/*/migrations/**"]
tags: [platform, database]
---

# PostgreSQL as the system of record

This decision applies to every service that stores business data: orders, listings, sellers,
payments and the ledger. It was taken in 2023, when the platform was split out of the original
monolith, and it is the foundation that later decisions such as the event bus in ADR-002 and the
ledger design in RFC-013 build upon. It is owned by the
platform team, which runs the managed database fleet and reviews every new database request.

## Context {#context}

The original monolith used a single MySQL database with about 400 tables, shared by every part of
the code. Schema changes required coordination across all teams, long-running reports slowed down
checkout, and several incidents came from one team's migration locking tables another team relied
on. As we split the monolith into services, we needed to choose the database that each service
would use for its own data.

The requirements were strong transactional guarantees for money (serialisable transactions where
needed, reliable constraints), mature tooling for online schema changes, good support for JSON
documents for semi-structured listing attributes, and a managed offering on our cloud provider so
that a small platform team could run dozens of databases. The team also valued an ecosystem of
extensions and a large hiring pool of engineers who already know the database.

## Decision {#decision}

Every service that owns business data uses PostgreSQL as its system of record, one logical
database per service, on the cloud provider's managed PostgreSQL offering. No service reads or
writes another service's database; data crosses service boundaries only through APIs or events.

Schema changes are applied by migrations stored next to the service code, reviewed like any other
change, and must be backward compatible with the currently running version of the service (expand
first, contract in a later release). Money columns follow ADR-006. Each database has a read
replica for reporting and analytics, and nightly snapshots are retained for 35 days with
point-in-time recovery for the last 7 days.

## Consequences {#consequences}

Teams can change their schema without coordinating with others, and an expensive query in one
service can no longer slow down checkout. Strong constraints, such as unique idempotency keys in
ADR-005 and balanced entries in the ledger, are enforced by the database rather than only by code.

The costs are real. Queries that used to be a join across domains now need an API call, an event
or a read model; search, for instance, maintains its own index fed by events (RFC-011). We run
more database instances, about 30 today, which increases baseline cost and the platform team's
operational load. Cross-service reports must be built in the data warehouse rather than with ad
hoc SQL. Engineers must learn to design for eventual consistency between services.

## Alternatives considered {#alternatives}

- Keep MySQL and split it into per-service schemas. This would have avoided a migration, but
  online schema change tooling was weaker for our use, and the team preferred PostgreSQL's
  constraint and JSON support.
- A distributed SQL database. Attractive for horizontal scale, but our largest service handles
  well under the write volume of a single large PostgreSQL instance, and the operational
  experience on the team was limited.
- A document database for listings. Flexible for attributes, but we would have needed a second
  database technology for money, and PostgreSQL's JSON columns cover the listing use case.
