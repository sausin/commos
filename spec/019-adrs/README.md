# Volume 19 — Architectural Decision Records

**Status:** REVIEW · **Version:** 0.4.0 · **Subsystem tag:** (governance)

Every significant decision is recorded here so future contributors do not unknowingly
re-open settled questions — and so that when a decision *should* be reopened, the
original context is available. Each ADR states the **problem**, **alternatives**,
**decision**, **consequences**, and **what would reopen it**.

ADRs are immutable once `Accepted`. A change of mind is a *new* ADR that supersedes
an old one (the old one is marked `Superseded by ADR-NNNN`, never deleted —
CMOS-00-ENG-012). Status ∈ `{Proposed, Accepted, Superseded, Rejected}`.

| ADR | Title | Status |
|-----|-------|--------|
| 0001 | Specification-first, with executable conformance as the contract | Accepted |
| 0002 | Identity-first domain model (four distinct principals) | Accepted |
| 0003 | Rust + Tokio as the reference implementation language | Accepted |
| 0004 | PostgreSQL as the system of record | Accepted |
| 0005 | Pluggable event bus behind one interface (not Kafka-mandated) | Accepted |
| 0006 | Capability-based authorization, not RBAC | Accepted |
| 0007 | WebAssembly (Wasmtime-class) for plugins | Accepted |
| 0008 | Object Storage abstraction, not a filesystem API | Accepted |
| 0009 | Repository licence: O'Saasy (source-available) | Accepted |
| 0010 | Control-plane / media-plane separation behind typed interfaces | Accepted |
| 0011 | CloudEvents-style envelope with at-least-once + idempotency | Accepted |

---

## ADR-0001 — Specification-first, with executable conformance as the contract
**Status:** Accepted
**Problem.** A PBX is a distributed real-time OS; a PRD alone cannot coordinate
parallel (human or AI) implementation of compatible components.
**Alternatives.** (a) PRD + reference implementation as de-facto spec; (b) prose
specs only; (c) specs + machine-readable contracts + executable conformance.
**Decision.** (c). The durable asset is the versioned specification plus
`contracts/` (JSON Schema, OpenAPI) plus `conformance/` (an executable harness).
Prose gives meaning; contracts give shape; the harness is the arbiter.
**Consequences.** Higher up-front cost; enables independent implementations and a
vendor-neutral standard (OCI/Kubernetes analogy). A volume is `FROZEN` only with a
contract and passing conformance (CONVENTIONS §5).
**Reopen if.** The overhead measurably outweighs the coordination benefit for the
actual contributor set.

## ADR-0002 — Identity-first domain model (four distinct principals)
**Status:** Accepted
**Problem.** Legacy PBXs collapse User, Identity, Device, and Extension into "an
extension", breaking billing, security, and mobility.
**Alternatives.** (a) Extension-centric (legacy); (b) User+Device only; (c) four
distinct principals with Extension as a mere label.
**Decision.** (c) — see CMOS-02-DOM-004 and the Glossary. A Device is owned by the
Organisation and can carry different Identities across calls.
**Consequences.** Correct per-User attribution on shared devices; more entities to
model; every downstream volume must respect the separation.
**Reopen if.** A concrete workload proves the distinction unnecessary (none known).

## ADR-0003 — Rust + Tokio as the reference implementation language
**Status:** Accepted
**Problem.** Media planes need predictable latency (no GC pauses), memory safety, and
strong async networking; the control plane needs the same toolchain for one binary.
**Alternatives.** Go (great deployment/hiring, but GC and weaker media performance);
C++ (fast, memory-unsafe); Java/.NET (heavy runtime); Node/Python (poor for media).
**Decision.** Rust/Tokio/Axum/SQLx as the **recommended reference** stack. It is not
*mandated*: conformance is defined against contracts, not languages (CONVENTIONS §3).
**Consequences.** Higher barrier to contribution; excellent latency/safety; Go
remains an acceptable control-plane alternative for an implementer who accepts the
media trade-off.
**Reopen if.** Rust's ecosystem gaps (e.g. a specific media/codec need) prove
disqualifying, or an implementer targets a control-plane-only profile.

## ADR-0004 — PostgreSQL as the system of record
**Status:** Accepted
**Problem.** Need one dependable store for structured state with strong consistency,
rich indexing, partitioning, and ubiquitous operational familiarity.
**Alternatives.** FoundationDB (powerful, operationally niche); a NewSQL cluster
(heavier); multiple specialised stores (operational sprawl — violates
CMOS-00-ENG-001).
**Decision.** PostgreSQL is the reference system of record; ephemeral distributed
state uses a Redis/NATS-class layer; large artifacts use Object Storage.
**Consequences.** One well-understood dependency; horizontal write scale needs
partitioning/replication strategy (Volume 6), not a different engine by default.
**Reopen if.** A tenant scale is reached where Postgres partitioning/replication no
longer meets Volume 17 targets.

## ADR-0005 — Pluggable event bus behind one interface (not Kafka-mandated)
**Status:** Accepted
**Problem.** The event bus is the core integration surface; mandating one broker
would burden small deployments and lock out large ones.
**Alternatives.** Mandate Kafka (heavy for SMB); mandate Redis Streams (limited at
scale); define one interface with multiple bindings.
**Decision.** One bus interface with a transactional outbox (CMOS-03-ARCH-030) and
pluggable bindings (in-process/Redis Streams/NATS JetStream/Kafka). Guarantees
(at-least-once, ordering by `sequence`, dead-letter) are specified, not the broker.
**Consequences.** SMB runs with no external broker; enterprise swaps the binding with
no code change.
**Reopen if.** A guarantee proves unimplementable across the target bindings.

## ADR-0006 — Capability-based authorization, not RBAC
**Status:** Accepted
**Problem.** Roles conflate unrelated permissions and drift; fine-grained control is
needed for MSP/enterprise and for plugins.
**Alternatives.** RBAC (familiar, coarse); ABAC (flexible, complex); capabilities
(fine-grained grants) with roles as UI-only bundles.
**Decision.** Capabilities are the authorization primitive (CMOS-00-ENG-009); roles,
if shown, are named capability bundles in the UI only. Plugins receive scoped
capability grants.
**Consequences.** Precise least-privilege; the UI must present capabilities
approachably (Volume 13) to preserve operability (CMOS-00-ENG-001).
**Reopen if.** Capability management proves too fine-grained to operate at SMB scale
without the role abstraction being effectively mandatory.

## ADR-0007 — WebAssembly (Wasmtime-class) for plugins
**Status:** Accepted
**Problem.** Third parties must extend the platform (provisioners, CRM, billing,
auth) without the power to crash or compromise it.
**Alternatives.** Native dynamic libs (unsafe, ABI-fragile); subprocess+RPC (heavier,
still OS-trusted); WASM sandbox with declared capabilities and resource limits.
**Decision.** WASM (Wasmtime-class). A plugin fault MUST NOT crash the host
(CMOS-03-ARCH-051); plugins get scoped capabilities and cpu/mem/time limits
(Volume 12).
**Consequences.** Strong isolation and portability; some host APIs must be exposed via
a stable ABI; certain low-level media tasks stay in-core.
**Reopen if.** WASM performance/ABI limits block a critical extension class.

## ADR-0008 — Object Storage abstraction, not a filesystem API
**Status:** Accepted
**Problem.** Recordings, voicemail, firmware, transcripts, exports must live
somewhere that scales and is portable across deployments and clouds.
**Alternatives.** Local filesystem API (doesn't scale/replicate cleanly); hard-code
S3 (cloud lock-in); one Object Storage interface over many backends.
**Decision.** All large artifacts are Objects behind one interface
(Local/S3/MinIO/R2/Backblaze/Azure/GCS) — CMOS-03-ARCH-040. Payloads carry Object
references, never blobs (CMOS-02-DOM-013).
**Consequences.** Cloud-native without cloud dependence (N-3); a thin local backend
is required for the single-binary default.
**Reopen if.** A backend-specific capability becomes essential and cannot be
abstracted.

## ADR-0009 — Repository licence: O'Saasy (source-available)
**Status:** Accepted
**Problem.** The project is both an open specification (meant to invite independent,
competing implementations) and a working reference implementation the maintainers may run
as a service. The licence must let anyone self-host and modify the code, while reserving the
right to run it as a commercial SaaS for the maintainers.
**Alternatives.** Permissive (Apache-2.0 / MIT); copyleft (AGPL); other source-available
licences (BSL, SSPL, Elastic); the [O'Saasy License](https://osaasy.dev/) (MIT grant + a
clause reserving competing-SaaS rights to the Licensor).
**Decision.** The repository is licensed under the **O'Saasy License** (see `/LICENSE`),
Copyright © 2026, Saurabh Singhvi. Self-host / use / modify / redistribute is permitted;
repackaging it as a competing hosted/SaaS product is not.
**Consequences.** Preserves self-hosting and community contribution while protecting the
maintainers' SaaS. Note the trade-off: O'Saasy is *source-available*, not OSI-approved open
source, so the specification text is not, by this licence alone, a fully vendor-neutral
standard; a future ADR may dual-licence the **spec + contracts** under a permissive/CC licence
if standardisation is pursued, keeping the **reference code** under O'Saasy.
**Reopen if.** Standardisation requires an OSI-open spec licence, or governance adds a
trademark policy for a "CommOS conformant" mark.

## ADR-0010 — Control-plane / media-plane separation behind typed interfaces
**Status:** Accepted
**Problem.** A monolith couples call-control logic to media handling and blocks
independent scaling of media.
**Alternatives.** Monolith with shared memory; always-separate services (operational
cost for SMB); one binary with an internal typed control↔media interface.
**Decision.** Separate the planes behind a typed interface even in one binary
(CMOS-00-ENG-006, CMOS-03-ARCH-001/002); allow splitting into dedicated media nodes
with no code redesign.
**Consequences.** SMB gets one process; enterprise scales media independently; the
interface must be kept genuinely typed and side-effect-free across the boundary.
**Reopen if.** The abstraction cost outweighs the benefit for all realistic scales.

## ADR-0011 — CloudEvents-style envelope with at-least-once + idempotency
**Status:** Accepted
**Problem.** Integrations need a stable, tool-friendly event contract with clear
delivery and ordering semantics.
**Alternatives.** Bespoke envelope; exactly-once transport (costly, often illusory);
CloudEvents-style envelope + at-least-once + consumer idempotency + per-correlation
ordering.
**Decision.** Adopt the envelope in `contracts/json-schema/envelope.schema.json`
(Volume 5 §2) with at-least-once delivery, idempotency keys, and `sequence` ordering
per `correlation_id` (CMOS-05-EVT-010..022).
**Consequences.** Broad tool compatibility; consumers must be idempotent (documented
and tested in Volume 16).
**Reopen if.** A transport offering practical exactly-once with acceptable cost
becomes the norm across target bindings.

## ADR-0012 — Embedded SQLite as the default system of record; PostgreSQL for scale
**Status:** Accepted
**Problem.** The default single-binary deployment (a small business, an edge box, a
Raspberry Pi) wants durable storage *without* operating a separate database server, and
on SD-card hosts must minimise write amplification to survive for years. The v0.4 spine
named PostgreSQL the sole system of record (CMOS-14-DEP-020), which forces an external
service even on a one-box install.
**Alternatives.** In-memory only (not durable); require PostgreSQL always (a service to
run, patch, and back up — heavy for a single box); embedded SQLite by default with
PostgreSQL as the scale/HA option.
**Decision.** The system of record is a SQL store behind one interface (the reference
`Store` trait). The **default** binding is **embedded SQLite** (WAL journal, `synchronous
= NORMAL`) — durable with zero external dependency, fulfilling CMOS-14-DEP-021 better
than an in-memory equivalent. **PostgreSQL** remains the binding for any **multi-node /
HA** topology (CMOS-14-DEP-011/030), where stateless control-plane nodes share one
database — which SQLite's single-file model cannot serve. Ephemeral high-churn state
(registrations, presence) stays in memory, never the durable store, to keep writes low.
**Consequences.** The single binary is durable *and* dependency-free out of the box;
SD-card hosts see minimal writes. A single node cannot share its SQLite file across
processes/nodes (by design — use PostgreSQL there). Amends CMOS-14-DEP-020: PostgreSQL is
the *scale* system of record, not the only one; the hard-dependency floor for the default
deployment becomes *none*, strengthening N-3.
**Reopen if.** A single embedded engine gains safe multi-writer / multi-node semantics,
or an operational reason makes PostgreSQL-by-default worthwhile again.

## Change log
- **0.4.x** — Added ADR-0012 (embedded SQLite default, PostgreSQL for scale), recording
  the reference implementation's storage tiering.
- **0.3.0** — Eleven ADRs recording the decisions embodied in the v0.3 spine and
  contracts; ADR-0009 (licence) left `Proposed` pending maintainer ratification.
