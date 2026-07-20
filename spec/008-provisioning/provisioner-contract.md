# Provisioner Contract — the vendor plugin interface

**Status:** DRAFT · **Version:** 0.3.0 · **Subsystem tag:** PROV

Companion to [`README.md`](README.md). This document specifies the **abstract
`Provisioner` interface** that every vendor plugin implements. It is deliberately
**vendor-neutral**: it contains no Yealink/Grandstream/Fanvil/Cisco/Poly/Snom/Mitel
specifics. A concrete `Provisioner` is a WASM plugin (Volume 12) loaded by the
substrate; this is the contract at that boundary.

The contract exists to satisfy CMOS-00-ENG-005 (no vendor template ever reaches an
admin) and CMOS-00-ENG-006 (control-plane logic talks to media/vendor concerns only
over typed interfaces). The substrate holds the lifecycle, security, and delivery; the
`Provisioner` holds only the vendor-specific knowledge of "how does *this* phone want
to be told what it already is."

> Note: field and method names here are the normative *surface*. The machine-readable
> plugin ABI (host functions, memory model, serialization) is Volume 12's concern;
> this document defines semantics, not the wire ABI.

---

## 1. Position in the system

```
 Device twin (desired state, vendor-neutral)
        │
        ▼
 ┌───────────────────────┐    build_config     ┌──────────────────────┐
 │  Substrate (control)  │ ──────────────────▶ │  Provisioner plugin  │
 │  lifecycle, security, │ ◀────────────────── │  (one per vendor)     │
 │  delivery, PKI, audit │   opaque artifact   └──────────────────────┘
 └───────────┬───────────┘
             │ signed one-time URL / mTLS (README §7)
             ▼
          Device
```

- **CMOS-08-PROV-100** A `Provisioner` MUST implement exactly four methods:
  `build_config`, `reboot`, `factory_reset`, `firmware_upgrade`. It MUST NOT expose
  additional externally-invoked entry points that mutate a Device; auxiliary pure
  helpers (e.g. capability declaration, model matching) are permitted but are not
  part of the mutation surface.
- **CMOS-08-PROV-101** A `Provisioner` MUST be **stateless across invocations**. All
  state is passed in by the substrate (the desired-state document and a per-call
  context) and returned in the result. A `Provisioner` MUST NOT rely on ambient state,
  persist to disk, or open network sockets (CMOS-08-PROV-044).
- **CMOS-08-PROV-102** A `Provisioner` MUST declare, at load time, the set of
  `vendor_key`/`model` patterns it handles and its declared plugin capabilities and
  resource limits (Volume 12). The substrate selects a `Provisioner` by
  `vendor_key`/`model` alone (CMOS-08-PROV-041).

## 2. Shared types (normative)

All inputs and outputs are JSON objects following CONVENTIONS §6 (`snake_case`,
UUIDv7 ids, RFC 3339 UTC time). The substrate provides them; the plugin returns them.

### 2.1 `DesiredState` (input to `build_config`)
The vendor-neutral projection of the Device twin + intent. The plugin reads it; it
MUST NOT assume any field beyond those declared, and MUST tolerate unknown fields
(CMOS-CONV-004).

| Field | Type | Notes |
|-------|------|-------|
| `device_id` | uuid | Subject Device. |
| `tenant_id` | uuid | Scope; the plugin MUST NOT cross it. |
| `vendor_key` | string | Selected vendor (open set). |
| `model` | string | Confirmed model. |
| `mac` | string | Normalised lower hex. |
| `network` | object | `vlan`, `switch`, `port`, `ip`, `location` (as known). |
| `lines` | array | Ordered line/identity assignments: `{ line_no, display_name, auth_ref, extension, codecs_intent }`. |
| `assigned_user_id` | uuid \| null | Null ⇒ shared/hot-desk. |
| `features` | object | Vendor-neutral feature intent (BLF, call-waiting, DND behaviour, time/NTP, locale). |
| `firmware_target` | object \| null | `{ version, image_object_ref, policy }` when a target is set. |
| `security` | object | Handles only: `credential_refs[]`, `server_cert_ref`, `client_cert_ref`, `pin_ca_ref` — never resolved secrets (§4). |
| `endpoints` | object | Where the Device should register/fetch: registrar/config hostnames the substrate owns. |

- **CMOS-08-PROV-110** `DesiredState.security` MUST carry only **references/handles**
  to credentials, certificates, and pinning material — never their plaintext. The
  substrate resolves handles to material at *delivery* time, outside the plugin
  (CMOS-08-PROV-042, Volume 9 secrets).

### 2.2 `BuildResult` (output of `build_config`)
| Field | Type | Notes |
|-------|------|-------|
| `content_type` | string | e.g. the vendor's config MIME/type; substrate treats body as opaque. |
| `artifact` | bytes | The generated configuration, opaque to the substrate. |
| `filename_hint` | string | Advisory only; the substrate controls the actual delivery URL (never a MAC-addressable path, CMOS-08-PROV-050). |
| `secret_placeholders` | array | Locations within `artifact` where the substrate MUST inject resolved secrets/certs at delivery (§4). |
| `reboot_required` | bool | Whether applying this config needs a reboot. |
| `firmware_directive` | object \| null | Optional firmware step the substrate should sequence. |

### 2.3 `ActionContext` (input to all methods)
| Field | Type | Notes |
|-------|------|-------|
| `correlation_id` | uuid | Shared across the operation's events (CMOS-05-EVT-020). |
| `idempotency_key` | string | Stable per logical action (§5). |
| `attempt` | int | Retry ordinal; the plugin MUST behave idempotently regardless. |
| `deadline` | timestamp | The plugin MUST return before this or yield a `TIMEOUT` error. |

### 2.4 `ActionResult` (output of `reboot`/`factory_reset`/`firmware_upgrade`)
| Field | Type | Notes |
|-------|------|-------|
| `outcome` | enum | `APPLIED \| ACCEPTED \| NOOP \| FAILED` (see §5). |
| `observed` | object | Any vendor-reported state to fold back into the Device twin (e.g. firmware version). |
| `error` | object \| null | Present iff `outcome=FAILED`; typed per §6. |

## 3. Methods (normative)

- **CMOS-08-PROV-120 — `build_config(DesiredState, ActionContext) → BuildResult`.**
  Produces a vendor-native configuration artifact that realises `DesiredState`. MUST
  be a **pure function** of its inputs (CMOS-08-PROV-101): identical inputs MUST yield
  a byte-identical `artifact` (modulo `secret_placeholders`, which are positional, not
  valued). MUST NOT embed resolved secrets (CMOS-08-PROV-110). MUST NOT perform I/O or
  delivery — it returns bytes; the substrate delivers them (CMOS-08-PROV-044).
- **CMOS-08-PROV-121 — `reboot(device_ref, ActionContext) → ActionResult`.** Requests
  the Device reboot using whatever vendor mechanism applies (e.g. a control action the
  substrate then dispatches). The method describes *intent and mechanism selection*;
  the substrate performs the authenticated dispatch. MUST be idempotent: a second
  `reboot` with the same `idempotency_key` MUST yield `NOOP`, not a second reboot.
- **CMOS-08-PROV-122 — `factory_reset(device_ref, ActionContext) → ActionResult`.**
  Returns the Device to factory state and, by contract, invalidates any on-device
  provisioned credentials. The substrate MUST pair this with revocation of the
  Device's platform-side tokens/credentials (CMOS-08-PROV-024). MUST be idempotent.
- **CMOS-08-PROV-123 — `firmware_upgrade(device_ref, firmware_target, ActionContext) →
  ActionResult`.** Drives the Device toward `firmware_target` (an Object reference with
  verified `sha256`, CMOS-08-PROV-070). MUST be idempotent: if the Device already runs
  the target version, MUST return `NOOP`. MUST NOT fetch the image itself; it directs
  the Device to the substrate-controlled, authenticated delivery URL
  (CMOS-08-PROV-073).

## 4. Secrets, certificates, and delivery boundary (normative)

- **CMOS-08-PROV-130** The plugin never sees plaintext secrets or private keys. It
  emits `secret_placeholders` describing *where* the substrate must inject resolved
  material into the artifact at delivery time. Injection, signing, and transport are
  the substrate's responsibility and occur outside the sandbox (CMOS-00-ENG-006,
  CMOS-00-ENG-012).
- **CMOS-08-PROV-131** The plugin MUST NOT log, echo, or return the resolved values of
  any handle. A conforming plugin's logs MUST be safe to persist in the clear.
- **CMOS-08-PROV-132** The plugin declares, per model, whether the vendor supports
  client certificates / certificate pinning (a capability flag). Where unsupported,
  the substrate falls back to one-time-token + short-TTL onboarding and records the
  reduced assurance on the twin (CMOS-08-PROV-053).

## 5. Idempotency (normative)

- **CMOS-08-PROV-140** Every method MUST be idempotent under retry keyed on
  `ActionContext.idempotency_key`: repeating a call with the same key and inputs MUST
  produce the same effect once, and MUST report `NOOP` (or the original `BuildResult`)
  on repeats — never a duplicated side effect (CMOS-04-API-023, CMOS-05-EVT-011).
- **CMOS-08-PROV-141** `build_config` is naturally idempotent by purity
  (CMOS-08-PROV-120). Action methods achieve idempotency by observing current state
  and treating an already-satisfied goal as `NOOP`.
- **CMOS-08-PROV-142** `outcome` semantics: `APPLIED` = the change was made now;
  `ACCEPTED` = the Device accepted the directive and will converge asynchronously;
  `NOOP` = already in the desired state / duplicate; `FAILED` = did not converge, see
  `error`.

## 6. Error handling (normative)

- **CMOS-08-PROV-150** On failure a method MUST return a typed `error`
  `{ class, code, retryable, detail }`, never an untyped string or a host trap. Error
  `class` MUST be one of: `TRANSIENT` (retry may succeed — timeout, device busy,
  transient network), `PERMANENT` (retry will not help — unsupported model, malformed
  desired state), `SECURITY` (auth/cert/pinning failure — surfaced to Volume 9), or
  `PRECONDITION` (Device not in a state where the action is valid).
- **CMOS-08-PROV-151** `retryable` MUST be consistent with `class`: `TRANSIENT` ⇒
  retryable; `PERMANENT`/`SECURITY` ⇒ not retryable without operator action. The
  substrate uses this to drive exponential backoff and, on exhaustion, dead-letter
  (CMOS-05-EVT-013) plus an operator-visible failure (CMOS-00-ENG-001).
- **CMOS-08-PROV-152** A plugin fault (trap, OOM, deadline overrun) MUST be treated by
  the substrate as a `TRANSIENT`/`FAILED` outcome for that attempt and MUST NOT be
  interpreted as success. Repeated faults MUST quarantine the plugin (Volume 12), not
  the lifecycle of unrelated Devices.
- **CMOS-08-PROV-153** No error path may leak secrets, resolved credentials, or full
  artifacts into event payloads or logs (CMOS-05-EVT-041, CMOS-08-PROV-131).

## 7. Sandbox & resource constraints (normative)

- **CMOS-08-PROV-160** A `Provisioner` runs under the Volume 12 WASM runtime with
  least-privilege limits: no ambient network, no filesystem, bounded CPU/memory/time
  per `ActionContext.deadline`. It receives inputs and returns outputs only through
  the plugin ABI (CMOS-08-PROV-044).
- **CMOS-08-PROV-161** A `Provisioner` MUST NOT reference another tenant's data; it
  operates strictly within the `tenant_id` of the `DesiredState`/`device_ref` passed
  to it (CMOS-00-ENG-008).

## 8. Conformance

A `Provisioner` plugin is contract-conformant when, driven by the substrate's plugin
harness:

- **build purity** — repeated `build_config` with identical `DesiredState` yields
  byte-identical `artifact` and stable `secret_placeholders` (CMOS-08-PROV-120).
- **no-secret guarantee** — no resolved secret/cert/key ever appears in any
  `artifact`, return value, or log line (CMOS-08-PROV-130/131/153).
- **idempotency** — each action method repeated under one `idempotency_key` produces a
  single effect and `NOOP` thereafter (CMOS-08-PROV-140).
- **typed errors** — induced failures return the correct `class`/`retryable`
  (CMOS-08-PROV-150/151); a forced fault is reported as `FAILED`, not success
  (CMOS-08-PROV-152).
- **isolation** — attempts at network/filesystem/cross-tenant access are denied by the
  sandbox and surfaced as `PERMANENT`/`SECURITY` errors (CMOS-08-PROV-160/161).

## Open items

- Precise `secret_placeholders` grammar (offset/marker vs. named-slot) — to align with
  the Volume 12 plugin ABI and Volume 9 secret-resolution handles.
- A vendor `capabilities` descriptor schema (cert support, factory-reset semantics,
  redirection-service support) — reserved for co-specification with Volume 12.
- Streaming/large-artifact return path for firmware-bundling vendors — deferred.

## Change log

- **0.3.0** — Initial implementation-grade draft of the abstract `Provisioner`
  interface: methods, shared types, secret/delivery boundary, idempotency, typed error
  taxonomy, sandbox constraints, and conformance; requirement IDs
  `CMOS-08-PROV-100…161`.
