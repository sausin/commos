# Volume 16 — Testing & Conformance

**Status:** REVIEW · **Version:** 0.4.0 · **Subsystem tag:** TEST

This volume operationalises the project's founding claim: **the specification and
its contracts are the strategic asset, not any single implementation**
(Volume 0 §1). In a specification-first project the tests *are* the executable
form of the specification. An artifact is not "CommOS" because it resembles one;
it is CommOS because it **passes the conformance suite for its declared profile
and level** (CONVENTIONS §3). This volume defines that suite, the harness that
runs it, and the ladder from contract validation (L1) through behavioural (L2) to
interoperability (L3), plus the non-functional test regimes (load, chaos/failover,
interop matrices, regression, security) on which a production claim depends.

The conformance harness described here is the **real, existing** harness at
[`conformance/run.py`](../../conformance/run.py) with its README at
[`conformance/README.md`](../../conformance/README.md). This volume specifies how
that harness is described and **extended**; it does not define a parallel testing
system.

---

## 1. Principles (normative)

- **CMOS-16-TEST-001** Conformance is defined **against the frozen contracts**
  (`contracts/`) and the normative requirements of the specification, **never
  against a reference implementation's behaviour**. No test SHALL assert an
  implementation detail (internal structure, language, storage engine) that is not
  a stated normative requirement. This preserves multiple interoperable
  implementations (CMOS-00-ENG-001; Volume 0 §1, N-6).
- **CMOS-16-TEST-002** Every conformance test **SHOULD** cite the requirement
  ID(s) `CMOS-<VOL>-<SUBSYS>-NNN` it exercises, in a machine-readable form (§7). A
  normative requirement with **zero** citing tests is reported as a **coverage
  gap** and MUST NOT be counted as conformance-covered for the purpose of freezing
  its volume (CONVENTIONS §5).
- **CMOS-16-TEST-003** A conformance claim MUST take the canonical form
  `CommOS <profile> conformant, level <Lx>, spec vX.Y` (CONVENTIONS §3) and MUST be
  reproducible: the exact harness version, contract version, and suite result MUST
  be recorded with the claim.
- **CMOS-16-TEST-004** Tests MUST be **deterministic** or explicitly quarantined. A
  test whose outcome depends on wall-clock timing, network jitter, or ordering not
  guaranteed by the spec MUST either inject a controlled clock/seed or be moved to
  a separately-reported non-gating tier (load/chaos, §8–§9). A flaky gating test is
  a specification defect, not an accepted cost.
- **CMOS-16-TEST-005** The suite MUST be **self-testing**: the contract-level suite
  verifies that every Event/entity named in a `REVIEW`/`FROZEN` volume has a schema
  and example and vice-versa (§4), so specification drift is caught by the harness
  rather than by review (executable form of CONVENTIONS §8).

## 2. The conformance ladder (normative)

The three levels of CONVENTIONS §3 map to three concentric test surfaces. Higher
levels **include** lower ones.

| Level | Surface under test | Oracle | Gate |
|-------|--------------------|--------|------|
| **L1 Contract** | Emitted/consumed shapes: Event envelopes, `data` payloads, API bodies, entities | JSON Schema 2020-12 + OpenAPI | `conformance/run.py` today (§4) |
| **L2 Behavioural** | State transitions, Event **set/order/idempotency**, policy outcomes | Volume 2 state machines + Volume 5 guarantees (§5) | scenario suite (§5) |
| **L3 Interoperable** | Real SIP endpoints, real vendor Devices, cross-implementation Event exchange | live protocol + physical/virtual Devices + a second implementation (§6) | interop harness + matrices (§6, §10) |

- **CMOS-16-TEST-010** An implementation MUST NOT advertise a level it has not
  demonstrably passed for the **whole** declared profile. Partial passes are
  reported per-requirement but do not confer the level.
- **CMOS-16-TEST-011** L2 presupposes L1 and L3 presupposes L2 for the same
  profile. A regression that drops a lower level invalidates the higher claim.

## 3. Test taxonomy (informative overview)

Six regimes, three gating (part of a conformance claim) and three assurance
(required for a *production* claim but not for the conformance label):

- **Gating:** L1 contract (§4), L2 behavioural (§5), L3 interoperability (§6).
- **Assurance:** load/capacity (§8, tied to Volume 17), chaos/failover (§9,
  asserts Volume 3 statelessness + outbox), security (§11). Interop/vendor matrices
  (§10) and regression (§12) span both.

## 4. L1 — the contract suite (`conformance/run.py`) (normative)

The existing harness is the **L1 gate**. It runs three suites over `contracts/`
and `spec/` and exits `0` on pass, non-zero on failure (CI-ready):

1. **schema-validity** — every file under `contracts/json-schema/` is a valid JSON
   Schema (2020-12) and all cross-file `$ref`s resolve through the registry keyed
   by `$id`.
2. **consistency** — the Volume 5 event catalogue
   ([`catalog.md`](../005-events/catalog.md)), the event schemas, and the event
   examples are in 1:1 agreement; every domain entity schema has an example; the
   OpenAPI document
   ([`commos.openapi.yaml`](../../contracts/openapi/commos.openapi.yaml)) parses
   and declares a 3.x version.
3. **examples-valid** — every example instance validates against its schema.

- **CMOS-16-TEST-020** Contract self-consistency (suites 1–3 green) is the gate for
  promoting a spine volume from `REVIEW` to `FROZEN` (CONVENTIONS §5): the shapes a
  volume defines MUST exist in `contracts/` and the harness MUST be green.
- **CMOS-16-TEST-021** The harness MUST be runnable with
  `python3 conformance/run.py` after `pip install jsonschema` (with `pyyaml` for
  full OpenAPI parse), require no network access, and be hermetic against the
  repository checkout alone.
- **CMOS-16-TEST-022** L1 conformance of a **running implementation** is
  established by pointing the same validators at that implementation's live
  surfaces — `GET /v1/openapi.json`, the schema-registry endpoint
  (`GET /v1/events/schemas`, Volume 4), and a captured Event stream — and
  asserting: emitted envelopes validate against
  [`envelope.schema.json`](../../contracts/json-schema/envelope.schema.json), each
  `data` validates against `events/<Type>.schema.json`, and API bodies validate
  against the OpenAPI shapes (CMOS-05-EVT-004, CMOS-05-EVT-005). This **live
  adapter** is the next addition to the harness (§7), reusing the suite-1/3
  validators unchanged.
- **CMOS-16-TEST-023** The tolerant-reader rule is itself tested: the L1 adapter
  MUST feed a consumer an envelope carrying an unknown field and a higher
  same-MAJOR `specversion` and assert it is accepted (CMOS-CONV-004,
  CMOS-05-EVT-004).

## 5. L2 — the behavioural suite (normative)

L2 derives scenarios directly from the Volume 2 state machines
([`state-machines.md`](../002-domain-model/state-machines.md)) and the Volume 5
delivery/ordering/idempotency guarantees. Scenarios live under
`conformance/scenarios/` and drive an implementation through the **live adapter**
(§7).

- **CMOS-16-TEST-030** For every entity with a normative state machine (Device,
  Call, Identity, CallFlow, User, Gateway, AIJob), the suite MUST exercise, for
  each **legal** transition, that the transition is accepted and that **exactly**
  the named Event is emitted (CMOS-02-DOM-007, CMOS-05-EVT-002). No unspecified
  Event may be emitted for that transition.
- **CMOS-16-TEST-031** For each **illegal** transition (any source→target pair not
  listed in the state machine) the suite MUST assert the implementation **rejects**
  it and does not emit the target-state Event.
- **CMOS-16-TEST-032** For a driven scenario, the suite MUST assert the **exact
  set** and **order** of Events: events sharing a `correlation_id` are totally
  ordered by `sequence` and MUST be observed in `sequence` order, not receipt order
  (CMOS-05-EVT-020..022). Assertion is on `sequence`, never on wall-clock arrival.
- **CMOS-16-TEST-033** **Idempotency** MUST be tested by replaying every Event in a
  scenario to an idempotent consumer and asserting a **no-op** (de-duplication on
  (`type`, `idempotency_key`) or `id`; CMOS-05-EVT-011, CMOS-05-EVT-022), and by
  replaying an idempotent **command** (same `Idempotency-Key`) and asserting a
  single effect (Volume 4).
- **CMOS-16-TEST-034** **Correlation** MUST be tested: all events and commands of
  one logical operation share one `correlation_id`, and `causation_id` chains back
  to the causing event/command (Volume 5 §2).
- **CMOS-16-TEST-035** Policy-gated transitions MUST be tested for both outcomes:
  e.g. a chargeable leg reaching `ANSWERED` requires a resolvable Identity, else the
  attempt yields `CallRejected` with a policy hangup cause (CMOS-02-DOM-010); and an
  Emergency Override bypasses the identity requirement (Volume 9).
- **CMOS-16-TEST-036** **Multi-tenancy** MUST be tested negatively: a subscription
  or API caller scoped to tenant A MUST NOT observe events or entities of tenant B
  (CMOS-05-EVT-040, CMOS-03-ARCH-050, CMOS-00-ENG-008). Cross-tenant leakage is a
  gating failure.
- **CMOS-16-TEST-037** Each scenario file MUST declare the requirement IDs it
  exercises (§7) and the state-machine transition(s) it covers, so coverage of the
  state machines is computable.

> Note (informative): scenarios are expressed as declarative fixtures (given a
> sequence of commands, expect this ordered set of events and these rejected
> transitions), not as implementation code, so a single scenario runs unchanged
> against any implementation via the adapter.

## 6. L3 — interoperability (normative)

L3 replaces mocked transports and mocked peers with **real** ones.

- **CMOS-16-TEST-040** **Real SIP endpoints.** The `voice` profile at L3 MUST
  complete the Call state machine against at least one independent third-party SIP
  user-agent and one SIP Trunk/Carrier over the wire: registration, INVITE/answer,
  hold/resume, blind and attended transfer, and BYE, each producing the Volume 5
  Events in `sequence` order. SIP is one transport (CMOS-00-ENG-016); WebRTC
  endpoints MUST be tested as a peer transport at the same level.
- **CMOS-16-TEST-041** **Real vendor Devices.** The `provisioning` profile at L3
  MUST drive at least one physical or vendor-faithful virtual Device of each
  supported family through the full Device state machine — DETECTED → PENDING →
  APPROVED → PROVISIONED → OPERATIONAL, plus REPLACING and RETIRED — using
  zero-touch signed-URL config delivery (CMOS-00-ENG-010) and asserting the
  `Provisioning/*` and `Registration/*` Events.
- **CMOS-16-TEST-042** **Cross-implementation Event exchange.** Two independent
  CommOS implementations MUST interoperate on the Event Bus and API: events emitted
  by implementation A MUST validate and be consumed idempotently by implementation
  B, and a federated/cross-org Call spanning both MUST yield a single coherent
  `correlation_id` timeline. This is the strongest evidence the contract — not an
  implementation — is the standard (Volume 0 §1).
- **CMOS-16-TEST-043** L3 results MUST be recorded in the compatibility matrices
  (§10) with the exact firmware/UA versions and dates; a matrix cell is valid only
  while the cited versions are unchanged.

## 7. Test↔requirement traceability & the adapter (normative)

- **CMOS-16-TEST-050** Every gating test MUST carry a machine-readable list of the
  requirement IDs it exercises (a `covers:` field in the scenario fixture or a
  structured annotation). The harness MUST emit a **coverage report** mapping each
  requirement ID to its citing tests and MUST flag requirements with no coverage
  (CMOS-16-TEST-002).
- **CMOS-16-TEST-051** The **implementation adapter** (planned
  `conformance/adapter/`) is the single seam between the specification-derived
  suites and an implementation under test. It MUST expose only contract surfaces —
  issue API commands, read the live OpenAPI and schema registry, and subscribe to
  the Event stream — and MUST NOT reach into implementation internals. Swapping the
  adapter is the only change needed to certify a different implementation.
- **CMOS-16-TEST-052** The behavioural fixtures (`conformance/scenarios/`) and the
  adapter (`conformance/adapter/`) are added to the **existing** harness layout
  (`conformance/run.py` + `README.md`) as L2/L3 land; they extend it and MUST NOT
  fork a second harness.

## 8. Load & capacity testing (normative)

Load tests are the measurement method for the Volume 17 targets; this volume
defines how they are run, Volume 17 defines the numbers.

- **CMOS-16-TEST-060** The load suite MUST reproduce the Volume 17 capacity
  envelopes as **repeatable** experiments with declared hardware, topology
  (single-binary vs split-media, Volume 3 §8 / Volume 14) and dataset size, and
  MUST report latency **percentiles** (p50/p95/p99), throughput, and error rate —
  never means alone.
- **CMOS-16-TEST-061** The suite MUST include the headline capacity scenarios:
  10,000 concurrent Registrations, 100,000 Extensions, and sustained 1,000,000
  CDR/day, and MUST assert the corresponding Volume 17 requirements hold at those
  scales (CMOS-17-PERF targets).
- **CMOS-16-TEST-062** Steady-state load MUST assert **zero packet loss** on the
  media path under the normal-load definition (Volume 17) and MUST hold one-way
  latency/jitter/loss within the MOS-tied budgets, correlating to the observability
  media-quality metrics (Volume 15).
- **CMOS-16-TEST-063** Load results are **assurance**, reported separately, and do
  not by themselves confer a conformance level; but a Volume 17 requirement with no
  passing load experiment MUST be reported as unverified (CMOS-16-TEST-002 applied
  to PERF IDs).

## 9. Chaos & failover testing (normative)

Chaos tests assert the architectural invariants of Volume 3 hold under fault.

- **CMOS-16-TEST-070** **Statelessness.** Killing any control-plane node
  mid-operation MUST NOT lose a committed state change and MUST allow any surviving
  node to serve the next request for the affected tenant, because control-plane
  services hold no durable state between requests (CMOS-03-ARCH-010,
  CMOS-03-ARCH-011; CMOS-00-ENG-015). The test asserts continuity via the API and
  the Event timeline, not via internal state.
- **CMOS-16-TEST-071** **Outbox guarantee.** Injecting a crash **between** the
  committed state change and the Event Bus relay MUST result in the Event being
  delivered after recovery (at-least-once), never silently dropped
  (CMOS-03-ARCH-030, CMOS-05-EVT-010). The test commits a change, crashes the relay,
  restarts, and asserts the Event appears with its original `idempotency_key`,
  consumed as a no-op if already seen.
- **CMOS-16-TEST-072** **Dead-letter.** An endpoint made permanently undeliverable
  MUST, after the bounded retry budget, route its Events to the dead-letter stream
  and never drop them (CMOS-05-EVT-013, CMOS-03-ARCH-031).
- **CMOS-16-TEST-073** **Media failover.** Killing a media node mid-Call MUST NOT
  require the control plane to have held media state; the Call either fails over or
  ends cleanly with the correct terminal Event, and no control-plane node is left
  holding media state (CMOS-03-ARCH-012).
- **CMOS-16-TEST-074** **Rolling upgrade.** A rolling upgrade across control-plane
  nodes MUST complete with **no dropped established Calls** and no lost committed
  Events (the Volume 17 rolling-upgrade target); the chaos harness drapes a live
  call/registration load across the upgrade window and asserts zero drops.
- **CMOS-16-TEST-075** **Tolerant-reader under version skew.** During upgrade,
  mixed-version nodes MUST interoperate: a lower-MINOR node MUST consume a
  higher-MINOR same-MAJOR envelope without rejecting it (CMOS-CONV-004).

## 10. Interop & vendor compatibility matrices (normative)

- **CMOS-16-TEST-080** The project MUST maintain a **SIP interoperability matrix**
  (SIP user-agents, Trunks/Carriers) and a **vendor Device compatibility matrix**
  (Device family × firmware × Provisioner) recording, per cell, the last passing L3
  run, the exact versions, and the date. A cell without a dated passing run is
  **untested**, not **passing**.
- **CMOS-16-TEST-081** Each matrix cell MUST cite the L3 requirement IDs
  (CMOS-16-TEST-040/041) it satisfies and link its run artifacts, so a claim of
  "supports vendor X" is falsifiable.
- **CMOS-16-TEST-082** Provisioner plugins (Volume 8/12) MUST be tested against
  their declared Device families; a firmware bump invalidates the cell until re-run
  (CMOS-16-TEST-043).

## 11. Security testing (normative)

- **CMOS-16-TEST-090** **Tenant isolation** MUST be fuzzed: cross-tenant access
  attempts across every API resource and Event subscription MUST be denied (defence
  in depth: data-layer scoping **and** API/Policy re-check; CMOS-03-ARCH-050,
  CMOS-00-ENG-008). A single passing bypass is a release blocker.
- **CMOS-16-TEST-091** **AuthZ** MUST be tested as capability-based: an action MUST
  be denied without its Capability and allowed with it (CMOS-00-ENG-009); role
  bundles are tested only as capability sets.
- **CMOS-16-TEST-092** **Zero-trust provisioning** MUST be tested: expired or reused
  one-time tokens, unsigned or tampered config URLs, and unapproved Devices MUST all
  be rejected (CMOS-00-ENG-010).
- **CMOS-16-TEST-093** **Webhook signatures** MUST be tested: an unsigned or
  wrong-HMAC delivery MUST be rejected and MUST emit `WebhookDeliveryFailed`
  (CMOS-05-EVT-014).
- **CMOS-16-TEST-094** **Event payload hygiene** MUST be tested: no schema or
  emitted payload carries secrets or raw media/recordings — only Object references —
  and fields marked `x-pii: true` are the only PII carriers (CMOS-05-EVT-041,
  CMOS-05-EVT-042).
- **CMOS-16-TEST-095** Dependency and supply-chain scanning for the reference stack
  is specified in Volume 18 (`cargo audit`, licence and provenance checks); this
  volume asserts those gates run in CI (Volume 18 §CI/CD).

## 12. Regression (normative)

- **CMOS-16-TEST-100** Every fixed defect MUST add a regression test that cites the
  requirement ID it protects; if the defect exposed a gap in the spec, the spec
  gains a requirement first and the test cites the new ID.
- **CMOS-16-TEST-101** A contract change MUST NOT merge if it breaks the
  compatibility rules encoded in the suite (CMOS-CONV-001..005): removing/narrowing a
  field, tightening a constraint, or renaming a type without a MAJOR bump is a gating
  failure (CMOS-05-EVT-030/031).
- **CMOS-16-TEST-102** The full gating suite (L1, plus L2/L3 as they land) MUST run
  in CI on every push and pull request; the contract job at
  [`.github/workflows/conformance.yml`](../../.github/workflows/conformance.yml) is
  the current gate (Volume 18 owns CI/CD policy).

## Conformance notes

- This volume defines the **meta-conformance** rules: how any other volume's
  requirements are demonstrated. Its own "conformance" is that the harness at
  `conformance/run.py` runs green (L1) and that the coverage report
  (CMOS-16-TEST-050) shows no un-cited normative requirement in a `FROZEN` volume.
- L1 is live today via `run.py`; L2 (`scenarios/`) and L3 (`adapter/` + matrices)
  extend the same harness and are gated as they land (CMOS-16-TEST-052).
- Load (§8) and chaos (§9) are assurance tiers: they verify Volume 17 and Volume 3
  guarantees respectively and are reported alongside, not folded into, the
  conformance-level label.

## Open items

- Schema of the `covers:` traceability annotation and the coverage-report output
  format (candidate for `conformance/` in v0.4).
- Reference SIP user-agent set and virtual-Device images for L3 (Volume 7/8 input).
- Chaos-injection tool binding (process kill, network partition) — pluggable like
  the bus binding (Volume 3 §4).
- Formal control↔media IDL to test the plane boundary directly once it lands
  (Volume 3 open item).

## Change log

- **0.3.0** — Initial implementation-grade draft. Defined the L1/L2/L3 ladder over
  the existing `conformance/run.py` harness, behavioural scenarios derived from the
  Volume 2 state machines and Volume 5 guarantees, L3 interoperability (real SIP,
  vendor Devices, cross-implementation exchange), the load/chaos/security/regression
  regimes, interop & vendor matrices, and test↔requirement traceability with a
  coverage report. Assigned IDs CMOS-16-TEST-001…102.
