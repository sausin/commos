# Volume 5 — Events

**Status:** REVIEW · **Version:** 0.3.0 · **Subsystem tag:** EVT

The event model is the platform's highest-leverage integration surface. AI, CRM,
billing, and automation are all **subscribers**; the platform embeds no knowledge of
any specific consumer (CMOS-00-ENG-004, CMOS-00-ENG-013). This volume freezes the
**envelope**, **delivery guarantees**, **ordering**, **idempotency**, and
**versioning** of every canonical Event.

Machine-readable form:
[`contracts/json-schema/envelope.schema.json`](../../contracts/json-schema/envelope.schema.json)
and [`contracts/json-schema/events/`](../../contracts/json-schema/events/).
Full list: [`catalog.md`](catalog.md).

---

## 1. What an Event is

- **CMOS-05-EVT-001** An Event is an **immutable, past-tense** record that something
  happened. Events are named `PascalCase` (`CallAnswered`), never imperative.
- **CMOS-05-EVT-002** Every state transition enumerated in the domain state machines
  ([Volume 2](../002-domain-model/state-machines.md)) MUST publish its named Event.
- **CMOS-05-EVT-003** Events are **facts, not commands**. A subscriber MUST NOT
  assume it can prevent or alter the fact by how it handles the event. Commands go
  through the API (Volume 4).

## 2. The envelope (normative)

Every Event is delivered inside a common envelope, modelled on CloudEvents 1.0 with
CommOS-required extensions. Schema:
[`envelope.schema.json`](../../contracts/json-schema/envelope.schema.json).

| Field | Type | Req | Meaning |
|-------|------|-----|---------|
| `id` | uuid (v7) | ✔ | Unique event id. Time-ordered. |
| `specversion` | string | ✔ | CommOS event spec version, e.g. `0.3`. |
| `type` | string | ✔ | Event type, e.g. `CallAnswered`. `PascalCase`. |
| `source` | string(uri) | ✔ | Emitting subsystem, e.g. `/routing` or node URI. |
| `time` | timestamp | ✔ | RFC 3339 UTC millis when the fact occurred. |
| `tenant_id` | uuid | ✔ | Organisation scope (CONVENTIONS §6). |
| `subject` | string | ✔ | Primary entity id the event is about. |
| `correlation_id` | uuid | ✔ | Shared across all events/commands of one logical operation (e.g. a Call). |
| `causation_id` | uuid | | The event/command id that directly caused this one. |
| `idempotency_key` | string | ✔ | Stable key for at-least-once de-duplication. |
| `sequence` | int | | Per-`correlation_id` monotonic ordinal (ordering, §4). |
| `traceparent` | string | | W3C trace context for distributed tracing (Volume 15). |
| `datacontenttype` | string | ✔ | Always `application/json` in v0.x. |
| `data` | object | ✔ | The event-specific payload; shape per event schema. |

- **CMOS-05-EVT-004** Consumers MUST tolerate unknown envelope and `data` fields
  (tolerant reader; CMOS-CONV-004) and MUST NOT reject an event solely for a higher
  `specversion` within the same MAJOR line.
- **CMOS-05-EVT-005** `data` MUST validate against the event's schema in
  `contracts/json-schema/events/<Type>.schema.json`.

## 3. Delivery guarantees (normative)

- **CMOS-05-EVT-010** Delivery is **at-least-once**. Producers MUST persist an event
  before acknowledging the state change that produced it (transactional outbox or
  equivalent), so no committed change is silently unpublished.
- **CMOS-05-EVT-011** Consumers MUST be **idempotent**, de-duplicating on
  (`type`, `idempotency_key`) or on `id`. Re-delivery MUST be safe.
- **CMOS-05-EVT-012** The bus MUST support **at-least-once** subscriptions with
  durable cursors; an **exactly-once effect** is achieved by consumer idempotency,
  not by the transport.
- **CMOS-05-EVT-013** Undeliverable events (after a bounded retry budget) MUST be
  routed to a **dead-letter** stream, never dropped silently. Retry uses exponential
  backoff with jitter; the budget is configurable per subscription.
- **CMOS-05-EVT-014** Webhook delivery MUST be signed (HMAC over the raw body with a
  per-webhook secret) and MUST emit `WebhookDelivered` / `WebhookDeliveryFailed`.

## 4. Ordering (normative)

- **CMOS-05-EVT-020** Events sharing a `correlation_id` are **totally ordered** by
  `sequence`. Consumers MUST use `sequence`, not receipt time, to order within a
  correlation.
- **CMOS-05-EVT-021** No global total order is guaranteed across correlations. Cross-
  entity ordering, where needed, is derived from `time` and is best-effort.
- **CMOS-05-EVT-022** A late or duplicate event with a `sequence` already processed
  for its `correlation_id` MUST be ignored by an idempotent consumer.

## 5. Versioning (normative)

- **CMOS-05-EVT-030** Event `type` names are permanent within a MAJOR line
  (CMOS-CONV-003). Renaming an event is a MAJOR change.
- **CMOS-05-EVT-031** New event types and new **optional** `data` fields are MINOR.
  Removing/narrowing a `data` field or making an optional field required is MAJOR.
- **CMOS-05-EVT-032** `specversion` advances with the event-spec MINOR/MAJOR; PATCH
  never changes payload shape.

## 6. Security & privacy

- **CMOS-05-EVT-040** Events are tenant-scoped; a subscription MUST NOT receive
  events for a `tenant_id` it is not authorised for (capability-gated).
- **CMOS-05-EVT-041** Payloads MUST NOT carry secrets, raw media, or full recordings
  — only Object references (URIs) resolvable under authorization (CMOS-02-DOM-013).
- **CMOS-05-EVT-042** PII in payloads is minimised; fields carrying PII are marked in
  the schema (`x-pii: true`) so downstream redaction can be automated.

## 7. Event families

See [`catalog.md`](catalog.md) for the full list with schema links. Families:
`Identity/*`, `User/*`, `Device/*`, `Provisioning/*`, `Registration/*`, `Call/*`,
`Media/Recording/*`, `Conference/*`, `Gateway/*`, `Billing/*`, `Webhook/*`,
`Automation/*`, `AI/*`, `Plugin/*`, `Audit/*`.

## 8. Conformance notes

- L1: emitted envelopes validate against `envelope.schema.json`; `data` validates
  against the event schema.
- L2: for a driven scenario, the exact set and **order** (`sequence`) of events match
  the state-machine expectation; re-delivery is a no-op on an idempotent consumer.
- The harness verifies catalog↔schema↔example consistency (`conformance/run.py`).

## 9. Open items
- Bus binding profiles (NATS JetStream, Redis Streams, Kafka) — Volume 3/12 detail.
- Schema registry endpoint (`GET /events/schemas`) — Volume 4.

## Change log
- **0.3.0** — Envelope, delivery/ordering/idempotency/versioning frozen; catalog
  linked to JSON Schemas with examples.
