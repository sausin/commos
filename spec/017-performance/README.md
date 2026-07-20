# Volume 17 — Performance Targets

**Status:** DRAFT · **Version:** 0.3.0 · **Subsystem tag:** PERF

Performance is a **contract**, not a hope. This volume states CommOS's performance
and capacity guarantees as **numeric, normative, measurable** requirements. Each
target has a stable ID, a scope (which topology and hardware it holds on), and a
**measurement method** — the experiment that decides pass/fail. The experiments
themselves are owned by the Volume 16 load and chaos suites (CMOS-16-TEST-060..063,
CMOS-16-TEST-070..075); this volume owns the numbers those experiments assert.

These targets serve the constitution's prime directive — a system is only *simpler
to operate* if it is also **predictably fast** at the scale an operator runs it
(CMOS-00-ENG-001) — and make "horizontal scalability without redesign"
(CMOS-00-ENG-015) falsifiable rather than aspirational.

---

## 1. Measurement principles (normative)

- **CMOS-17-PERF-001** Every latency target is a **percentile**, not a mean. Unless a
  requirement states otherwise, a target value `T` means **p95 ≤ T** measured over a
  stated steady-state window (≥ 10 minutes) at the stated load; p50 and p99 are
  reported alongside. A target verified only by its mean is **not** verified.
- **CMOS-17-PERF-002** Every target declares its **reference topology** (single-binary
  or split-media, Volume 3 §8) and **reference hardware envelope** (§6). A target
  holds only within its declared envelope; outside it, the target is *reported* but
  not *guaranteed*.
- **CMOS-17-PERF-003** Latency is measured **server-side at the API/signalling
  boundary** (request receipt → response emission, or triggering Event → resulting
  Event), excluding client network RTT, unless the requirement is explicitly
  end-to-end. The clock source and measurement point MUST be recorded.
- **CMOS-17-PERF-004** Targets are verified by the Volume 16 load suite as
  **repeatable** experiments with declared dataset size, warm/cold state, and
  concurrency; a target with no passing experiment is **unverified**
  (CMOS-16-TEST-063). Results use the same percentile definitions the observability
  layer exposes (Volume 15), so production telemetry and conformance share one
  definition.
- **CMOS-17-PERF-005** "Normal load" is a **named, fixed** operating point per
  reference deployment (§6): a defined mix of registrations, call attempts/second,
  API QPS, and CDR rate at or below the capacity envelope. Requirements that say
  "under normal load" bind to this point; overload behaviour is governed by §7.

## 2. Control-plane operation latency (normative)

Measured per CMOS-17-PERF-003 on the single-binary reference topology under normal
load unless stated.

- **CMOS-17-PERF-010 — Cold start.** A single-binary node MUST reach ready-to-serve
  (accepting API requests and SIP registrations) in **< 2 s** from process start,
  given a reachable, migrated PostgreSQL. *Method:* start the process; measure to
  first successful health probe; **max ≤ 2 s** over ≥ 20 cold starts. Fast start is
  what makes the single-artifact default (CMOS-00-ENG-014) and rolling upgrades
  (CMOS-17-PERF-070) operationally cheap.
- **CMOS-17-PERF-011 — Create User.** `POST /v1/users` MUST return in **p95 < 100 ms**
  (p99 < 250 ms), emitting `UserCreated` (Volume 5) within the same budget. *Method:*
  closed-loop driver at normal API QPS; server-side timing.
- **CMOS-17-PERF-012 — Provision a phone.** From operator approval (`DeviceApproved`)
  to the Device reaching OPERATIONAL (`RegistrationSucceeded`), zero-touch, MUST
  complete in **< 30 s** wall-clock. *Method:* drive the Device state machine
  (Volume 2) end-to-end against a virtual Device; measure
  `DeviceApproved`→`RegistrationSucceeded`; excludes human approval latency and vendor
  reboot time beyond the Device's rated boot window, which is recorded separately.
- **CMOS-17-PERF-013 — General read API.** A single-entity `GET` (e.g.
  `GET /v1/devices/{id}`) MUST return in **p95 < 50 ms** (p99 < 150 ms) under normal
  load with a warm cache.
- **CMOS-17-PERF-014 — General write API.** A single-entity mutation without a specific
  target MUST return in **p95 < 150 ms** (p99 < 400 ms), including the
  transactional-outbox write of its Event (CMOS-03-ARCH-030). Outbox persistence is
  **inside** the budget, never deferred to appear faster.

## 3. Call setup & signalling latency (normative)

- **CMOS-17-PERF-020 — Internal call setup.** For an on-net Call between two registered
  Devices, the interval from `CallStarted` to `CallRinging` (destination alerted) MUST
  be **p95 < 150 ms** (p99 < 300 ms) under normal load. *Method:* drive the Call state
  machine (Volume 2); measure the Event interval by `sequence`, not receipt time
  (CMOS-05-EVT-020).
- **CMOS-17-PERF-021 — Post-answer media cut-through.** From `CallAnswered` to
  bidirectional RTP flowing, latency MUST be **p95 < 200 ms** so the first word is not
  clipped. Media never traverses the control plane (CMOS-03-ARCH-003), so this is a
  media-plane measurement.
- **CMOS-17-PERF-022 — Routing decision.** Resolving a Route/CallFlow/IVR/Queue to a
  destination MUST add **p95 < 20 ms** to setup, excluding any external dependency the
  Call Flow explicitly invokes (attributed to that dependency).
- **CMOS-17-PERF-023 — Registration processing.** A SIP REGISTER MUST be processed
  (accepted/challenged, `RegistrationSucceeded` emitted) in **p95 < 50 ms** under the
  registration-churn point of §4.

## 4. Capacity & scale (normative)

Capacity is a **guaranteed floor** on the reference hardware envelope (§6), holding
all latency targets of §2–§3 simultaneously.

- **CMOS-17-PERF-030 — Concurrent registrations.** A reference deployment MUST sustain
  **≥ 10,000 concurrent registered Devices** with active refresh, while holding
  CMOS-17-PERF-023. *Method:* ramp to 10,000, hold ≥ 10 min with realistic refresh
  intervals; assert no registration loss and all latency budgets.
- **CMOS-17-PERF-031 — Extensions per deployment.** A reference deployment MUST support
  **≥ 100,000 Extensions** (dialable addresses) across its tenants without degrading
  routing latency (CMOS-17-PERF-022) or read latency (CMOS-17-PERF-013). Extensions are
  labels, not identities (CMOS-00-ENG-002); verified with a seeded 100,000-Extension
  dataset.
- **CMOS-17-PERF-032 — CDR throughput.** A reference deployment MUST assemble, persist,
  and make queryable **≥ 1,000,000 CDR/day** (sustained ≥ ~11.6 CDR/s, and ≥ 3× that
  at peak) with no CDR loss, each CDR attributable to Device + User + Organisation
  (CMOS-00-ENG-011). *Method:* drive Call completions at sustained and peak rates for
  ≥ 1 h; assert count integrity against emitted `CallEnded`/`BillingGenerated` Events.
- **CMOS-17-PERF-033 — Concurrent Calls.** The reference envelope MUST publish a
  guaranteed concurrent-Call floor per media-node profile (§6) with all §3 latency and
  §5 media budgets held; the number is topology- and hardware-specific and is recorded
  in the envelope table (§6), not fixed here.
- **CMOS-17-PERF-034 — Linear control-plane scale-out.** Adding stateless control-plane
  nodes MUST increase sustained API/registration throughput **near-linearly** (≥ 0.8×
  per-node efficiency to at least 8 nodes) with no code change, because control-plane
  services are stateless (CMOS-03-ARCH-010/011, CMOS-00-ENG-015). *Method:* measure
  throughput at N and 2N nodes against shared PostgreSQL/Redis; assert efficiency and
  unchanged per-request latency percentiles.
- **CMOS-17-PERF-035 — Independent media scale-out.** In split-media topology, media
  capacity MUST scale by adding media nodes without changing control-plane logic
  (CMOS-03-ARCH-002); the concurrent-Call floor MUST grow near-linearly with media
  nodes.

## 5. Media quality budgets (normative)

Media budgets are tied to Mean Opinion Score (MOS) and correlate to the Volume 15
media-quality metrics; the same definitions are used in production telemetry and in
conformance load tests (CMOS-16-TEST-062).

- **CMOS-17-PERF-040 — Zero packet loss under normal load.** On the media path, under
  normal load (CMOS-17-PERF-005), platform-induced packet loss MUST be **0%** (loss
  attributable to the CommOS media plane, distinct from access-network loss).
  *Method:* instrumented RTP endpoints; assert zero platform-side drops over the
  steady-state window.
- **CMOS-17-PERF-041 — One-way latency.** Platform-added one-way media latency (ingress
  to egress through the media plane, excluding access network) MUST be **≤ 40 ms** p95.
  Combined with a typical access network this keeps end-to-end mouth-to-ear within the
  ITU-T G.114 comfort zone.
- **CMOS-17-PERF-042 — Jitter.** Post-jitter-buffer jitter introduced by the platform
  MUST be **≤ 15 ms** p95.
- **CMOS-17-PERF-043 — MOS floor.** For a supported wideband codec under normal load,
  the platform MUST sustain an estimated **MOS ≥ 4.0** (and ≥ 3.6 for narrowband),
  computed from the loss/latency/jitter budgets above per the Volume 15 model. A
  measured MOS below floor at or under normal load is a performance defect.

## 6. Reference envelopes & topologies (normative)

- **CMOS-17-PERF-050** Each guaranteed floor MUST be published against a named
  **reference envelope**: topology (single-binary vs split-media, Volume 3 §8 /
  Volume 14), node CPU/RAM class, PostgreSQL class, and the Redis/NATS layer. A
  conformance claim of a PERF requirement MUST cite the envelope it was measured on.
- **CMOS-17-PERF-051** The **single-binary** envelope (one node + PostgreSQL, optional
  Redis; CMOS-00-ENG-014) MUST meet CMOS-17-PERF-010..014, -020..023, -030..032, and
  -040..043 at the small-business normal-load point. It is the default and the floor
  every implementation MUST demonstrate.
- **CMOS-17-PERF-052** The **split-media** envelope (control-plane nodes + dedicated
  media nodes, shared state) MUST additionally demonstrate CMOS-17-PERF-034/035 and
  raise the concurrent-Call and CDR floors per added node, with the **observable
  contracts identical** to single-binary (CMOS-03-ARCH-060) — deployment shape MUST
  NOT change API/Event behaviour or latency percentiles at equal load.

| Envelope (informative defaults, refined in v0.4) | Topology | Guarantees |
|---|---|---|
| `sb-small` | single binary, 4 vCPU / 8 GB + PostgreSQL | full §2–§5 at small-business normal load; 10k regs, 100k ext, 1M CDR/day |
| `split-media` | ≥ 2 control nodes + ≥ 1 media node | §2–§5 **plus** near-linear scale-out (§4.34/35), higher concurrent-Call & CDR floors |

## 7. Degradation & overload (normative)

- **CMOS-17-PERF-060 — Graceful degradation.** Beyond the capacity envelope the system
  MUST shed or queue load **predictably** — bounded latency growth and explicit
  backpressure / `429` / retry-after (Volume 4), never unbounded latency, deadlock, or
  silent Event loss. At-least-once delivery and the outbox guarantee hold under
  overload (CMOS-05-EVT-010, CMOS-03-ARCH-030).
- **CMOS-17-PERF-061 — No head-of-line failure.** Overload in one tenant or one workload
  MUST NOT breach the latency targets for others beyond a stated fairness bound (tenant
  isolation extends to performance; CMOS-00-ENG-008).
- **CMOS-17-PERF-062 — Recovery.** After load returns to normal, all §2–§5 targets MUST
  be met again within a bounded recovery window (≤ 60 s), with no residual backlog
  loss.

## 8. Upgrade & availability (normative)

- **CMOS-17-PERF-070 — Rolling upgrade, no dropped calls.** A rolling upgrade across
  control-plane nodes MUST complete with **zero dropped established Calls** and zero
  lost committed Events, under a live call/registration load. *Method:* the Volume 16
  chaos harness (CMOS-16-TEST-074) drapes load across the upgrade window and asserts
  zero drops; mixed-version nodes interoperate via the tolerant-reader rule
  (CMOS-CONV-004, CMOS-16-TEST-075).
- **CMOS-17-PERF-071 — Failover continuity.** Loss of a control-plane node MUST NOT drop
  in-progress operations beyond the in-flight request, since state is external
  (CMOS-03-ARCH-010/011); any surviving node serves the next request. Media-node loss
  fails over or ends the Call cleanly without control-plane media state
  (CMOS-03-ARCH-012). Verified by CMOS-16-TEST-070..073.
- **CMOS-17-PERF-072 — Migration cost.** A schema migration on the reference dataset
  MUST fit the maintenance/upgrade budget (bounded, online-where-possible) so
  CMOS-17-PERF-070 remains achievable; long-running migrations MUST be expand/contract,
  not blocking.

## Conformance notes

- Each PERF requirement is verified by a Volume 16 experiment: §2–§5 by the load suite
  (CMOS-16-TEST-060..063), §7–§8 by the chaos suite (CMOS-16-TEST-070..075). A PERF
  requirement without a passing experiment is **unverified**, reported as a coverage
  gap (CMOS-16-TEST-002).
- Targets are guarantees **within a cited reference envelope** (§6). A claim MUST name
  the envelope; the same number outside the envelope is informative.
- Percentiles use the Volume 15 definitions so production SLO telemetry and conformance
  results are directly comparable.

## Open items

- Numeric concurrent-Call floors per media-node profile and per codec (§4, CMOS-17-PERF-033)
  — requires the media-plane detail of Volume 7 and hardware baselines.
- Formal reference-hardware classes and the fixed "normal load" mixes per envelope
  (candidate for a machine-readable `perf-envelopes` artifact in v0.4).
- MOS estimation model reference (E-model parameters) — shared definition with Volume 15.
- Fairness bound constant for CMOS-17-PERF-061.

## Change log

- **0.3.0** — Initial implementation-grade draft. Set numeric normative targets for
  cold start (< 2 s), create-user (< 100 ms), provision-a-phone (< 30 s), internal call
  setup (< 150 ms) and media quality; capacity floors for 10k registrations, 100k
  Extensions, 1M CDR/day, zero packet loss, and no-dropped-call rolling upgrade;
  p50/p95/p99 discipline; single-binary vs split-media envelopes with near-linear
  scale-out; overload/degradation and upgrade/availability requirements. Each target
  carries a measurement method and an ID CMOS-17-PERF-001…072, tied to the Volume 16
  load/chaos suites.
