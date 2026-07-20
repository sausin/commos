# Volume 10 — Billing

**Status:** REVIEW · **Version:** 0.4.0 · **Subsystem tag:** BILL

Billing is a **first-class workload of the substrate**, not a bolt-on report. Voice
is one billable workload; messaging, video, AI jobs, and future workloads bill
through the same machinery. Every chargeable event resolves to an **attribution
chain** and is recorded as an immutable, append-only **CDR** (Call Detail Record).
This volume freezes how a Call becomes a CDR, how a CDR is **rated**, how cost is
**allocated** up the organisational hierarchy, and the policy controls (identity
authentication, prepaid balances, quotas, approvals, fraud) that gate chargeable
activity.

Companion: [`model.md`](model.md) (hierarchy & charge sources). CDR entity:
[Volume 2 · entities](../002-domain-model/entities.md#cdr) and cross-entity invariant
[CMOS-02-DOM-014](../002-domain-model/README.md#4-cross-entity-invariants). Emission
event: `BillingGenerated`
([catalog](../005-events/catalog.md#billing-webhook-automation-ai-plugin-audit)).

> Note (informative): the entire volume exists to make one sentence true — *"Alice
> logs into the reception Device, calls out, and Alice's User (via her Identity) and
> her Cost Centre are billed; later Bob logs in and Bob is billed"* — for every
> workload, at any scale, without exception.

---

## 1. Scope & principles

- **CMOS-10-BILL-001** Every chargeable Call MUST produce **exactly one** primary
  CDR, derived from the Call's canonical Events (Volume 5), never from a parallel
  media-plane log. The CDR is the single billable projection of the Call.
  (Serves CMOS-00-ENG-004.)
- **CMOS-10-BILL-002** A CDR MUST be **attributable**: it MUST resolve the three
  identities of the Call — **Device → User (via Identity) → Organisation** — or
  record an explicit, policy-authorised reason for a missing Identity. Attribution is
  never a silent default. (Serves CMOS-00-ENG-011, CMOS-02-DOM-010.)
- **CMOS-10-BILL-003** CDRs are **immutable and append-only**. A correction MUST be a
  new CDR that references and supersedes the prior one (`supersedes_cdr_id`), never an
  in-place mutation. (Serves CMOS-00-ENG-012.)
- **CMOS-10-BILL-004** Billing MUST be **tenant-scoped**. A CDR, rating profile,
  balance, or quota MUST NOT reference or aggregate across a `tenant_id` boundary.
  (Serves CMOS-00-ENG-008, CMOS-CONV-015.)
- **CMOS-10-BILL-005** Rating MUST be **deterministic and reproducible**: given the
  same CDR inputs and the same versioned rating profile, an implementation MUST
  compute the same `cost`. Reproducibility is a conformance property.
- **CMOS-10-BILL-006** Monetary amounts MUST use the `{ currency, minor_units }`
  integer money type (CMOS-CONV-013); intermediate rating arithmetic MUST NOT use
  binary floating point. Rounding is defined per rating profile (§3).

## 2. Attribution model (normative)

The organisational hierarchy from [`model.md`](model.md) is the allocation spine:

```
Organisation ─▶ Cost Centre ─▶ Department ─▶ User ◀─asserts─ Identity ─on─ Device
```

- **CMOS-10-BILL-010** At CDR generation the platform MUST resolve, where applicable:
  `organisation_id`, `cost_centre_id`, `department_id`, `user_id`, `identity_id`,
  `device_id`, `extension`, `did`, and `carrier_id`. These are copied as a **snapshot**
  into the CDR and MUST NOT be re-resolved later (attribution is fixed at call time).
- **CMOS-10-BILL-011** The `user_id`/`cost_centre_id`/`department_id` MUST be derived
  from the **Identity** attributed to the chargeable leg (`Call.identity_id`), not
  from the Device's static `assigned_user_id`. On a shared/hot-desk Device
  (`assigned_user_id = null`), the per-call Identity is the sole source of user
  attribution (CMOS-02-DOM-015).
- **CMOS-10-BILL-012** Where a chargeable external leg lacks a required Identity, the
  implementation MUST NOT bill it to the Device owner by default. It MUST either
  (a) block the call per Policy (`REQUIRE_IDENTITY`, §6), or (b) record the CDR with
  `attribution_status = UNATTRIBUTED` and `user_id = null`, routing it to an
  unattributed-cost bucket for operator review.
- **CMOS-10-BILL-013** `Emergency Override` calls (Glossary) MUST always be recorded
  as CDRs with `attribution_status = EMERGENCY_OVERRIDE` even when identity/authorisation
  was bypassed; emergency calls are never blocked for billing reasons.

## 3. The CDR (normative)

The CDR is the entity frozen in [Volume 2](../002-domain-model/entities.md#cdr); this
section is the **billing semantics** of its fields. Where prose and the entity schema
disagree about shape, the schema wins (CONVENTIONS §8).

| Group | Fields | Billing meaning |
|-------|--------|-----------------|
| Correlation | `call_id`, `correlation_id` | Ties the CDR to the Call and all its Events. |
| Attribution | `organisation_id`, `cost_centre_id`, `department_id`, `user_id`, `identity_id`, `device_id`, `extension`, `did` | The snapshotted chain (§2). |
| Transport | `carrier_id`, `trunk_id`, `gateway_id`, `charge_source` | Which charge source rated the leg (§5). |
| Timing | `started_at`, `answered_at`, `ended_at`, `duration_ms`, `billable_ms` | `billable_ms` is post-increment (§3.2). |
| Rating | `rating_profile_id`, `rating_profile_version`, `rate`, `cost`, `currency` | Reproducible output (CMOS-10-BILL-005). |
| Media | `codec`, `recording_object_id`, `transcript_object_id` | Object references only, never inline (CMOS-02-DOM-013). |
| Direction | `direction`, `hangup_cause` | `INBOUND\|OUTBOUND\|INTERNAL`. |
| Lifecycle | `attribution_status`, `rating_status`, `supersedes_cdr_id`, `tags` | §3.3, §7. |

- **CMOS-10-BILL-020** A CDR MUST carry `duration_ms` (wall-clock from `answered_at`
  to `ended_at`, `0` if never answered) and `billable_ms` (the rated duration after
  applying the rating profile's increment and minimum, §3.2). Durations are integer
  milliseconds (CMOS-CONV-012).
- **CMOS-10-BILL-021** A CDR MUST carry the `rating_profile_id` **and** the
  `rating_profile_version` used, so the rating is reproducible after the profile
  changes.
- **CMOS-10-BILL-022** A CDR for an unanswered terminal outcome (`NO_ANSWER`, `BUSY`,
  `REJECTED`, `FAILED`) MUST still be emitted, with `billable_ms = 0` and
  `cost.minor_units = 0` unless the applicable rating profile explicitly rates setup
  or failed attempts (`rate_failed_attempts = true`).

### 3.1 Rating profiles

- **CMOS-10-BILL-030** A **rating profile** is a versioned entity owned by an
  Organisation (or referenced from a Carrier, `Carrier.rating_profile_id`) that maps a
  matched **rate key** to a per-unit rate, increment, minimum duration, setup fee,
  currency, and rounding mode. Publishing a new profile version is a Digital-Twin
  transition (CMOS-02-DOM-005); prior versions remain resolvable for reproduction.
- **CMOS-10-BILL-031** Rate matching MUST be **longest-prefix on a normalised
  destination** (E.164 for external, opaque route reference for internal;
  CMOS-CONV-016) with deterministic tie-breaking by `priority`. The matched rate key
  MUST be recorded on the CDR (`rate`).
- **CMOS-10-BILL-032** A rating profile MUST define its rounding mode
  (`HALF_UP\|HALF_EVEN\|CEIL\|FLOOR`) and its rounding granularity (to `minor_units`).
  Rounding is applied once, at the final `cost`, not per increment.

### 3.2 Increments & minimums

- **CMOS-10-BILL-033** `billable_ms` MUST be computed as
  `max(minimum_seconds, ceil(duration_ms / 1000 / increment_seconds) * increment_seconds) * 1000`
  using the profile's `increment_seconds` (e.g. `1` for per-second, `60` for
  per-minute) and `minimum_seconds`. `cost = setup_fee + rate_per_minute * billable_ms / 60000`,
  rounded per CMOS-10-BILL-032.
- **CMOS-10-BILL-034** Increment and minimum MUST be applied from the profile, never
  hard-coded. Internal calls MAY rate to zero via an `INTERNAL` charge source with a
  zero rate, but MUST still produce a CDR (CMOS-10-BILL-001).

### 3.3 Corrections & re-rating

- **CMOS-10-BILL-035** Re-rating (e.g. a late carrier correction, a disputed leg) MUST
  emit a **new** CDR with `supersedes_cdr_id` set to the prior CDR and
  `rating_status = RERATED`; the superseded CDR is retained. A `BillingGenerated`
  event MUST accompany the new CDR (§8).

## 4. Charge sources & cost allocation

- **CMOS-10-BILL-040** Every rated leg MUST name its **charge source**
  (`charge_source` enum): `CARRIER`, `MOBILE_GATEWAY`, `SIP_TRUNK`, or
  `INTERNAL_RECHARGE` (see [`model.md`](model.md)). The charge source selects the
  applicable rating profile and the settlement counterparty.
- **CMOS-10-BILL-041** `INTERNAL_RECHARGE` MUST be used for cross-charging between Cost
  Centres of the same Organisation (e.g. a shared trunk billed back to a department).
  An internal recharge CDR references both the originating and receiving
  `cost_centre_id` and MUST net to zero at the Organisation root.
- **CMOS-10-BILL-042** Cost allocation MUST roll a CDR's `cost` up the spine
  Identity → User → Department → Cost Centre → Organisation deterministically. Roll-up
  totals are **derived projections** of immutable CDRs; they MUST be recomputable from
  the CDR ledger alone.
- **CMOS-10-BILL-043** Allocation MUST be stable under Digital-Twin changes: moving a
  User to a different Department after a call MUST NOT retroactively re-allocate that
  User's historical CDRs (attribution is snapshotted, CMOS-10-BILL-010).

## 5. Prepaid, quotas & approvals

- **CMOS-10-BILL-050** An Organisation, Cost Centre, Department, or User MAY carry a
  **prepaid balance** in a stated currency. When a subject is prepaid, the platform
  MUST perform an **authorisation reservation** (hold) at call setup and MUST reject
  setup when the available balance cannot cover the profile's minimum charge.
- **CMOS-10-BILL-051** A **quota** is a periodic ceiling (spend or minutes) on a
  subject over a window (`DAILY\|WEEKLY\|MONTHLY`). When a chargeable action would
  exceed a quota, the platform MUST enforce the quota's `on_exceed` effect:
  `BLOCK`, `REQUIRE_APPROVAL`, or `ALLOW_AND_ALERT`. Quota state MUST be evaluated
  before setup, not only at CDR time.
- **CMOS-10-BILL-052** `REQUIRE_APPROVAL` MUST express a Policy effect
  (`REQUIRE_APPROVAL`, Volume 2 Policy) and MUST record an `AuditEntry` for the
  approval decision. An unapproved call MUST NOT be connected.
- **CMOS-10-BILL-053** A live call that crosses a balance/quota boundary mid-call MUST
  either be allowed to complete (recording the overage on the CDR with
  `tags: ["overage"]`) or be disconnected per the subject's `mid_call_policy`; the
  chosen behaviour MUST be explicit, never implementation-defined.
- **CMOS-10-BILL-054** Balance debits MUST be **idempotent** against the CDR's
  `id`/`idempotency_key` so that at-least-once re-delivery of `BillingGenerated`
  (CMOS-05-EVT-010) never double-charges. (Serves CMOS-05-EVT-011.)

## 6. Authentication-before-charge policy (login-before-chargeable-call)

The legacy "log in before you can dial out" control is generalised to **Identity
authentication as a precondition for chargeable activity**, tying billing to
Volume 9 (Security & Identity).

- **CMOS-10-BILL-060** A Policy MAY declare that a chargeable action requires an
  authenticated Identity (`REQUIRE_IDENTITY`). When it does, the platform MUST verify
  an `ACTIVE` Identity of adequate `assurance_level` on the Device before connecting
  the chargeable leg. (Serves CMOS-00-ENG-011, CMOS-00-ENG-002.)
- **CMOS-10-BILL-061** The required `assurance_level` (`LOW\|MEDIUM\|HIGH`) MAY be
  raised by destination class (e.g. international, premium-rate) or by spend
  threshold. A call whose destination requires `HIGH` assurance held by a `LOW`
  Identity MUST be blocked or re-challenged, never downgraded silently.
- **CMOS-10-BILL-062** Identity expiry/revocation (`IdentityExpired`,
  `IdentityRevoked`) MUST take effect for **future** chargeable setups immediately; it
  MUST NOT retroactively alter CDRs already generated.
- **CMOS-10-BILL-063** `Emergency Override` (CMOS-10-BILL-013) MUST bypass
  `REQUIRE_IDENTITY` for emergency destinations while still producing an attributed-as-
  possible CDR.

## 7. Fraud detection & anomaly signalling

- **CMOS-10-BILL-070** The platform MUST expose fraud-relevant signals as Events and
  CDR fields (call velocity, concurrent-leg count, destination risk class, spend
  rate, off-hours activity, new-destination-first-seen), so that fraud logic MAY be an
  **external consumer** (AI/automation, Volume 11) rather than embedded. (Serves
  CMOS-00-ENG-004, CMOS-00-ENG-013.)
- **CMOS-10-BILL-071** The platform MUST support **hard guardrails** enforced in-band
  independent of any external consumer: per-subject concurrent-call caps, per-window
  spend caps, and a high-risk-destination allowlist/blocklist. These MUST be
  enforceable at setup without waiting on an asynchronous decision.
- **CMOS-10-BILL-072** On a guardrail trip the platform MUST emit a fraud-signal Event,
  record an `AuditEntry`, and apply the configured effect (`BLOCK`, `THROTTLE`,
  `REQUIRE_APPROVAL`, `ALERT`). Suspected-fraud CDRs MUST be tagged
  (`tags: ["fraud_suspected"]`) but never suppressed.
- **CMOS-10-BILL-073** Fraud thresholds and destination risk classes are declarative
  configuration (CMOS-00-ENG-005), tenant-scoped, and versioned as Digital Twins.

## 8. CDR lifecycle & emission (normative)

```
Call events ──▶ CallEnded ──▶ [assemble CDR] ──▶ [attribute] ──▶ [rate] ──▶ CDR persisted
                                                                      │
                                                                      ▼
                                                             BillingGenerated (subject = CDR)
                                                                      │
                                             ┌────────────────────────┼───────────────────────┐
                                             ▼                        ▼                        ▼
                                    balance/quota debit        cost roll-up            external consumers
                                    (idempotent, §5)           (derived, §4)           (AI, webhooks, export)
```

- **CMOS-10-BILL-080** A CDR MUST be generated on **`CallEnded`** (the ENDED
  transition, [catalog](../005-events/catalog.md#call)) and MUST be persisted before
  its `BillingGenerated` event is acknowledged (transactional outbox,
  CMOS-05-EVT-010).
- **CMOS-10-BILL-081** CDR generation MUST publish a **`BillingGenerated`** Event whose
  `subject` is the CDR `id`, carrying the frozen envelope
  ([Volume 5 §2](../005-events/README.md#2-the-envelope-normative)) with the Call's
  `correlation_id` and a stable `idempotency_key` derived from the CDR. The `data`
  payload MUST validate against
  [`BillingGenerated.schema.json`](../../contracts/json-schema/events/BillingGenerated.schema.json).
- **CMOS-10-BILL-082** `BillingGenerated.data` MUST carry Object **references** for
  any recording/transcript (`recording_object_id`, `transcript_object_id`) and MUST
  NOT embed raw media or full transcripts (CMOS-05-EVT-041, CMOS-02-DOM-013).
- **CMOS-10-BILL-083** Consumers of `BillingGenerated` MUST be idempotent on
  (`type`, `idempotency_key`) (CMOS-05-EVT-011); re-delivery MUST NOT double-charge a
  balance or double-count a roll-up (CMOS-10-BILL-054).
- **CMOS-10-BILL-084** A superseding CDR (§3.3) MUST emit its own `BillingGenerated`;
  downstream systems reconcile via `supersedes_cdr_id`.

## 9. Security, privacy & retention

- **CMOS-10-BILL-090** CDR export and balance/quota administration MUST be
  capability-gated (`billing.export`, `billing.admin`, …), not role-implicit
  (CMOS-00-ENG-009).
- **CMOS-10-BILL-091** PII on CDRs and billing events MUST be minimised and marked
  (`x-pii: true`) so downstream redaction is automatable (CMOS-05-EVT-042). Dialled
  external numbers are PII where regulation requires and MUST be redactable on export.
- **CMOS-10-BILL-092** CDR retention MUST follow the Object/retention policy model;
  CDRs are never hard-deleted while under a legal-hold or statutory retention window
  (CMOS-00-ENG-012). Deletion is a state transition to `ARCHIVED`.

## Conformance notes

- **L1 (Contract):** emitted `BillingGenerated` envelopes and `data` validate against
  the frozen schemas; every generated CDR validates against the CDR entity schema and
  carries `rating_profile_id` + `rating_profile_version`.
- **L2 (Behavioural):** for a driven scenario — a Call answered on a shared Device with
  Alice's Identity — the CDR attributes to Alice's User/Cost Centre (CMOS-10-BILL-011);
  identical inputs + profile version reproduce identical `cost` (CMOS-10-BILL-005);
  re-delivering `BillingGenerated` debits the balance exactly once (CMOS-10-BILL-054);
  unanswered outcomes still emit a zero-cost CDR (CMOS-10-BILL-022); a `REQUIRE_IDENTITY`
  policy blocks an unauthenticated chargeable leg (CMOS-10-BILL-060).
- **L3 (Interoperable):** re-rating and roll-up totals reconcile across a real Carrier
  correction feed; cost allocation nets to zero for internal recharge
  (CMOS-10-BILL-041).
- The harness (`conformance/run.py`) checks CDR↔schema and `BillingGenerated`
  catalog↔schema↔example consistency.

## Open items
- Machine-readable **rating-profile** schema (`contracts/json-schema/entities/RatingProfile.schema.json`)
  and a `Balance`/`Quota` entity schema — to be added before freeze.
- Settlement/reconciliation export format (carrier invoice matching) — reserved for v0.4.
- Multi-currency roll-up and FX snapshotting at CDR time — reserved.
- Non-voice workload rating (per-message, per-AI-job metering via Volume 11) — reserved.

## Change log
- **0.3.0** — Initial implementation-grade draft: attribution model, CDR billing
  semantics, rating profiles/increments, charge sources & cost allocation, prepaid/
  quota/approval controls, authentication-before-charge policy, fraud guardrails, and
  the `CallEnded → BillingGenerated` emission lifecycle; builds on `model.md`.
