# Volume 3 — System Architecture

**Status:** REVIEW · **Version:** 0.3.0 · **Subsystem tag:** ARCH

This volume defines the subsystems, their boundaries, and how they interact. It is
the engineering bible: it constrains *structure*, not implementation technology
beyond the abstractions the constitution requires. Component inventory:
[`components.md`](components.md).

The reference technology stack (Rust/Tokio/Axum, PostgreSQL, SQLx, S3-compatible
object storage, Wasmtime plugins, Vue 3 UI) is **recommended**, not mandated;
conformance is defined against contracts, not languages. Rationale is recorded in
Volume 19 (ADRs).

---

## 1. The two planes (normative)

- **CMOS-03-ARCH-001** The system is divided into a **Control Plane** (decides *what*
  happens) and a **Media Plane** (moves real-time media). They communicate only over
  **typed interfaces**, never shared mutable memory, even when compiled into one
  binary (CMOS-00-ENG-006).
- **CMOS-03-ARCH-002** The control/media boundary MUST be a stable interface such
  that the media plane can be split into separate processes or nodes without changing
  control-plane logic. Media nodes are horizontally scalable and addressable.
- **CMOS-03-ARCH-003** Media never traverses the control plane. Signalling decisions
  flow control→media as commands; media-quality/state facts flow media→control as
  events (Volume 5).

```
                 ┌──────────────────────── Control Plane ───────────────────────┐
   clients ────▶ │ API Gateway ─ Identity ─ Policy ─ Routing ─ Provisioning ─    │
  (UI/CLI/API)   │ Billing ─ Automation ─ Presence ─ Cluster Mgr ─ Event Bus     │
                 └───────▲───────────────────────────────┬──────────────────────┘
                         │ events (facts)                 │ typed commands
                 ┌───────┴───────────────────────────────▼──────────────────────┐
   endpoints ──▶ │ SIP ─ RTP/SRTP ─ Transcoding ─ Recording ─ Conferencing ─     │  Media Plane
  (phones/WebRTC)│ WebRTC/ICE/TURN                                               │
                 └───────────────────────────────────────────────────────────────┘
        shared state:  PostgreSQL  ·  Object Storage (S3-compatible)  ·  Redis/NATS
```

## 2. Statelessness & shared state (normative)

- **CMOS-03-ARCH-010** Control-plane services are **stateless** between requests. All
  durable state lives in PostgreSQL; all large artifacts in Object Storage; all
  distributed ephemeral state (registrations, presence, locks, cursors) in the
  Redis/NATS-class layer (CMOS-00-ENG-015).
- **CMOS-03-ARCH-011** Any control-plane node can serve any request for any tenant it
  is authorised for; there is no node affinity for correctness (only for locality
  optimisation).
- **CMOS-03-ARCH-012** Media sessions MAY be node-affine for the life of a Call;
  affinity is an optimisation, and failover MUST NOT require the control plane to
  hold media state.

## 3. Concurrency model (normative)

- **CMOS-03-ARCH-020** Real-time entities (Call, Conference, Registration, Queue,
  Gateway) are modelled as **actors**: each owns its state and communicates by
  message passing. No global mutex guards call state (CMOS-00-ENG-006 rationale).
- **CMOS-03-ARCH-021** Blocking or long-running work (recording write, transcode,
  firmware transfer, AI dispatch) runs on **background workers**; it MUST NOT block
  an RTP/media path (Volume 17 latency targets).

## 4. Event Bus & outbox (normative)

- **CMOS-03-ARCH-030** Every state change that must be observable is written to a
  **transactional outbox** in the same transaction as the state change, then relayed
  to the Event Bus (guarantees CMOS-05-EVT-010). The bus binding (NATS JetStream,
  Redis Streams, Kafka) is pluggable behind one interface.
- **CMOS-03-ARCH-031** Subscriptions carry durable cursors and dead-letter streams
  (CMOS-05-EVT-013). The bus interface is the same in single-binary and clustered
  deployments; only the binding changes.

## 5. Object Storage abstraction (normative)

- **CMOS-03-ARCH-040** All large artifacts are accessed through one **Object Storage
  interface** with backends Local / S3 / MinIO / R2 / Backblaze / Azure Blob / GCS.
  No subsystem depends on a specific backend (CMOS-00-ENG-007).
- **CMOS-03-ARCH-041** Object access uses short-lived presigned URLs where the
  backend supports it; the platform mediates authorization before issuing them.

## 6. Subsystem responsibilities (summary)

Full inventory in [`components.md`](components.md).

| Subsystem | Plane | Responsibility |
|-----------|-------|----------------|
| API Gateway | Control | AuthN/Z, rate limits, request→command, OpenAPI surface |
| Identity | Control | Users, Identities, authentication methods, sessions |
| Policy Engine | Control | Evaluate allow/deny/require-identity/approval |
| Routing | Control | Resolve Route/CallFlow/IVR/Queue to a destination |
| Provisioning | Control | Device lifecycle, zero-touch, signed config delivery |
| Billing | Control | CDR assembly, rating, cost allocation |
| Automation | Control | Event-triggered declarative actions |
| Presence | Control | Registration/presence fan-out over WebSocket |
| Cluster Manager | Control | Node membership, media-node placement, failover |
| Event Bus | Control | Outbox relay, subscriptions, dead-letter |
| SIP | Media | Signalling: registration, INVITE/BYE, transfer |
| RTP/SRTP | Media | Media transport, jitter buffer, DTMF |
| Transcoding | Media | Codec negotiation & conversion |
| Recording | Media→worker | Capture to Object Storage off the hot path |
| Conferencing | Media | SFU-like mixing/forwarding |
| WebRTC | Media | ICE/STUN/TURN, browser endpoints |
| Plugin Runtime | Control | Sandboxed WASM extensions (Volume 12) |

## 7. Trust & isolation (normative)

- **CMOS-03-ARCH-050** Tenant isolation is enforced at the data layer (row-level
  scoping by `tenant_id`) and re-checked at the API/Policy layer; a single missed
  check MUST NOT leak cross-tenant data (defence in depth; CMOS-00-ENG-008).
- **CMOS-03-ARCH-051** Plugins run in a WASM sandbox with declared capabilities and
  resource limits; a plugin fault MUST NOT crash the host (CMOS-00-ENG-013 / V12).

## 8. Deployment topologies (informative → normative floor)

- **Single binary** (default): all subsystems in one process + PostgreSQL (+ optional
  Redis). Target cold start < 2s (Volume 17).
- **Split media**: control-plane nodes + dedicated media nodes, shared PostgreSQL /
  Object Storage / Redis-NATS. Enabled by CMOS-03-ARCH-002 with no code redesign.
- **CMOS-03-ARCH-060** Whichever topology, the observable contracts (API, events)
  MUST be identical; deployment shape is invisible to clients.

## 9. Conformance notes
- Architecture conformance is verified indirectly: an implementation is `core`-L2
  conformant if its API and events behave per Volumes 2/4/5 regardless of internal
  structure. Volume 16 defines chaos/failover tests that assert the statelessness and
  outbox guarantees above.

## 10. Open items
- Formal typed control↔media interface (IDL) — candidate for `contracts/` in v0.4.
- Cluster membership & media placement protocol detail.

## Change log
- **0.3.0** — Two-plane model, statelessness, actor concurrency, outbox, storage
  abstraction, subsystem responsibilities, and topologies specified.
