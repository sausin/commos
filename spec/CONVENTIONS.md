# CommOS Specification Conventions

This document is **normative**. It defines how every other volume is written and
read. If any volume contradicts this document, this document wins until the
conflict is resolved by an ADR.

---

## 1. Requirement language (RFC 2119 / RFC 8174)

The key words **MUST**, **MUST NOT**, **REQUIRED**, **SHALL**, **SHALL NOT**,
**SHOULD**, **SHOULD NOT**, **RECOMMENDED**, **MAY**, and **OPTIONAL** are to be
interpreted as described in RFC 2119 and RFC 8174 — and **only** when they appear
in all-capitals.

- A **normative** statement constrains a conforming implementation.
- An **informative** statement (examples, rationale, notes) does not. Informative
  blocks are prefixed with `> Note:` or placed under a heading containing
  "(informative)".

Every normative statement SHOULD be phrased so that conformance is testable.

## 2. Requirement identifiers

Every discrete normative requirement is assigned a stable identifier:

```
CMOS-<VOL>-<SUBSYS>-<NNN>
```

- `VOL` — two-digit volume number (`02`, `05`, …).
- `SUBSYS` — short uppercase subsystem tag (`DOM`, `EVT`, `API`, `SIP`, `PROV`,
  `SEC`, `BILL`, `AI`, `PLUG`, `UI`, `DEP`, `OBS`, `TEST`, `PERF`, `ENG`).
- `NNN` — zero-padded ordinal, monotonically increasing within its
  `VOL`+`SUBSYS`, **never reused** even if a requirement is deleted.

Example: `CMOS-05-EVT-014` is the 14th event-model requirement.

Requirement IDs are permanent contract points. A conformance test SHOULD cite the
requirement ID(s) it exercises. Deleting a requirement means marking it
`WITHDRAWN`, not renumbering its neighbours.

## 3. Conformance profiles and levels

An implementation declares which **profiles** it implements and at which **level**.

### Profiles
A profile is a coherent slice of the platform that can be implemented and tested
independently:

| Profile | Covers |
|---------|--------|
| `core` | Identity, Policy, Event Bus, API Gateway, Object Storage abstraction, Domain Model, Audit. Required by every other profile. |
| `voice` | SIP/RTP signalling & media, routing, registration, voicemail, recording (Volumes 7, 3-media). |
| `provisioning` | Device lifecycle & zero-touch onboarding (Volume 8). |
| `billing` | CDR, rating, cost allocation (Volume 10). |
| `ai` | AI integration surface (Volume 11). |
| `plugins` | WASM plugin runtime (Volume 12). |
| `contact-center` | Queues, agents, supervision (subset of Volumes 1/7). |

### Levels
| Level | Meaning |
|-------|---------|
| **L1 Contract** | Emits/consumes the frozen JSON Schemas and OpenAPI shapes correctly. Schema-valid, but behaviour untested. |
| **L2 Behavioural** | Passes the behavioural conformance suite for the profile (state machines, ordering, idempotency). |
| **L3 Interoperable** | Passes L2 **and** the interop suite (real SIP endpoints, real vendor devices, cross-implementation event exchange). |

A claim of conformance MUST be of the form:
`CommOS <profile> conformant, level <Lx>, spec vX.Y`.

## 4. Contract versioning (SemVer for contracts)

The specification suite and each machine-readable contract carry a semantic
version `MAJOR.MINOR.PATCH`:

- **PATCH** — editorial only; no change to any normative requirement or schema shape.
- **MINOR** — backwards-compatible additions: new optional fields, new events, new
  endpoints, new enum values behind capability negotiation. An L1-conformant
  implementation of `X.(Y-1)` remains L1-conformant against `X.Y`.
- **MAJOR** — a breaking change to a frozen contract. Requires an ADR and a
  migration note. MAJOR bumps SHOULD be rare and batched.

Rules that preserve MINOR compatibility (all normative):

- `CMOS-CONV-001` A new schema field MUST be optional, or MUST have a default that
  reproduces prior behaviour.
- `CMOS-CONV-002` An enum MUST NOT have members removed or renumbered within a
  MAJOR line; members may be added.
- `CMOS-CONV-003` Event and entity **type names** are permanent within a MAJOR line.
- `CMOS-CONV-004` Consumers MUST ignore unknown fields (the tolerant-reader rule)
  and MUST NOT reject an envelope solely for carrying a higher `specversion` with
  the same MAJOR.
- `CMOS-CONV-005` Removing or narrowing any field, tightening any constraint, or
  changing a field's type is a MAJOR change.

## 5. The freeze lifecycle

Every volume and every contract has a status:

| Status | Meaning |
|--------|---------|
| `DRAFT` | Under active authoring. May change without notice. |
| `REVIEW` | Structurally complete; soliciting review. Changes tracked. |
| `FROZEN` | Normative and stable. Changes only via SemVer rules + ADR. Has a machine-readable contract (where applicable) and passing conformance coverage. |
| `WITHDRAWN` | Superseded. Retained for history; MUST NOT be implemented. |

A volume MUST NOT be marked `FROZEN` unless: (a) every normative statement has a
requirement ID, (b) any data shapes it defines exist under `contracts/`, and (c)
the conformance harness passes for it.

## 6. Identifiers, data types, and encoding

These apply to every wire format (API bodies, event payloads) unless a volume
states otherwise.

- `CMOS-CONV-010` **Entity identifiers** are UUIDv7 (time-ordered) encoded as
  lowercase canonical strings. They are globally unique and opaque; clients MUST
  NOT parse meaning from them beyond ordering.
- `CMOS-CONV-011` **Timestamps** are RFC 3339 / ISO 8601 in UTC with millisecond
  precision and a trailing `Z` (e.g. `2026-07-20T14:32:05.123Z`).
- `CMOS-CONV-012` **Durations** are integer milliseconds unless a field name ends
  in `_seconds`.
- `CMOS-CONV-013` **Money** is represented as `{ "currency": "<ISO-4217>", "minor_units": <int> }`
  (integer minor units; never floating point).
- `CMOS-CONV-014` **JSON field names** are `snake_case`. **Event and entity type
  names** are `PascalCase`. **Enum values** are `SCREAMING_SNAKE_CASE`.
- `CMOS-CONV-015` **Tenant scoping**: every persisted entity and every event
  carries a `tenant_id`. There is no cross-tenant identifier reuse.
- `CMOS-CONV-016` **E.164** is the canonical form for external telephone numbers.
  Internal addressing uses opaque identity/route references, not dial strings.
- `CMOS-CONV-017` **Enumerations are closed by default**; where an open set is
  intended (e.g. vendor names) the field name ends in `_key` and accepts any
  `^[a-z0-9_.-]+$` string.

## 7. Naming: entities, events, endpoints

- Entities are singular `PascalCase` nouns (`User`, `Device`, `Call`).
- Events are `PascalCase` and past-tense verbs on an entity
  (`CallAnswered`, `DeviceApproved`). An event names something that *has happened*.
- Commands (API mutations) are imperative and expressed as HTTP verbs on resource
  paths (`POST /calls`, `POST /devices/{id}:approve`), never as events.
- Long-running actions use the `:verb` sub-resource form
  (`POST /devices/{id}:approve`) and MUST emit a corresponding event.

## 8. How the contracts and prose relate

- The **prose** (`spec/`) is the source of *meaning and rationale*.
- The **machine-readable contracts** (`contracts/`) are the source of *shape*.
- Where they disagree about a shape, the contract wins and the prose is corrected.
  Where they disagree about meaning, the prose wins and the contract is corrected.
- The conformance harness enforces that every event/entity named in a `FROZEN`
  volume has a schema, and vice-versa (`conformance/run.py`).

## 9. Document template

Each volume's `README.md` SHOULD follow this order: **Status & version → Scope →
Normative requirements (with IDs) → Data shapes (linking `contracts/`) → State
machines / flows → Conformance notes → Open items → Change log**.
