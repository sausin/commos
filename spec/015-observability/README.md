# Volume 15 — Observability

**Status:** REVIEW · **Version:** 0.4.0 · **Subsystem tag:** OBS

Observability is how the operability mandate (CMOS-00-ENG-001) is verified in
production: a system that cannot be seen cannot be operated simply. This volume
specifies the four signal classes — **metrics, structured logs, distributed traces,
and health/readiness** — plus **media-quality telemetry**, **diagnostic bundles**,
and the **expert-mode SIP trace / PCAP** surface. Observability is not an add-on: the
event model (Volume 5) already carries the correlation and trace context that binds
these signals together, and this volume makes that correlation normative end to end.
Everything here honours progressive complexity (Volume 0 §5): default views speak
golden signals and call quality; protocol-level traces are opt-in expert surfaces
(N-5).

---

## 1. Scope

In scope: metric exposition, log structure, trace propagation, health/readiness
endpoints, media-quality (MOS/jitter/loss/latency) telemetry, diagnostic/debug
bundles, SIP traces and PCAP capture, golden signals, SLOs, and correlation across all
signal classes. Out of scope: the numeric SLO/latency **targets** themselves (Volume
17, which this volume feeds), the event envelope definition (Volume 5), and the
security controls governing who may read telemetry (Volume 9 — referenced where
telemetry is sensitive).

## 2. Correlation model (normative)

Correlation is the spine of this volume: it is what lets an operator pivot from a
metric spike to the exact logs, traces, and events of one Call.

- **CMOS-15-OBS-001** Every signal — metric exemplar, log line, trace span, and
  emitted Event — that arises from one logical operation MUST carry the operation's
  **`correlation_id`** (Glossary; CMOS-05-EVT envelope). For a Call this is the Call's
  `correlation_id`; the same value appears on the API responses (`X-Correlation-Id`,
  CMOS-04-API-042) and on every `Call/*` event.
- **CMOS-15-OBS-002** Distributed tracing MUST use **W3C Trace Context**: the
  `traceparent` (and, where used, `tracestate`) value MUST be propagated across
  process and plane boundaries and MUST be carried on the event envelope's
  `traceparent` field (CMOS-05-EVT envelope). A trace MUST span the control→media
  command path and the media→control fact path (CMOS-03-ARCH-003) so one Call is a
  single connected trace.
- **CMOS-15-OBS-003** Given a `correlation_id`, an operator MUST be able to retrieve
  the correlated logs, the trace, and the ordered events (`sequence`, CMOS-05-EVT-020)
  for that operation; the three signal classes MUST be mutually navigable, not siloed.
- **CMOS-15-OBS-004** Telemetry MUST be **tenant-scoped**: every signal that pertains
  to a tenant MUST carry `tenant_id`, and access to a tenant's telemetry MUST be
  capability-gated (CMOS-00-ENG-008 / CMOS-05-EVT-040). Cross-tenant telemetry leakage
  MUST be impossible by construction.

## 3. Metrics (normative)

- **CMOS-15-OBS-010** The platform MUST expose metrics in **Prometheus /
  OpenMetrics** text exposition over an HTTP endpoint (reference: `/metrics`),
  scrapable without authentication coupling to the tenant API, and MUST document every
  metric's name, type, unit, and labels.
- **CMOS-15-OBS-011** Metric label cardinality MUST be **bounded**: `correlation_id`,
  Call id, or other high-cardinality per-operation identifiers MUST NOT be metric
  labels. Per-operation correlation is carried by **exemplars** (OpenMetrics
  exemplars referencing a `trace_id`) or by logs/traces, not by label explosion.
- **CMOS-15-OBS-012** The platform MUST expose the **golden signals** per subsystem —
  request/Call **rate**, **errors**, **duration/latency**, and **saturation** — and
  MUST distinguish control-plane request metrics from media-plane Call/stream metrics
  (CMOS-03-ARCH-001). Latency metrics MUST be histograms enabling percentile SLOs (§9).
- **CMOS-15-OBS-013** Metric names, types, and units are a **compatibility surface**:
  renaming or retyping an exposed metric follows the SemVer contract rules
  (CONVENTIONS §4) — additive within a MINOR, breaking only in a MAJOR.

## 4. Structured logging (normative)

- **CMOS-15-OBS-020** Logs MUST be **structured** (machine-parseable key/value; JSON in
  the reference implementation), not free-form text. Each entry MUST carry at minimum
  `time` (CMOS-CONV-011), `level`, `message`, `source` subsystem, and — where
  applicable — `tenant_id`, `correlation_id`, and `trace_id`/`span_id` (§2).
- **CMOS-15-OBS-021** Log output MUST integrate cleanly with **systemd/journald** for
  the single-binary deployment and with stdout for the container deployment
  (CMOS-14-DEP-002/003) without requiring a bespoke log shipper.
- **CMOS-15-OBS-022** Logs MUST NOT contain secrets, credentials, or raw media, and
  PII MUST be minimised and redactable (mirrors CMOS-05-EVT-041/042). Fields carrying
  PII SHOULD be marked so downstream redaction can be automated.
- **CMOS-15-OBS-023** Log verbosity MUST be runtime-adjustable per subsystem without a
  restart, and raising verbosity MUST NOT change platform behaviour (observation is
  side-effect-free; parallels Expert Mode, CMOS-13-UI-011).

## 5. Distributed tracing (normative)

- **CMOS-15-OBS-030** The platform MUST emit **distributed traces** using W3C Trace
  Context (CMOS-15-OBS-002) and SHOULD export in an **OpenTelemetry (OTLP)**-compatible
  form so any conforming backend can ingest them. The platform MUST NOT hard-code a
  specific tracing vendor (parallels CMOS-00-ENG-013).
- **CMOS-15-OBS-031** A trace MUST cover a Call end to end: API ingress → routing
  decision → control→media command → media setup → media→control fact → teardown, with
  spans annotated by `correlation_id` and the relevant entity ids so the trace and the
  event stream describe the same Call.
- **CMOS-15-OBS-032** Sampling policy MUST be configurable; error and anomalous Calls
  SHOULD be retained preferentially (tail-based or equivalent) so failures are always
  traceable even under aggressive sampling.

## 6. Health & readiness (normative)

- **CMOS-15-OBS-040** Every node MUST expose distinct **liveness** and **readiness**
  endpoints (reference: `/healthz`, `/readyz`). Liveness reflects "the process is
  alive"; readiness reflects "this node can correctly serve" — including reachability
  of its hard dependency PostgreSQL and, in multi-node topologies, the distributed-state
  layer (CMOS-14-DEP-020/021).
- **CMOS-15-OBS-041** Readiness MUST gate load-balancer membership and graceful drain
  (CMOS-14-DEP-033/051): a node MUST report **not ready** before it is able to serve
  and while draining, so rolling upgrades drop no Calls (CMOS-14-DEP-050).
- **CMOS-15-OBS-042** Health endpoints MUST NOT leak tenant data or secrets and MUST be
  safe to expose to infrastructure probes independent of tenant authorization.

## 7. Media-quality telemetry (normative)

Media quality is a first-class signal, not a debugging afterthought — it is how "voice
is just one workload" is held to a measurable bar.

- **CMOS-15-OBS-050** For every **Media Stream** in a Call, the platform MUST collect
  quality telemetry — **MOS**, **jitter**, **packet loss**, and **latency (RTT/one-way
  where available)** — corresponding to the `MediaStream.stats` object (Volume 2) and
  the media-quality facts the RTP/SRTP subsystem produces (Volume 3 components;
  Volume 7).
- **CMOS-15-OBS-051** Media-quality facts MUST flow **media→control as Events**
  (`Media/*` quality facts, CMOS-03-ARCH-003) and MUST NOT be computed on, or block,
  the RTP hot path (CMOS-03-ARCH-021). Aggregate quality MUST also be exposed as
  metrics (§3) for fleet-wide SLOs.
- **CMOS-15-OBS-052** Per-Call and per-Stream quality MUST be **attributable** to the
  Call's three identities — Device, User (via Identity), Organisation
  (CMOS-00-ENG-011) — and correlatable via `correlation_id` (§2), so "which users had
  bad calls on which devices" is answerable without protocol-level digging.
- **CMOS-15-OBS-053** Quality telemetry retention and aggregation feed the Reports
  surface (Volume 13) and the SLOs of §9/Volume 17; the numeric quality thresholds
  (e.g. minimum acceptable MOS) are defined in Volume 17, not here.

## 8. Diagnostics: bundles, SIP traces & PCAP (normative; expert mode)

- **CMOS-15-OBS-060** The platform MUST support producing a **diagnostic/debug bundle**
  — a point-in-time collection of configuration snapshot, recent logs, health, metric
  snapshots, and relevant traces — for support and incident analysis. A bundle MUST be
  stored as an **Object** of kind `DIAGNOSTIC` (Volume 2 Object) via the Object Storage
  abstraction and fetched under authorization via presigned URL (CMOS-03-ARCH-040/041).
- **CMOS-15-OBS-061** **SIP traces** and **PCAP capture** are **Expert Mode** surfaces
  (N-5, CMOS-13-UI-011). They MUST be off by default, enabled only on explicit opt-in,
  and MUST be **capability-gated** (CMOS-13-UI-013 / CMOS-05-EVT-040); enabling capture
  MUST NOT alter call behaviour.
- **CMOS-15-OBS-062** Captured traces/PCAP MUST be stored as Objects (kind
  `DIAGNOSTIC`) under tenant scope and authorization, MUST be subject to retention
  limits, and MUST NOT be exposed to non-expert, unauthorized operators — captures can
  contain sensitive signalling and media metadata (Volume 9).
- **CMOS-15-OBS-063** Diagnostic captures MUST carry the `correlation_id` (and trace
  context where applicable) of the operation they concern, so a PCAP is pivotable to the
  same Call's logs, trace, and events (§2/§3).

## 9. Golden signals & SLOs (normative)

- **CMOS-15-OBS-070** The platform MUST expose the metrics necessary to compute the
  **golden signals** (latency, traffic, errors, saturation) and **service-level
  indicators** for control-plane requests, call setup, and media quality
  (CMOS-15-OBS-012/050).
- **CMOS-15-OBS-071** SLOs are **defined against Volume 17 targets**; this volume
  guarantees the *observability* of the underlying SLIs, and Volume 17 sets the numeric
  objectives. The two MUST stay consistent — an SLI that Volume 17 sets an objective on
  MUST be exposed here.
- **CMOS-15-OBS-072** Alerting SHOULD be driven by SLO burn on golden signals rather
  than on individual raw metrics, keeping operational surface small (CMOS-00-ENG-001).

## 10. Vendor neutrality (normative)

- **CMOS-15-OBS-080** All observability surfaces MUST use **open, standard formats** —
  Prometheus/OpenMetrics, W3C Trace Context, OTLP, structured JSON logs — so any
  conforming backend can consume them. The platform MUST NOT depend on a specific
  observability vendor (parallels CMOS-00-ENG-007/013).

## Conformance notes

- **L1 (Contract):** `/metrics` exposition is valid OpenMetrics with documented
  names/types/units and bounded label cardinality; logs are structured with the
  required fields; `/healthz` and `/readyz` behave per §6; emitted traces are valid W3C
  Trace Context and consistent with the event `traceparent` (CMOS-15-OBS-002/010/020/040).
- **L2 (Behavioural):** For a driven Call, one `correlation_id` retrieves the correlated
  logs, a single connected end-to-end trace, and the ordered `Call/*` and `Media/*`
  events; per-Stream MOS/jitter/loss/latency are captured off the hot path and are
  attributable to the three identities; a diagnostic bundle and (in expert mode) a
  capability-gated PCAP are produced as `DIAGNOSTIC` Objects
  (CMOS-15-OBS-001/003/031/050/052/060/061).
- **Non-interference:** raising log verbosity or enabling traces/PCAP demonstrably does
  not change platform behaviour or drop Calls (CMOS-15-OBS-023/061).

## Open items

- A machine-readable **metric & SLI catalogue** under `contracts/` (names, types,
  units, labels) so the harness can assert exposition ↔ catalogue consistency —
  deferred; prose-only at 0.3.0.
- Alignment of the **`Media/*` quality-fact event schemas** with `MediaStream.stats`
  (joint with Volumes 2/5/7).
- Standard **diagnostic-bundle manifest** shape and default retention windows for
  `DIAGNOSTIC` Objects.
- Numeric SLO/quality thresholds — owned by Volume 17, referenced here.

## Change log

- **0.3.0** — Initial implementation-grade draft: end-to-end correlation via
  `correlation_id` and W3C `traceparent` across events/logs/traces; Prometheus/
  OpenMetrics exposition with bounded cardinality and golden signals; structured logs;
  distributed tracing over OTLP; liveness/readiness gating drain and rolling upgrades;
  per-MediaStream MOS/jitter/loss/latency telemetry flowing media→control off the hot
  path and attributable to the three identities; diagnostic bundles and expert-mode
  SIP trace/PCAP capture as capability-gated `DIAGNOSTIC` Objects; and SLIs feeding
  Volume 17 SLOs — all assigned stable requirement IDs.
