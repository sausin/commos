# Volume 14 — Deployment & Operations

**Status:** REVIEW · **Version:** 0.4.0 · **Subsystem tag:** DEP

Deployment is where the operability mandate (CMOS-00-ENG-001) meets the ground. This
volume specifies how CommOS is packaged, run, scaled, upgraded, backed up, and
recovered — and it holds the line that the constitution draws: **one native binary
plus PostgreSQL is the default; complexity is opt-in as scale demands**
(CMOS-00-ENG-014). The same artifact that a ten-person business runs under systemd
scales, without redesign, to a hundred-thousand-user enterprise across many nodes
(CMOS-00-ENG-015). The observable contracts — API (Volume 4) and events (Volume 5) —
are **identical across every topology** (CMOS-03-ARCH-060); deployment shape is
invisible to clients.

---

## 1. Scope

In scope: packaging and artifacts, the three supported deployment topologies and their
support tiers, hard vs. optional dependencies, high availability, horizontal scaling,
CPU architectures, backup/restore, rolling upgrades, disaster recovery, and
config-as-code (`pbx.yaml`). Out of scope: the numeric performance/capacity targets
(Volume 17), the security controls and **secret-management mechanics** (Volume 9 —
referenced here only where deployment touches secrets), and observability signals
(Volume 15).

## 2. Packaging & artifacts (normative)

- **CMOS-14-DEP-001** The **primary** deliverable MUST be a **single self-contained
  native binary** that runs the full control plane and media plane in one process and
  requires no runtime interpreter, sidecar, or orchestration layer
  (CMOS-00-ENG-014). Its only hard runtime dependency is PostgreSQL (§4).
- **CMOS-14-DEP-002** The binary MUST be operable under **systemd** as a standard
  service unit, with clean start/stop/reload, a defined exit-code contract, and log
  output that integrates with the platform's structured logging (Volume 15). A
  reference unit file and default config path MUST be documented.
- **CMOS-14-DEP-003** A **host-network container image** MUST be published as an
  **officially supported secondary** artifact (CMOS-00-ENG-014). It MUST run with host
  networking (RTP/SIP port ranges are not amenable to per-port NAT mapping) and MUST be
  behaviourally identical to the native binary. It is secondary, not primary.
- **CMOS-14-DEP-004** All artifacts MUST be published for **both `amd64` and `arm64`**
  (§8). Artifacts MUST be reproducible and **signed**, with published checksums, so an
  operator can verify provenance before install (ties to Volume 9 supply-chain).
- **CMOS-14-DEP-005** The default deployment MUST NOT require Kubernetes, a
  message-broker cluster, or any cloud account (N-3). Cloud-native capability MUST NOT
  become cloud *dependence*.

## 3. Deployment topologies (normative)

Whichever topology is chosen, the two-plane split (CMOS-03-ARCH-001) and control-plane
statelessness (CMOS-03-ARCH-010) are preserved; only process/node boundaries move.

- **CMOS-14-DEP-010 — Single binary (default / primary tier).** All subsystems run in
  one process alongside PostgreSQL (and optionally Redis). This MUST be a first-class,
  production-supported topology, not a demo mode. It is the reference target for cold
  start and footprint budgets (Volume 17; Volume 3 §8).
- **CMOS-14-DEP-011 — Split media (HA / scale tier).** Stateless control-plane nodes
  and dedicated, horizontally scalable **media nodes** share PostgreSQL, Object
  Storage, and the Redis/NATS-class layer. This topology MUST be reachable **without
  code redesign** — it is enabled by the stable control↔media interface
  (CMOS-03-ARCH-002/003). Media nodes MAY be Call-affine for a Call's lifetime; the
  control plane MUST hold no media state (CMOS-03-ARCH-012).
- **CMOS-14-DEP-012 — Kubernetes (large scale).** Kubernetes is a **supported** target
  for large deployments but MUST remain **optional** (N-3). Control-plane pods are
  stateless and horizontally scalable; media requires host-network / `hostPort` or an
  equivalent so RTP is directly reachable. Manifests/Helm MUST NOT introduce behaviour
  divergent from the native binary.
- **CMOS-14-DEP-013** The API and event contracts MUST be **identical in observable
  behaviour** across all three topologies (CMOS-03-ARCH-060). A client MUST NOT be able
  to detect the topology from the contract surface.

## 4. Dependencies (normative)

- **CMOS-14-DEP-020** The **only hard dependency** is **PostgreSQL**, the system of
  record for structured entities (CMOS-00-ENG-007; Volume 6). A single-binary
  deployment MUST be able to run against a single PostgreSQL instance.
- **CMOS-14-DEP-021** **Redis/NATS-class distributed state** (registrations, presence,
  locks, cursors, bus binding) is **OPTIONAL for a single node** and becomes
  **REQUIRED for any multi-node topology** (CMOS-03-ARCH-010/030). The single binary
  MUST provide an embedded/in-process equivalent so it needs no external broker.
- **CMOS-14-DEP-022** **Object Storage** is accessed only through the Object Storage
  abstraction with a local-filesystem backend as the zero-dependency default and
  S3/MinIO/R2/Azure/GCS as scale backends (CMOS-03-ARCH-040). No subsystem may hard-code
  a specific backend.
- **CMOS-14-DEP-023** The platform MUST NOT require any external identity provider,
  message broker cluster, or cloud managed service to boot; each is an optional
  scale/integration edge (Volume 3 components; N-3).

## 5. High availability (normative)

- **CMOS-14-DEP-030** In an HA topology, the control plane MUST be run as **two or more
  stateless nodes** behind a load balancer; loss of any single control-plane node MUST
  NOT lose committed state or in-progress Calls, because all durable state is in
  PostgreSQL / Object Storage / the distributed-state layer (CMOS-03-ARCH-010/011).
- **CMOS-14-DEP-031** Media availability MUST follow the **split-media topology**
  (CMOS-03-ARCH-002): media nodes are independently addressable and scalable, and
  failure of one media node MUST NOT require the control plane to reconstruct media
  state (CMOS-03-ARCH-012).
- **CMOS-14-DEP-032** PostgreSQL HA (primary + replica with automatic failover) MUST be
  documented as the supported database posture for HA deployments; the platform MUST
  tolerate a database failover within a bounded reconnect window without data loss
  (transactional-outbox guarantee, CMOS-03-ARCH-030 / CMOS-05-EVT-010).
- **CMOS-14-DEP-033** Health and readiness signals (Volume 15) MUST gate load-balancer
  membership: a node MUST report **not ready** before it can serve, and MUST drain
  in-flight work on shutdown (§7).

## 6. Horizontal scaling (normative)

- **CMOS-14-DEP-040** Scaling MUST be **additive**: capacity is increased by adding
  control-plane and/or media nodes, never by a fundamental redesign (CMOS-00-ENG-015).
  There MUST be no node affinity required for correctness (CMOS-03-ARCH-011).
- **CMOS-14-DEP-041** Control-plane and media-plane capacity MUST be **independently
  scalable** (CMOS-00-ENG-006 / CMOS-03-ARCH-002); an operator can add media nodes for a
  call-volume spike without adding control-plane nodes, and vice versa.
- **CMOS-14-DEP-042** The scaling path from single-binary to clustered MUST NOT require
  changing tenant data, entity ids, or client-visible contracts — only topology and the
  dependency posture of §4.

## 7. Rolling upgrades & lifecycle (normative)

- **CMOS-14-DEP-050** In a multi-node topology, upgrades MUST be performable as a
  **rolling upgrade with no dropped Calls**: control-plane nodes are drained and
  replaced one at a time; media nodes stop accepting **new** Calls, are held until
  existing Calls end (or are gracefully re-homed where supported), then replaced. A
  planned upgrade MUST NOT terminate an established Call.
- **CMOS-14-DEP-051** On shutdown a node MUST **drain gracefully**: report not-ready,
  stop accepting new work, flush the transactional outbox, and finish or hand off
  in-flight work within a bounded timeout before exit (CMOS-14-DEP-033).
- **CMOS-14-DEP-052** Database schema migrations MUST be **backward-compatible across
  one release** so that old and new binaries can run **concurrently** during a rolling
  upgrade (expand/contract migration discipline). A migration that both old and new
  code cannot tolerate MUST be split across releases.
- **CMOS-14-DEP-053** Upgrades and rollbacks MUST be **reversible within a release
  line**; the SemVer contract rules (CONVENTIONS §4) govern which changes are
  compatible, and a MAJOR jump MUST carry a documented migration note.
- **CMOS-14-DEP-054** The single-binary topology MAY incur brief unavailability on
  restart; where zero-downtime is required, the operator MUST use a multi-node topology
  (§5). This trade-off MUST be documented, not hidden.

## 8. CPU architectures (normative)

- **CMOS-14-DEP-060** Every published artifact MUST support both **`arm64` and
  `amd64`** with feature and behaviour parity (CMOS-14-DEP-004). Conformance evidence
  MUST cover both, since media/codec paths are architecture-sensitive.

## 9. Backup, restore & disaster recovery (normative)

- **CMOS-14-DEP-070** A conforming deployment MUST support a **consistent backup** of
  all durable state: the PostgreSQL system of record **and** the Object Storage
  contents (recordings, voicemail, exports, diagnostic bundles), taken such that they
  can be restored to a mutually consistent point (CMOS-00-ENG-007/012).
- **CMOS-14-DEP-071** Restore MUST reconstitute a working deployment from those backups
  **plus** the config-as-code (`pbx.yaml`, §10) and the externally held secrets
  (Volume 9) — and MUST NOT depend on any node-local state that was not backed up
  (statelessness, CMOS-03-ARCH-010).
- **CMOS-14-DEP-072** Because history is append-only and deletion is a state transition
  (CMOS-00-ENG-012), backup/restore MUST preserve audit and configuration history, not
  just current state; a restore MUST NOT silently drop prior versions (e.g. CallFlow
  version history — Volume 2/13).
- **CMOS-14-DEP-073** The deployment MUST document its **recovery-point** and
  **recovery-time** posture; the numeric objectives (RPO/RTO) are set against Volume 17
  targets and SHOULD be verified by a periodic restore rehearsal, not assumed.
- **CMOS-14-DEP-074** Backups containing tenant data MUST remain tenant-scoped and
  encrypted at rest; backup artifacts are Objects and inherit the storage abstraction's
  authorization (CMOS-03-ARCH-041, Volume 9). Restores MUST NOT reassign or collide
  `tenant_id`s (CMOS-CONV-015).

## 10. Config-as-code (normative)

- **CMOS-14-DEP-080** Deployment and platform configuration MUST be expressible as a
  **declarative `pbx.yaml`** that captures intent (people, phones, numbers, call flows,
  hours, queues, policies, trunks) and can be **exported from and imported to** a
  running deployment (CMOS-00-ENG-005). The declared state is reconciled by the
  platform; the file is the desired state, not an imperative script.
- **CMOS-14-DEP-081** `pbx.yaml` MUST be **Git-reviewable**: deterministic ordering,
  stable serialisation, and diff-friendly so a change is reviewable as a pull request.
  Export→import→export MUST round-trip without spurious diffs.
- **CMOS-14-DEP-082** Applying `pbx.yaml` MUST go **through the public API** and its
  capability checks (CMOS-00-ENG-003 / CMOS-04-API-021); config-as-code is a client of
  the same API, with no privileged path. Import MUST be idempotent and MUST report a
  plan/diff before mutating (reconciliation, Volume 0 §6).
- **CMOS-14-DEP-083 — Secrets never in YAML.** `pbx.yaml` MUST NOT contain secrets
  (passwords, API keys, private keys, trunk credentials, TLS material). Secrets MUST be
  **referenced** (e.g. by external reference/URI) and resolved from an external secret
  manager; the secret mechanics are defined in **Volume 9** and are deferred here
  (CMOS-03 components; N-3). A YAML file that embeds a secret MUST be rejected by
  import.
- **CMOS-14-DEP-084** Config-as-code changes MUST leave an **audit trail** and MUST be
  reversible via the platform's append-only history/Time Machine (CMOS-00-ENG-012);
  applying a config revision never hard-deletes the prior configuration.

## Conformance notes

- **L1 (Contract):** Published artifacts exist for `amd64` and `arm64`, are signed with
  verifiable checksums, and the single binary boots against a lone PostgreSQL with no
  broker or cloud dependency (CMOS-14-DEP-001/004/005/020/021).
- **L2 (Behavioural):** Failover, rolling-upgrade, and restore scenarios are exercised
  in the Volume 16 chaos/failover suite — a control-plane node loss loses no committed
  state; a rolling upgrade drops no established Call; a restore from PostgreSQL + Object
  Storage + `pbx.yaml` + external secrets reconstitutes a working, history-preserving
  deployment (CMOS-14-DEP-030/050/052/070/071/072).
- **Topology invariance:** the same API/event conformance suite (Volumes 4/5) MUST pass
  identically on single-binary, split-media, and Kubernetes topologies
  (CMOS-14-DEP-013 / CMOS-03-ARCH-060).
- **Config-as-code:** export→import round-trips without spurious diff, import is
  idempotent and plan-first, and an import carrying an embedded secret is rejected
  (CMOS-14-DEP-081/082/083).

## Open items

- A machine-readable **`pbx.yaml` schema** under `contracts/` (deferred; this volume is
  prose-only at 0.3.0).
- Reference **systemd unit**, container spec, and Helm chart as companion artifacts
  (informative appendices in a later version).
- The precise **media re-homing / graceful-drain protocol** for zero-drop media-node
  upgrades — depends on the Volume 3 typed control↔media IDL (Volume 3 Open items).
- RPO/RTO numeric objectives — to be set jointly with Volume 17.

## Change log

- **0.3.0** — Initial implementation-grade draft: single-binary-primary / host-network
  Docker-secondary / Kubernetes-optional topologies with contract invariance;
  PostgreSQL as sole hard dependency and optional Redis/NATS/Object-Storage posture; HA
  via stateless control plane and split media; additive horizontal scaling; ARM64/AMD64
  parity; consistent backup/restore and disaster recovery preserving append-only
  history; rolling upgrades with no dropped calls via expand/contract migrations and
  graceful drain; and Git-reviewable declarative `pbx.yaml` config-as-code with secrets
  deferred to Volume 9 — all assigned stable requirement IDs.
