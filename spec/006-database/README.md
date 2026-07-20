# Volume 6 — Database & Logical Data Model

**Status:** REVIEW · **Version:** 0.4.0 · **Subsystem tag:** DB

This volume specifies the **logical** persistence model for the CommOS Control
Plane: the relations, keys, constraints, isolation, partitioning, retention, and
migration rules that hold the durable state of the entities frozen in
[Volume 2](../002-domain-model/README.md). It is the storage counterpart to the
event model of [Volume 5](../005-events/README.md): the transactional outbox
defined here (§7) is what makes at-least-once delivery (CMOS-05-EVT-010) true.

Structured state lives in PostgreSQL; large artifacts live in Object Storage as
references, never blobs (CMOS-00-ENG-007). **PostgreSQL 15+ is the reference**
engine and every requirement below is expressed against portable relational
concepts — this volume is *logical*, not vendor DDL. Physical DDL, index tuning,
and the SQLx/migration tooling are implementation detail (CMOS-00-ENG-014, N-6).

Companion: [`schema-overview.md`](schema-overview.md) — table-by-table listing of
the core relations.

> Note (informative): "logical" means this volume constrains *what must be true of
> the data* (keys, constraints, isolation, history), not *which B-tree you build*.
> An implementation MAY use a different engine if it satisfies every requirement
> here and the contracts it serves.

---

## 1. Scope

In scope: relational shape of the ~20 core entities, primary/foreign keys,
uniqueness and check constraints, tenant isolation, time+tenant partitioning of
high-volume relations (CDR, events, audit), soft-delete and retention, the
transactional outbox, and the forward-only migration discipline.

Out of scope: Object Storage internals (Volume 3 §5), the ephemeral
Redis/NATS-class layer (registrations, presence, locks, bus cursors —
CMOS-03-ARCH-010), rating tables (Volume 10), and physical DDL/tuning.

- **CMOS-06-DB-001** Durable, structured, tenant-owned state MUST be persisted in
  the reference relational store; distributed *ephemeral* state (live
  registrations, presence, locks, event-bus cursors) MUST NOT be the system of
  record and MUST be reconstructable from durable state + events. (Serves
  CMOS-00-ENG-007, CMOS-03-ARCH-010, CMOS-00-ENG-015.)
- **CMOS-06-DB-002** The database is an implementation surface, not a public
  contract: no external client depends on table shape. The API (Volume 4) and
  events (Volume 5) are the contract; schema MAY evolve freely behind them
  provided those contracts hold. (Serves CMOS-00-ENG-003.)

## 2. Universal row model (normative)

Every persisted entity relation carries the common columns below, mirroring the
domain envelope ([entities.md](../002-domain-model/entities.md)).

| Column | Type class | Notes |
|--------|-----------|-------|
| `id` | uuid (v7) | Primary key. Time-ordered. |
| `tenant_id` | uuid | Owning Organisation; isolation axis. |
| `version` | bigint | Monotonic Digital Twin counter (CMOS-02-DOM-005). |
| `state` | text/enum | Where the entity has a state machine. |
| `created_at` / `updated_at` | timestamptz | UTC, millisecond precision. |
| `deleted_at` | timestamptz null | Soft-delete tombstone (§6); `NULL` ⇒ live. |

- **CMOS-06-DB-010** Every entity table's primary key MUST be a UUIDv7 `id`
  (CMOS-CONV-010). Natural keys (Extension number, DID E.164, MAC, slug) MUST NOT
  be primary keys; they are enforced as uniqueness constraints (§4). (Serves
  CMOS-00-ENG-002 — a dialable label is never an identity.)
- **CMOS-06-DB-011** Time-ordered UUIDv7 primary keys SHOULD be used directly as
  the clustering/insertion order so that `id` ordering approximates creation
  order without a separate sequence.
- **CMOS-06-DB-012** Every entity table MUST carry a non-null `tenant_id`, except
  the `organisation` root where `tenant_id = id` (CMOS-02-DOM-001).
- **CMOS-06-DB-013** `updated_at` MUST advance and `version` MUST increment by
  exactly one on every materially-observable mutation, in the same transaction as
  the mutation (CMOS-02-DOM-005). Optimistic concurrency MUST be enforced by
  compare-and-set on `version`; a stale write MUST fail, not clobber.
- **CMOS-06-DB-014** A `state` column MUST only ever hold a value in that entity's
  state-machine enumeration ([state-machines.md](../002-domain-model/state-machines.md));
  transitions are validated in the application layer (CMOS-02-DOM-007) and MAY be
  additionally guarded by a check constraint. Illegal states MUST be
  unrepresentable or rejected.

## 3. Tenant scoping & row-level isolation (normative)

Isolation is the load-bearing property; it is enforced at the data layer and
re-checked above it (defence in depth, CMOS-03-ARCH-050).

- **CMOS-06-DB-020** Every query against a tenant-owned relation MUST be
  constrained by `tenant_id`. The reference implementation MUST enforce this with
  **row-level security (RLS)** keyed to a per-connection/transaction tenant
  context, so a missing `WHERE tenant_id = …` cannot leak rows. (Serves
  CMOS-00-ENG-008, CMOS-03-ARCH-050.)
- **CMOS-06-DB-021** No foreign key, join, or view may reference a row of a
  different `tenant_id`. Cross-tenant references are forbidden by construction
  (CMOS-02-DOM-002). Composite foreign keys SHOULD include `tenant_id` so the
  database itself rejects a cross-tenant reference.
- **CMOS-06-DB-022** The tenant context MUST be set from an authenticated
  principal, never from client-supplied row data. A connection with no tenant
  context set MUST see no tenant-owned rows (fail-closed).
- **CMOS-06-DB-023** Administrative/cross-tenant operations (platform operators)
  MUST use a distinct, audited role that bypasses RLS explicitly; such access MUST
  emit an `AuditEntryRecorded` event. Application service roles MUST NOT hold that
  bypass. (Serves CMOS-00-ENG-012.)
- **CMOS-06-DB-024** Every secondary index on a tenant-owned relation SHOULD lead
  with `tenant_id` so that per-tenant queries are index-local and one tenant's
  volume does not degrade another's plans. (Serves CMOS-00-ENG-015.)

## 4. Keys, constraints & indexes (normative)

- **CMOS-06-DB-030** Foreign keys MUST model the ownership graph of
  [Volume 2 §3](../002-domain-model/README.md) and MUST be acyclic with
  `organisation` at the root (CMOS-02-DOM-006). Ownership FKs are `ON DELETE
  RESTRICT`; lifecycle is a soft state transition, never a cascade delete (§6).
- **CMOS-06-DB-031** **Extension** number MUST be unique per tenant:
  `UNIQUE (tenant_id, number)` on `extension`. It MUST NOT be globally unique
  (CMOS-02-DOM-012). This is the canonical example: labels are unique only within
  the Organisation that owns them. (Serves CMOS-00-ENG-002, CMOS-00-ENG-008.)
- **CMOS-06-DB-032** The following uniqueness constraints are REQUIRED:
  - `organisation`: `UNIQUE (slug)` — DNS-safe, globally unique.
  - `did`: `UNIQUE (e164)` — an external number routes into at most one tenant at a
    time (a ported/released number is a new row; history retained, §6).
  - `device`: `UNIQUE (tenant_id, mac)` where `mac IS NOT NULL`.
  - `user`: `UNIQUE (tenant_id, email)` where `email IS NOT NULL`.
  - `outbox`/entity idempotency: `UNIQUE (tenant_id, idempotency_key)` on the
    outbox (§7).
- **CMOS-06-DB-033** A `did.destination_ref` and a `route.destination_ref` MUST
  resolve to exactly one live destination at a time (CMOS-02-DOM-011); polymorphic
  references MUST record both the target kind and target `id`, and a partial/check
  constraint MUST forbid a reference to a soft-deleted target being *created*
  (existing references to since-retired targets remain resolvable for history).
- **CMOS-06-DB-034** Indexes REQUIRED for correctness/performance at minimum:
  - FK-backing indexes on every foreign key column (lead with `tenant_id`).
  - `call`: index on `(tenant_id, correlation_id)` and `(tenant_id, created_at)`.
  - `cdr`: index on `(tenant_id, created_at)`, plus attribution indexes on
    `user_id`, `department_id`, `cost_centre_id` for Volume 10 rollups.
  - `outbox`: index on `(status, created_at)` for the relay poller (§7).
  - Soft-deleted rows SHOULD be excluded from hot-path indexes via partial indexes
    `WHERE deleted_at IS NULL`.
- **CMOS-06-DB-035** Money MUST be stored as integer minor units plus an ISO-4217
  currency code (two columns or a composite type), never floating point
  (CMOS-CONV-013). Timestamps MUST be `timestamptz` stored in UTC (CMOS-CONV-011).

## 5. Partitioning strategy (normative)

High-volume, append-heavy, time-scoped relations are partitioned so that
retention is a partition drop and per-tenant queries stay bounded as the platform
scales from ten users to a hundred thousand (CMOS-00-ENG-015).

- **CMOS-06-DB-040** `cdr`, `event_outbox` (archival tier), `audit_entry`, and any
  media/quality-stats relation MUST be **range-partitioned by time** (e.g. monthly
  on `created_at`). (Serves CMOS-00-ENG-015, CMOS-00-ENG-001 — retention becomes an
  O(1) partition operation, not a mass delete.)
- **CMOS-06-DB-041** Where a single tenant's volume warrants it, time partitions
  MAY be **sub-partitioned or hash-distributed by `tenant_id`** (composite
  time+tenant partitioning) so one large tenant does not dominate a partition.
  Partitioning strategy MUST be invisible to the API and event contracts
  (CMOS-03-ARCH-060).
- **CMOS-06-DB-042** Partition keys MUST be immutable for the row's lifetime; a row
  MUST NOT migrate partitions. `created_at` and `tenant_id` satisfy this.
- **CMOS-06-DB-043** Partition creation MUST be automated ahead of need (no
  write MUST ever fail for a missing future partition); partition retirement MUST
  respect the retention policy in §6 and MUST be audited.

## 6. Soft-delete, history & retention (normative)

Nothing of record is hard-deleted; deletion is a state transition and history
stays resolvable (CMOS-00-ENG-012, CMOS-02-DOM-003).

- **CMOS-06-DB-050** No application code path MUST issue a hard `DELETE` against an
  entity relation. Deletion is represented by the entity's terminal state
  (`RETIRED`/`DELETED`/`ARCHIVED`) and a `deleted_at` tombstone. (Serves
  CMOS-00-ENG-012.)
- **CMOS-06-DB-051** A soft-deleted row MUST remain foreign-key-resolvable so that
  a historical Call, CDR, or AuditEntry that references it can still be rendered.
  Reads on live surfaces MUST filter `deleted_at IS NULL` by default; history and
  audit surfaces MUST be able to see tombstoned rows.
- **CMOS-06-DB-052** The change history of any Digital Twin MUST be reconstructable
  (CMOS-02-DOM-005) from the append-only `audit_entry` relation plus the event
  stream; the reference implementation SHOULD keep an append-only per-entity
  history/revision relation rather than only the latest row. Rollback ("Time
  Machine") is republication of a prior version, never an in-place overwrite of
  history.
- **CMOS-06-DB-053** Each retention-governed relation (`cdr`, `event_outbox`
  archive, `audit_entry`, `object` metadata, `recording`/`voicemail` references)
  MUST carry an effective retention policy; expiry MUST be enforced by partition
  drop or tombstone-then-purge on a schedule, and every purge MUST emit an audit
  event. (Serves CMOS-00-ENG-012, CMOS-00-ENG-001.)
- **CMOS-06-DB-054** `audit_entry` is strictly append-only: no `UPDATE` and no
  `DELETE` except a policy-driven, audited retention purge of whole aged
  partitions. A trigger or restricted grant SHOULD enforce append-only at the
  database. (Serves CMOS-00-ENG-012.)
- **CMOS-06-DB-055** Hard erasure demanded by law (e.g. right-to-erasure) MUST be
  implemented as **crypto-shredding or field-level redaction** that preserves
  referential structure and the audit trail of the erasure itself, not as row
  deletion. PII columns MUST be identifiable (aligned with the event `x-pii`
  marking, CMOS-05-EVT-042) so redaction is automatable.

## 7. Transactional outbox (normative)

The outbox is the bridge from committed state to published Events; it is the
storage mechanism behind CMOS-03-ARCH-030 and CMOS-05-EVT-010.

- **CMOS-06-DB-060** Every state change that must be observable MUST write its
  Event envelope into an `event_outbox` row **in the same transaction** as the
  entity mutation. If the transaction commits, the event is durably queued; if it
  rolls back, no event exists. There is no code path that mutates state and
  publishes out-of-band. (Serves CMOS-00-ENG-004, CMOS-03-ARCH-030,
  CMOS-05-EVT-010.)
- **CMOS-06-DB-061** `event_outbox` MUST store the full envelope fields of
  [Volume 5 §2](../005-events/README.md#2-the-envelope-normative): `id`,
  `specversion`, `type`, `source`, `time`, `tenant_id`, `subject`,
  `correlation_id`, `causation_id`, `idempotency_key`, `sequence`, `traceparent`,
  and the `data` payload (jsonb), plus a relay `status`
  (`PENDING|PUBLISHED|DEAD`), `attempts`, and `next_attempt_at`.
- **CMOS-06-DB-062** `UNIQUE (tenant_id, idempotency_key)` MUST be enforced on the
  outbox so a retried producer transaction cannot enqueue a duplicate fact
  (supports consumer de-duplication, CMOS-05-EVT-011).
- **CMOS-06-DB-063** `sequence` MUST be assigned per `correlation_id` monotonically
  within the producing transaction so relayed events preserve intra-correlation
  order (CMOS-05-EVT-020). A per-`correlation_id` counter or a gap-free ordinal
  derived at write time satisfies this.
- **CMOS-06-DB-064** A relay process MUST move `PENDING` rows to the Event Bus,
  mark them `PUBLISHED`, retry with exponential backoff + jitter on failure, and
  route rows exceeding the retry budget to `DEAD` (dead-letter, CMOS-05-EVT-013).
  The relay MUST be safe to run concurrently (row-level claim / `SKIP LOCKED`) and
  MUST be idempotent — re-publishing a `PENDING` row after a crash is expected and
  harmless because consumers de-duplicate on `id`/`idempotency_key`.
- **CMOS-06-DB-065** Published outbox rows MUST NOT be hard-deleted on the hot
  path; they age out by partition retention (§5, §6) so the outbox doubles as a
  replayable local event log.

## 8. Migration strategy (normative)

- **CMOS-06-DB-070** Schema evolution MUST be **forward-only and versioned**:
  migrations are an ordered, immutable, append-only sequence, each identified and
  checksummed, applied exactly once and recorded in a `schema_migrations` ledger.
  A previously applied migration MUST NOT be edited or re-ordered. (Serves
  CMOS-00-ENG-012, CMOS-00-ENG-001.)
- **CMOS-06-DB-071** Migrations MUST be **backwards-compatible within a contract
  MINOR line** and apply online without downtime: additive first (new nullable
  column / new table / new index built concurrently), backfill, then switch. A
  destructive change (drop/narrow a column) MUST follow the expand→migrate→
  contract pattern across releases and MUST be gated behind a MAJOR bump plus an
  ADR when it affects a contract surface (CMOS-CONV-005). (Serves CMOS-00-ENG-003.)
- **CMOS-06-DB-072** The database schema version MUST be inspectable at runtime and
  the process MUST refuse to start against a schema newer than it understands
  (fail-closed), so a rolling upgrade never runs old code on a contracted-away
  schema.
- **CMOS-06-DB-073** Reference data / seed migrations MUST be idempotent and
  tenant-agnostic; no migration may embed one tenant's data.
- **CMOS-06-DB-074** Long-running data backfills MUST run as background,
  resumable, batched jobs off the migration critical path (CMOS-03-ARCH-021) so a
  migration lock is never held for a bulk rewrite.

## 9. Consistency & transactions (normative)

- **CMOS-06-DB-080** A single logical operation that spans multiple relations
  (e.g. answer a Call: update `call`, insert `media_stream` rows, enqueue
  `event_outbox`) MUST be atomic — one transaction, all-or-nothing.
- **CMOS-06-DB-081** Cross-service or cross-node consistency MUST NOT rely on
  distributed locks in the database; it is achieved by the outbox + idempotent
  event handlers (eventual consistency with at-least-once delivery), not two-phase
  commit. (Serves CMOS-00-ENG-015.)
- **CMOS-06-DB-082** Real-time actor state (live Call/Registration, Volume 3 §3)
  is authoritative in memory for the life of the session and is *checkpointed* to
  the database at meaningful transitions; the database is the durable record, not
  the per-packet hot path (CMOS-03-ARCH-021).

## 10. Data shapes

The relation-by-relation listing — columns, type classes, keys, and
index/constraint notes for the ~10 core entities plus the outbox — is in
[`schema-overview.md`](schema-overview.md). Where that listing and a JSON Schema
in [`contracts/`](../../contracts/) disagree about a field's *shape*, the schema
wins (CONVENTIONS §8); this volume owns *relational* concerns (keys, isolation,
partitioning, retention) that the schemas do not express.

## Conformance notes

- Profile: `core`. These requirements are exercised indirectly — an implementation
  is `core`-L2 conformant if its API (Volume 4) and events (Volume 5) behave
  correctly regardless of internal schema (CMOS-03-ARCH §9, CMOS-06-DB-002).
- L2 storage-relevant checks the behavioural suite MUST assert: (a) a committed
  state change always yields exactly one outbox row and the relayed event
  (CMOS-06-DB-060); (b) a rolled-back operation yields neither
  state nor event; (c) a cross-tenant read returns zero rows under RLS
  (CMOS-06-DB-020/022); (d) an Extension number collision within a tenant is
  rejected and across tenants is allowed (CMOS-06-DB-031); (e) a "delete" leaves a
  resolvable tombstone, never a missing row (CMOS-06-DB-050/051).
- L3 additionally exercises retention partition-drop and online migration under
  load (Volume 16 chaos suite).

## Open items

- Physical DDL reference and migration tooling binding (SQLx) — implementation
  note, not a contract; candidate for an appendix.
- Per-entity history/revision relation shape (CMOS-06-DB-052) — decide append-log
  vs. temporal tables; ADR pending.
- Read-model / CQRS projections for reporting (Volume 10, Volume 15) — reserved.
- Messaging/Video workload relations — track Volume 2 v0.4 entities.
- Formal PII column registry aligned with event `x-pii` marking (CMOS-06-DB-055).

## Change log
- **0.3.0** — Initial implementation-grade draft: universal row model, UUIDv7
  keys, RLS tenant isolation, uniqueness/FK/index constraints (Extension unique
  per tenant), time+tenant partitioning, soft-delete/retention (no hard deletes),
  transactional outbox backing event delivery, and forward-only versioned
  migrations.
