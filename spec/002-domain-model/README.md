# Volume 2 — Domain Model

**Status:** REVIEW · **Version:** 0.3.0 · **Subsystem tag:** DOM

The domain model is the **keystone** of the specification. Every event payload
(Volume 5) and every API body (Volume 4) is a projection of an entity defined here.
Freezing entity identity, ownership, and lifecycle first prevents churn everywhere
downstream.

Machine-readable form: [`contracts/json-schema/entities/`](../../contracts/json-schema/entities/).
Where this prose and a schema disagree about *shape*, the schema wins (CONVENTIONS §8).

- Detailed per-entity fields: [`entities.md`](entities.md)
- Lifecycle state machines: [`state-machines.md`](state-machines.md)

---

## 1. Modelling rules (normative)

- **CMOS-02-DOM-001** Every entity has a UUIDv7 `id`, a `tenant_id`, `created_at`,
  and `updated_at`. (Root tenant entities carry `tenant_id == id` conceptually; see
  Organisation.)
- **CMOS-02-DOM-002** Entities are **tenant-scoped**. No relationship may cross a
  `tenant_id` boundary. Cross-tenant references are forbidden by construction.
- **CMOS-02-DOM-003** Deletion is a **state transition** to a terminal state
  (`RETIRED`, `DELETED`, `ARCHIVED`), never a hard delete of record. Referenced
  history remains resolvable. (Serves CMOS-00-ENG-012.)
- **CMOS-02-DOM-004** The four principals — **User**, **Identity**, **Device**,
  **Organisation** — are distinct entities and MUST NOT be collapsed. **Extension**
  is a label, not a principal. (Serves CMOS-00-ENG-002.)
- **CMOS-02-DOM-005** Every entity that can change materially is a **Digital Twin**:
  it carries a monotonic `version` integer and its change history is reconstructable
  from the audit log. (Serves CMOS-00-ENG-012.)
- **CMOS-02-DOM-006** Ownership is explicit. Each entity names its owning parent(s).
  The ownership graph is acyclic with `Organisation` at the root.
- **CMOS-02-DOM-007** State transitions are closed sets. An implementation MUST
  reject a transition not present in the entity's state machine
  ([`state-machines.md`](state-machines.md)) and MUST emit the corresponding event.

## 2. The principals (why four, not one)

> Informative. The legacy PBX "extension" fuses four concepts. CommOS separates them
> because billing, security, and mobility each need a different one:

```
Organisation ──owns──▶ Device ──presents──▶ Identity ──asserts──▶ User
      │                                                             ▲
      └──────────────── contains ──────────────────────────────────┘
                        (Department, Cost Centre group Users)
```

- **Organisation** — the tenant; unit of isolation and billing root.
- **User** — the person/service principal; who you *are*.
- **Identity** — a *proven* authentication at a point in time on a Device (PIN,
  FIDO2, NFC…); how the system *knows* it's you *now*.
- **Device** — the hardware/endpoint; *where* the call physically happens. Owned by
  the Organisation so a shared phone can serve different Users across calls.
- **Extension** — a dialable label that resolves (via Route) to a destination.

This is what makes "Alice logs into the reception phone, calls out, and *Alice* is
billed; later Bob logs in and *Bob* is billed" work correctly (CMOS-00-ENG-011).

## 3. Entity catalogue

Full field lists in [`entities.md`](entities.md); schemas in `contracts/`.

### Tenancy & people
| Entity | Owns / relates | Purpose |
|--------|----------------|---------|
| **Organisation** | root | Tenant; isolation & billing root. |
| **CostCentre** | in Organisation | Accounting grouping. |
| **Department** | in Organisation, under CostCentre | Grouping of Users. |
| **User** | in Organisation, in Department | Human/service principal. |
| **Identity** | of User, on Device | A proven authentication assertion + method. |
| **Capability** | granted to User | Fine-grained permission. |
| **Policy** | in Organisation | Declarative allow/deny/require rule. |

### Endpoints & numbering
| Entity | Owns / relates | Purpose |
|--------|----------------|---------|
| **Device** | owned by Organisation, assigned to User | Physical/virtual endpoint. |
| **Extension** | in Organisation | Dialable label → Route. |
| **DID** | in Organisation, via Carrier | External E.164 number. |
| **Gateway** | in Organisation | Bridge to PSTN/mobile/SIP. |
| **Carrier** | in Organisation | External transport provider. |
| **Trunk** | on Carrier | Signalling relationship with a Carrier. |

### Routing & workloads
| Entity | Owns / relates | Purpose |
|--------|----------------|---------|
| **Route** | in Organisation | Source/condition → destination rule. |
| **CallFlow** | in Organisation | Versioned graph of routing nodes. |
| **IVR** | node in CallFlow | Interactive menu. |
| **Queue** | in Organisation | Ordered waiting Calls + strategy. |
| **Conference** | in Organisation | Many-party media session. |
| **Call** | in Organisation | A signalling+media session (workload instance). |
| **MediaStream** | in Call | One directional media flow. |
| **Voicemail** | of User/Extension | A stored message (→ Object). |
| **Recording** | of Call/Conference | A stored recording (→ Object). |

### Platform objects
| Entity | Owns / relates | Purpose |
|--------|----------------|---------|
| **Object** | in Organisation | Large binary artifact (abstracted store). |
| **CDR** | derived from Call | Billable, attributable record. |
| **Webhook** | in Organisation | Event subscription endpoint. |
| **Automation** | in Organisation | Event-triggered declarative action. |
| **AIJob** | in Organisation | An AI task over events/objects (external). |
| **Plugin** | in Organisation/global | A WASM extension instance. |
| **AuditEntry** | in Organisation | Append-only record of an action. |

## 4. Cross-entity invariants

- **CMOS-02-DOM-010** A **Call** MUST reference, at answer time, an owning
  Organisation and — for any chargeable leg — a Device and an Identity (hence a
  User). Absence of a required Identity on a chargeable call is a policy failure, not
  a silent default. (Serves CMOS-00-ENG-011.)
- **CMOS-02-DOM-011** A **DID** resolves to exactly one destination *at a time*
  (Route/CallFlow/Extension/Queue/User); many DIDs MAY resolve to the same
  destination; one User MAY hold many DIDs.
- **CMOS-02-DOM-012** An **Extension** is unique within its Organisation and MUST NOT
  be assumed unique globally.
- **CMOS-02-DOM-013** A **Recording**, **Voicemail**, or transcript is stored only as
  an **Object**; entities hold Object references (URIs), never inline blobs.
- **CMOS-02-DOM-014** Every **CDR** references the Call, Organisation, and — where
  applicable — CostCentre, Department, User, Identity, Device, Extension, DID,
  Carrier, Recording Object, and Transcript Object. (Serves Volume 10.)
- **CMOS-02-DOM-015** A **Device** may be `assigned_user_id = null` (a shared/hot-desk
  device); in that state, chargeable external calls require a per-call Identity per
  Policy.

## 5. Lifecycles (summary)

Full diagrams in [`state-machines.md`](state-machines.md). Each terminal/again-active
transition emits an event (Volume 5).

- **Device:** `DETECTED → PENDING → APPROVED → PROVISIONED → OPERATIONAL → (REPLACING) → RETIRED`
- **Call:** `INITIATED → RINGING → ANSWERED → (HELD ⇄ ANSWERED) → ENDED` with
  branch `INITIATED/RINGING → FAILED|NO_ANSWER|BUSY|REJECTED`.
- **Identity:** `REQUESTED → AUTHENTICATED → ACTIVE → EXPIRED|REVOKED`.
- **CallFlow:** `DRAFT → PUBLISHED → (SUPERSEDED)`; publishing is versioned and
  rollback-able (Time Machine).
- **User:** `INVITED → ACTIVE → SUSPENDED → DEACTIVATED`.
- **Provisioning artifact URLs:** single-use, short-lived (Volume 8).

## 6. Conformance notes

- L1: entities emitted by an implementation MUST validate against
  `contracts/json-schema/entities/*`.
- L2: state transitions MUST match [`state-machines.md`](state-machines.md); illegal
  transitions rejected; correct events emitted.
- The harness (`conformance/run.py`) checks that every entity named here has a schema
  and that example instances validate.

## 7. Open items
- Messaging/Video workload entities (Message, Thread, VideoRoom) — reserved for v0.4.
- IoT endpoint entity model — reserved.
- Formal relational cardinalities move to Volume 6 (Database).

## Change log
- **0.3.0** — Full entity catalogue, principal separation, cross-entity invariants,
  lifecycle summary; backed by JSON Schema contracts.
