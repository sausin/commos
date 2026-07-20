# Volume 0 — Philosophy & Design Principles

**Status:** REVIEW · **Version:** 0.3.0 · **Subsystem tag:** (constitution)

This volume is the **constitution**. It does not describe features; it states the
invariants and non-goals that every other volume, contract, and implementation
MUST obey. When a design decision is contested, it is resolved by appeal to this
volume. Amending an invariant here requires an ADR (Volume 19) and, in most cases,
a MAJOR version bump.

---

## 1. Vision

CommOS is a **communications operating system**: a substrate of shared services on
which communication *workloads* run. Voice/telephony is the first workload and the
one that proves the model, but it is not privileged in the architecture. Messaging,
video, intercom, contact centre, AI agents, and IoT endpoints are peer workloads on
the same substrate.

The strategic asset is the **specification and its contracts**, not any single
implementation. Success looks like multiple interoperable implementations — the way
many container runtimes implement OCI — validated by a shared conformance suite.

## 2. The prime directive

> **CMOS-00-ENG-001 (invariant).** Every feature MUST make the system *simpler to
> operate*, not merely more powerful. A feature that increases operational surface
> area without a commensurate reduction elsewhere MUST be rejected or redesigned.

This is the tie-breaker for all other principles. Legacy PBXs failed operators by
accreting power without bounding complexity; CommOS treats operability as a
first-class, non-negotiable constraint.

## 3. Core invariants

These are `MUST`-level and cross-cutting. Volumes refine them; none may contradict them.

- **CMOS-00-ENG-002 — Identity-first, not extension-first.** The subjects of the
  system are People, Devices, Numbers, and Organisations. An Extension is a mere
  dialable label. Billing, security, and mobility are defined against Identity,
  User, Device, and Organisation — never against an Extension.
- **CMOS-00-ENG-003 — API-first.** Every capability of the system is available
  through the public API. The web UI, CLI, mobile app, and Terraform provider are
  all clients of the *same* API. There is no privileged back door. If the UI can do
  it, the API can do it, and vice versa.
- **CMOS-00-ENG-004 — Event-first.** Every state change of consequence emits a
  canonical Event (Volume 5). Integrations (AI, CRM, billing, automation) subscribe
  to events; the platform never embeds knowledge of a specific consumer.
- **CMOS-00-ENG-005 — Declarative configuration.** Administrators describe *intent*
  (people, phones, call flows, hours, numbers). The platform reconciles reality to
  intent. There are no XML dialplans, Lua scripts, SIP profiles, or ACL files
  exposed to users; those, if they exist at all, are internal implementation detail.
- **CMOS-00-ENG-006 — Control/media plane separation.** The system that decides
  *what happens* (control plane) is architecturally distinct from the system that
  *moves media* (media plane), communicating over typed interfaces — even when
  shipped in one binary. This preserves the option to scale media independently.
- **CMOS-00-ENG-007 — Storage abstraction.** Large artifacts are Objects behind an
  Object Storage interface. The platform depends on *an* object store, never on a
  specific vendor. Structured state lives in PostgreSQL; distributed ephemeral state
  in a Redis/NATS-class abstraction.
- **CMOS-00-ENG-008 — Multi-tenancy is not an afterthought.** One deployment serves
  thousands of Organisations with true isolation. Every entity and event is
  tenant-scoped. Cross-tenant access is impossible by construction, not by policy.
- **CMOS-00-ENG-009 — Capabilities, not roles.** Authorization is expressed as
  fine-grained, grantable Capabilities. Roles, if offered, are named bundles of
  capabilities in the UI only.
- **CMOS-00-ENG-010 — Zero-trust provisioning & operation.** Devices onboard through
  short-lived signed URLs, mutual authentication, one-time tokens, explicit
  approval, and revocation. Trust is never implied by network location.
- **CMOS-00-ENG-011 — Everything attributable.** Every Call carries three
  identities — Device, User (via Identity), Organisation — so every CDR is
  attributable for billing and audit.
- **CMOS-00-ENG-012 — Immutable history.** Configuration and security-relevant
  actions are append-only and auditable. Nothing of record is ever hard-deleted;
  deletion is a state transition (Time Machine / rollback is possible).
- **CMOS-00-ENG-013 — AI is an external consumer.** The platform does not implement
  AI. It exposes the events, streams, objects, and APIs that let any AI system
  (Claude, OpenAI, Gemini, local models, n8n, LangGraph) plug in. No AI-vendor
  assumptions leak into the substrate.
- **CMOS-00-ENG-014 — Single artifact by default.** The reference deployment is one
  native binary plus PostgreSQL (and optionally Redis), run under systemd; a
  host-network Docker image is officially supported but secondary. Complexity is
  opt-in as scale demands.
- **CMOS-00-ENG-015 — Horizontal scalability without redesign.** Components are
  stateless except for PostgreSQL, the object store, and the distributed-state
  layer. Adding nodes is the scaling path; no fundamental redesign is required to go
  from a 10-person business to a 100,000-user enterprise.
- **CMOS-00-ENG-016 — SIP is one transport.** SIP/RTP is an implementation of the
  Communications workload, not the model. The domain model is protocol-neutral;
  WebRTC and future protocols are peers.

## 4. Non-goals

Stated explicitly so they are not silently reintroduced:

- **N-1** CommOS is **not** a FreeSWITCH/Asterisk configuration front-end. It does
  not expose their primitives.
- **N-2** CommOS does **not** ship its own LLM, ASR, or TTS. (See CMOS-00-ENG-013.)
- **N-3** The default deployment does **not** require Kubernetes, a message-broker
  cluster, or a cloud account. Cloud-native, without cloud *dependence*.
- **N-4** CommOS does **not** guarantee bug-for-bug compatibility with any legacy
  dialplan behaviour.
- **N-5** Users are **not** exposed to SIP headers, SDP, or codec negotiation in the
  default experience; an explicit **expert mode** may surface them.
- **N-6** The specification does **not** mandate a specific SIP stack, media codec
  implementation, or database vendor beyond the stated abstractions — only their
  contracts.

## 5. Progressive complexity

The system MUST present a "zero-training" surface to a small business — People,
Phones, Departments, Call Flows, Business Hours, Queues, Numbers, Devices, Reports —
while allowing an **expert mode** to expose SIP traces, SDP, NAT behaviour, and
protocol internals on demand. Complexity is revealed, never required.

## 6. Design tenets (informative)

- Prefer *reconciliation* (declare desired state, converge) over imperative mutation.
- Prefer *typed contracts* over stringly-typed configuration.
- Prefer *one clear way* to do a thing over many configurable ways.
- Make the common case automatic and the rare case possible.
- Every error message names the intent that failed and the next action.
- If a feature needs a manual to be safe to operate, redesign it.

## 7. How to use this volume

Every other volume's normative requirements MUST be traceable to one or more
invariants here. A requirement that serves no invariant, or violates one, is a bug
in the specification. ADRs (Volume 19) record any tension between invariants and how
it was resolved.

## Change log
- **0.3.0** — Rewritten from skeleton to constitutional form; invariants assigned
  stable IDs.
