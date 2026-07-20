# Volume 18 — Engineering Standards

**Status:** REVIEW · **Version:** 0.4.0 · **Subsystem tag:** ENG

This volume defines the engineering standards for **reference implementations** of
CommOS: error handling, logging, documentation, testing, an unsafe-code policy,
dependency and supply-chain policy, API/contract versioning discipline, CI/CD, and
the release process. It exists so that a reference implementation is trustworthy,
auditable, and operable — the prime directive applied to source code
(CMOS-00-ENG-001).

> **Scope and status of the stack.** Conformance is defined against **contracts,
> not languages** (Volume 0 §1, N-6; Volume 3). The Rust/Tokio/Axum/SQLx stack
> below is the **RECOMMENDED** reference stack (Volume 3), not a mandate. An
> implementation in any language is fully CommOS-conformant if it passes the
> Volume 16 suite for its declared profile and level. Requirements in this volume
> that name a Rust tool are **normative for the reference implementation** and
> **informative (a worked example of the intent)** for any other; the *intent* each
> encodes — memory safety, auditability, reproducibility — is normative for all.

---

## 1. Reference stack (normative for the reference implementation)

- **CMOS-18-ENG-001** The reference implementation SHOULD use the stack recorded in
  Volume 3 and its ADR (Volume 19): **Rust** (async via **Tokio**), HTTP/API via
  **Axum**, database access via **SQLx** against **PostgreSQL**, WASM plugins via
  **Wasmtime** (Volume 12), and an S3-compatible Object Storage client behind the
  Volume 3 storage interface. Any component MAY be substituted provided the
  contracts and Volume 17 targets still hold.
- **CMOS-18-ENG-002** The reference implementation MUST target a pinned, stated
  minimum toolchain (a `rust-toolchain.toml` pinning the channel) and MUST build
  reproducibly from a committed `Cargo.lock`. A non-Rust implementation MUST provide
  an equivalent pinned toolchain and lockfile.
- **CMOS-18-ENG-003** The default deployment artifact MUST remain a **single native
  binary + PostgreSQL (+ optional Redis)** runnable under systemd (CMOS-00-ENG-014);
  build tooling MUST NOT introduce a mandatory Kubernetes, message-broker cluster, or
  cloud dependency (Volume 0, N-3).

## 2. Code structure & the plane boundary (normative)

- **CMOS-18-ENG-010** Source MUST keep the **Control Plane** and **Media Plane**
  separable: they communicate only over typed interfaces, never shared mutable
  memory, even when compiled into one binary (CMOS-00-ENG-006, CMOS-03-ARCH-001).
  The build MUST be able to produce split-media binaries from the same tree without
  changing control-plane logic (CMOS-03-ARCH-002).
- **CMOS-18-ENG-011** Persistence, Object Storage, Event Bus binding, and
  distributed-state layer MUST each sit **behind a trait/interface**; no subsystem
  may depend on a concrete backend (CMOS-00-ENG-007, CMOS-03-ARCH-040). Swapping
  Postgres/NATS/S3 backends MUST be a wiring change, not a code rewrite.
- **CMOS-18-ENG-012** Every persisted entity and every Event MUST be `tenant_id`-scoped
  at the type level so that omitting the scope is a compile-time or query-builder
  error, not a runtime slip (defence in depth; CMOS-00-ENG-008, CMOS-03-ARCH-050).

## 3. Error handling (normative)

- **CMOS-18-ENG-020** Fallible operations MUST return typed results
  (`Result<T, E>`), never sentinel values or silent failure. Library/domain code
  MUST NOT `panic!`, `unwrap()`, or `expect()` on recoverable conditions; panics are
  reserved for true invariant violations (bugs) and MUST be caught at the task/worker
  boundary so a single fault cannot crash the host (CMOS-03-ARCH-051 rationale).
- **CMOS-18-ENG-021** Errors MUST carry enough structure to render the API's typed
  error body (Volume 4) and to serve the design tenet **"every error message names
  the intent that failed and the next action"** (Volume 0 §6). Internal error detail
  MUST NOT leak secrets or cross-tenant data into responses or logs.
- **CMOS-18-ENG-022** Error taxonomy MUST distinguish **client** (4xx-class),
  **server** (5xx-class), and **overload/backpressure** (retryable) conditions so
  callers can act correctly; retryable errors MUST be idempotency-safe to retry
  (Volume 4; CMOS-05-EVT-011).

## 4. Structured logging & observability hooks (normative)

- **CMOS-18-ENG-030** Logs MUST be **structured** (machine-parseable key/value, JSON
  by default), never free-text-only, and MUST carry `tenant_id`, `correlation_id`,
  and — where present — `causation_id` and `traceparent` so a Call or operation is
  reconstructable end-to-end (Volume 5 envelope; Volume 15).
- **CMOS-18-ENG-031** Logs, metrics, and traces MUST use the field names and semantic
  conventions defined by Volume 15; emitting telemetry is a first-class requirement,
  not an afterthought (Volume 3 §concurrency; CMOS-00-ENG-004).
- **CMOS-18-ENG-032** Logs MUST NOT contain secrets, raw media, full recordings, or
  unmasked PII; PII fields follow the Event `x-pii` marking so redaction is automatable
  (CMOS-05-EVT-041/042). Log levels MUST be configurable without recompilation.

## 5. Unsafe-code policy (normative)

- **CMOS-18-ENG-040** `unsafe` is **deny-by-default**. The reference implementation
  MUST set `#![forbid(unsafe_code)]` (or `unsafe_code = "deny"` lints) at every crate
  root; a non-Rust implementation MUST adopt the strictest equivalent memory-safety
  posture its language offers.
- **CMOS-18-ENG-041** Any `unsafe` block that is unavoidable (e.g. an FFI boundary to
  a codec or SIP stack) MUST be (a) explicitly `allow`-ed at the **narrowest** scope,
  (b) accompanied by a `// SAFETY:` comment stating the invariant that makes it sound,
  and (c) reviewed and signed off by a second engineer with the review recorded. An
  `unsafe` block without a justification and recorded review MUST NOT merge.
- **CMOS-18-ENG-042** `unsafe` surface MUST be minimised and encapsulated behind a
  safe API; it MUST be covered by tests (including, where feasible, Miri or a
  sanitiser run in CI). Untrusted input (WASM plugins, network parsers) MUST NOT reach
  `unsafe` code without validation (CMOS-03-ARCH-051).

## 6. Dependency, licence & supply-chain policy (normative)

- **CMOS-18-ENG-050** Dependencies MUST be vetted and pinned via a committed lockfile;
  CI MUST run a vulnerability audit (`cargo audit` / `cargo deny advisories`, or the
  language equivalent) and MUST fail on an unwaived advisory. Waivers MUST be
  time-bounded and recorded with justification.
- **CMOS-18-ENG-051** Every dependency's licence MUST be checked (`cargo deny
  licenses` or equivalent) against an allowlist compatible with the project's licence;
  a disallowed or unknown licence MUST fail CI. Copyleft that would compromise the
  single-binary distribution MUST be excluded.
- **CMOS-18-ENG-052** Supply-chain integrity MUST be enforced: reproducible builds
  from the pinned lockfile, dependency provenance/source verification, and a generated
  **SBOM** per release. New or bumped dependencies MUST be reviewed; adding one purely
  to gain a minor convenience is weighed against the prime directive
  (CMOS-00-ENG-001) — fewer, well-understood dependencies are preferred.
- **CMOS-18-ENG-053** The build MUST NOT execute unaudited network-fetching build
  scripts; anything a `build.rs` (or equivalent) does at build time is in scope for
  review.

## 7. Documentation (normative)

- **CMOS-18-ENG-060** Every public API item in the reference implementation MUST carry
  doc comments; the crate MUST build docs with **`#![deny(missing_docs)]`** on public
  surfaces and MUST build with `cargo doc` warning-free in CI.
- **CMOS-18-ENG-061** Documentation and code comments MUST use the canonical GLOSSARY
  terms exactly (Organisation, User, Identity, Device, Extension, Call, Event, …) and
  MUST NOT reintroduce legacy PBX vocabulary except as a noted alias. Where behaviour
  implements a normative requirement, the code SHOULD cite the requirement ID
  (`CMOS-<VOL>-<SUBSYS>-NNN`) so implementation traces to specification.
- **CMOS-18-ENG-062** A design decision that diverges from, or resolves a tension in,
  the specification MUST be recorded as an ADR (Volume 19); code comments are not a
  substitute for an ADR.

## 8. Testing requirements (normative)

Volume 16 owns *conformance*; this section governs the implementation's *own* tests.

- **CMOS-18-ENG-070** The reference implementation MUST ship unit and integration
  tests and MUST wire the **Volume 16 conformance harness** (`conformance/run.py` at
  L1, plus L2/L3 as they land) into its own CI as a required gate. Passing one's own
  tests is necessary but **not sufficient**; the conformance suite is the arbiter
  (Volume 16 §1).
- **CMOS-18-ENG-071** Tests MUST be deterministic (controlled clock/seed) or
  quarantined out of the gating set (CMOS-16-TEST-004). Behavioural tests SHOULD be
  expressed so they can feed, or be fed by, the Volume 16 scenario fixtures.
- **CMOS-18-ENG-072** A merge MUST NOT reduce conformance coverage: every fixed defect
  adds a regression test citing the requirement ID it protects (CMOS-16-TEST-100), and
  a contract-incompatible change is blocked unless accompanied by the required version
  bump (§9, CMOS-16-TEST-101).
- **CMOS-18-ENG-073** Code formatting and linting MUST be enforced in CI
  (`cargo fmt --check`, `cargo clippy -D warnings`, or equivalents); style is settled
  by the tool, not by review.

## 9. API & contract versioning discipline (normative)

This section binds implementation practice to CONVENTIONS §4.

- **CMOS-18-ENG-080** Implementations MUST honour SemVer for contracts: **PATCH** is
  editorial, **MINOR** is backwards-compatible additions, **MAJOR** is a breaking
  change requiring an ADR and migration note (CONVENTIONS §4). A change that removes
  or narrows a field, tightens a constraint, changes a type, or renames an Event/entity
  type within a MAJOR line MUST NOT ship as anything less than MAJOR
  (CMOS-CONV-001..005, CMOS-05-EVT-030/031).
- **CMOS-18-ENG-081** Consumers MUST implement the **tolerant-reader** rule: ignore
  unknown fields and never reject an envelope solely for a higher same-MAJOR
  `specversion` (CMOS-CONV-004, CMOS-05-EVT-004). This is a testable release gate
  (CMOS-16-TEST-023/075).
- **CMOS-18-ENG-082** New schema fields MUST be optional or carry a
  behaviour-preserving default; enum members MUST NOT be removed or renumbered within a
  MAJOR line (CMOS-CONV-001/002). The reference implementation MUST expose its
  contract version and a schema-registry endpoint (`GET /v1/events/schemas`, Volume 4)
  so consumers and the Volume 16 adapter can verify shape at runtime.
- **CMOS-18-ENG-083** Requirement IDs are permanent contract points: a deleted
  requirement is marked `WITHDRAWN`, never renumbered (CONVENTIONS §2); tooling that
  generates or checks IDs MUST enforce non-reuse.

## 10. CI/CD (normative)

- **CMOS-18-ENG-090** The conformance workflow at
  [`.github/workflows/conformance.yml`](../../.github/workflows/conformance.yml) is the
  **contract gate**: it installs the harness dependencies and runs
  `python3 conformance/run.py` on every push and pull request; a red run MUST block
  merge (Volume 16 §12).
- **CMOS-18-ENG-091** The implementation's CI MUST additionally run, as required gates:
  format + lint (CMOS-18-ENG-073), the vulnerability/licence/supply-chain checks
  (§6), the unsafe-audit (§5), the doc build (§7), and the implementation's own test
  suite plus the Volume 16 behavioural suite as it lands (§8). Load and chaos suites
  (Volume 16 §8–§9) run on a scheduled/pre-release cadence, not necessarily per-commit.
- **CMOS-18-ENG-092** CI MUST be **hermetic and reproducible**: no dependence on
  mutable external state, pinned toolchain and actions, and buildable offline from the
  lockfile (CMOS-18-ENG-002/052).
- **CMOS-18-ENG-093** Deployment automation MUST support **rolling upgrade with no
  dropped Calls** (CMOS-17-PERF-070) and expand/contract migrations
  (CMOS-17-PERF-072); a release that cannot be rolled out this way is not release-ready.

## 11. Release process & SemVer (normative)

- **CMOS-18-ENG-100** Releases are versioned `MAJOR.MINOR.PATCH` and MUST state the
  **contract/spec version** they implement and the **profiles + level** they claim
  (`CommOS <profile> conformant, level <Lx>, spec vX.Y`; CONVENTIONS §3,
  CMOS-16-TEST-003). A release MUST NOT claim a level it has not passed.
- **CMOS-18-ENG-101** Every release MUST publish: a change log, the SBOM
  (CMOS-18-ENG-052), the conformance result (harness version + suites + coverage
  report, CMOS-16-TEST-050), and any migration notes for a MAJOR contract change.
- **CMOS-18-ENG-102** Security-relevant and configuration-relevant actions in the
  running system are append-only and auditable (CMOS-00-ENG-012); the release and its
  provenance are likewise recorded so a deployed artifact is traceable to its source
  and its conformance evidence.
- **CMOS-18-ENG-103** A MAJOR contract bump MUST be rare, batched, and accompanied by
  an ADR (Volume 19) and migration guidance (CONVENTIONS §4); MINOR and PATCH releases
  MUST preserve L1 conformance of prior same-MAJOR implementations.

## Conformance notes

- This volume's requirements are largely **process** requirements on a reference
  implementation; they are not part of the black-box conformance label, which is
  contract-defined (Volume 16 §1). Where an ENG requirement has a runtime-observable
  effect — tolerant reader (CMOS-18-ENG-081), version exposure (CMOS-18-ENG-082),
  no-dropped-call upgrade (CMOS-18-ENG-093) — it is verified by the cited Volume 16/17
  test.
- The gate that a CI must be green against is real and executable today:
  `conformance/run.py` via `.github/workflows/conformance.yml`.
- The Rust stack is RECOMMENDED; a differently-built implementation satisfies this
  volume by meeting the *intent* of each requirement (safety, auditability,
  reproducibility, versioning discipline) with its own toolchain.

## Open items

- Concrete licence allowlist and advisory-waiver register (candidate for a repo-level
  policy file in v0.4).
- SBOM format selection (SPDX vs CycloneDX) and provenance attestation mechanism.
- Minimum-supported toolchain version pinning for the reference implementation.
- Mapping of each ENG requirement to the specific CI job that enforces it, once the CI
  grows beyond the contract gate.

## Change log

- **0.3.0** — Initial implementation-grade draft. Specified the RECOMMENDED Rust
  reference stack, code/plane-boundary structure, error-handling and structured-logging
  conventions, a deny-by-default unsafe policy with justified+reviewed exceptions, the
  dependency/licence/supply-chain policy, documentation and testing requirements,
  API/contract versioning discipline aligned to CONVENTIONS §4, the CI/CD gate at
  `.github/workflows/conformance.yml`, and the release/SemVer process. Assigned IDs
  CMOS-18-ENG-001…103.
