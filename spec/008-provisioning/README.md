# Volume 8 — Provisioning

**Status:** DRAFT · **Version:** 0.3.0 · **Subsystem tag:** PROV

Zero-touch device provisioning is the feature that most directly proves the prime
directive (CMOS-00-ENG-001): an operator plugs a phone into a switch port and, within
one screen and one click, the device is safe, configured, and registered — with no
vendor template, no TFTP root, and no hand-edited config in sight. This is the
"TP-Link Omada for phones" experience: the network discovers the endpoint, the
platform proposes the intent, the operator approves the *person*, and the substrate
reconciles reality to that intent.

This volume specifies the **Device lifecycle**, **network discovery**, the **operator
approval workflow**, **secure (zero-trust) onboarding**, **one-click replacement**,
**firmware management**, and the **vendor abstraction** by which every phone vendor is
a plugin behind a single `Provisioner` contract. The admin NEVER sees a vendor
template (CMOS-00-ENG-005).

Companion documents:
- [`provisioner-contract.md`](provisioner-contract.md) — the abstract `Provisioner`
  plugin interface a WASM plugin (Volume 12) implements, without vendor specifics.

Related contracts: Device entity ([Volume 2 entities](../002-domain-model/entities.md#device)),
Device state machine ([Volume 2 state-machines](../002-domain-model/state-machines.md#device)),
Device/Provisioning/Registration events ([Volume 5 catalog](../005-events/catalog.md#device-provisioning--registration)),
device endpoints ([Volume 4 endpoints](../004-api/endpoints.md#endpoints--numbering)).

---

## 1. Scope

In scope: discovery of physical/virtual endpoints on the operator network; the
approval → provisioning → registration path; secure delivery of configuration to a
Device; firmware lifecycle; device replacement; and the plugin boundary that isolates
all vendor knowledge into a `Provisioner`.

Out of scope (owned elsewhere): the Device entity shape (Volume 2), the event
envelope and delivery guarantees (Volume 5), capability grants and the policy engine
(Volume 9), PKI issuance mechanics (Volume 9), the WASM runtime and plugin packaging
(Volume 12), and media/registration signalling internals (Volume 7). This volume
consumes those contracts; it does not redefine them.

> Note (informative): "provisioning" here means *bringing a Device to OPERATIONAL
> safely and declaratively*. It is not dialplan configuration — a phone has no
> dialplan in CommOS; it has an Identity, an assigned User (optionally), and a set of
> line keys derived from intent.

## 2. Design principles (traceability)

Every requirement below serves one or more Volume 0 invariants:

| Principle | Invariant | How this volume honours it |
|-----------|-----------|----------------------------|
| Admin describes intent, not templates | CMOS-00-ENG-005 | Operator picks a User/model; the platform + `Provisioner` synthesise config. No vendor config is ever surfaced. |
| Zero-trust onboarding | CMOS-00-ENG-010 | Short-lived signed URLs, one-time tokens, mutual auth, cert pinning, explicit approval, revocation. |
| Simpler to operate | CMOS-00-ENG-001 | One discovery inbox, one approval click, one replacement flow across all vendors. |
| Everything attributable | CMOS-00-ENG-011 | A Device's assigned User and every provisioning action are recorded and audited. |
| Immutable history | CMOS-00-ENG-012 | Every lifecycle transition is an append-only audit entry + event; retirement is a state, not a delete. |
| Multi-tenant isolation | CMOS-00-ENG-008 | Discovery, tokens, and config are tenant-scoped; a Device belongs to exactly one Organisation. |
| Declarative reconciliation | CMOS-00-ENG-005 | Config is a function of desired intent + Device twin; drift is re-reconciled, not patched by hand. |

## 3. The Device lifecycle (normative)

The Device state machine is **frozen in Volume 2**
([state-machines.md](../002-domain-model/state-machines.md#device)). This volume MUST
NOT diverge from it; it only specifies the *behaviour* attached to each transition.

```
DETECTED → PENDING → APPROVED → PROVISIONED → OPERATIONAL → (REPLACING) → RETIRED*
              │reject                                          ↑
              ▼                                                └ replacement provisioned
           REJECTED*
```

- **CMOS-08-PROV-001** An implementation MUST drive a Device only through the
  transitions enumerated in the Volume 2 Device state machine, MUST reject any other
  transition with `409 conflict` / `code=invalid_transition`, and MUST emit the named
  event for each transition it performs (CMOS-02-DOM-007, CMOS-05-EVT-002).
- **CMOS-08-PROV-002** A Device MUST NOT be reachable for signalling or media (i.e.
  MUST NOT be allowed to register, CMOS-08-PROV-030) until it has reached
  `PROVISIONED`. `DETECTED` and `PENDING` Devices are untrusted (CMOS-00-ENG-010).
- **CMOS-08-PROV-003** Retirement is a state transition to `RETIRED`, never a hard
  delete; the Device record and its provisioning history MUST remain resolvable
  (CMOS-02-DOM-003, CMOS-00-ENG-012). Retiring a Device MUST revoke its outstanding
  provisioning tokens, credentials, and pinned certificates (CMOS-08-PROV-024).

| Transition | Behaviour specified here | Event (Volume 5) |
|------------|--------------------------|------------------|
| → DETECTED | Discovery ingests a candidate endpoint (§4). | `DeviceDetected` |
| DETECTED → PENDING | Candidate enqueued to the operator approval inbox. | `DeviceDetected` (pending) |
| PENDING → APPROVED | Operator approves + assigns intent (§5); requires `provision.devices`. | `DeviceApproved` |
| PENDING → REJECTED* | Operator rejects; candidate quarantined (§5). | `DeviceRejected` |
| APPROVED → PROVISIONED | `Provisioner.build_config` + secure delivery (§6, §7). | `ProvisioningStarted` → `ProvisioningFinished` |
| PROVISIONED → OPERATIONAL | Device registers successfully (§8). | `RegistrationSucceeded` |
| OPERATIONAL → REPLACING | Replacement workflow started (§9). | `DeviceReplacementStarted` |
| REPLACING → OPERATIONAL | Replacement provisioned + registered. | `ProvisioningFinished`, `RegistrationSucceeded` |
| any non-terminal → RETIRED* | Operator retires the Device. | `DeviceRetired` |
| OPERATIONAL → OPERATIONAL | Registration refresh or loss. | `RegistrationSucceeded` / `RegistrationLost` |

## 4. Network discovery (normative)

Discovery turns "a phone appeared on the network" into a `DETECTED` Device candidate
without any manual data entry.

- **CMOS-08-PROV-010** The platform MUST support at least one **passive** discovery
  method and SHOULD support several, correlating their signals into one candidate:
  DHCP fingerprinting (option 60 vendor class, option 55 parameter list), LLDP/CDP
  neighbour data (switch, port, VLAN), mDNS/SSDP announcements, and SIP `REGISTER`
  attempts from unknown endpoints observed at the edge.
- **CMOS-08-PROV-011** A discovery signal MUST be normalised into a candidate carrying
  at minimum a normalised `mac` (lower hex, no separators), an inferred `vendor_key`
  (open set per CMOS-CONV-017), an inferred `model` where derivable, and observed
  `network` facts (`vlan`, `switch`, `port`, `ip`, `location`). Unknown fields MUST
  be recorded, not discarded.
- **CMOS-08-PROV-012** Discovery MUST be **tenant-scoped**: a candidate is attributed
  to exactly one Organisation from the discovering site/network binding, and MUST NOT
  be visible to another tenant (CMOS-00-ENG-008). A discovery signal that cannot be
  attributed to a tenant MUST be held in an unattributed quarantine, never
  auto-assigned.
- **CMOS-08-PROV-013** Discovery MUST be **idempotent** on `mac` within a tenant:
  repeated signals for a known candidate update the existing Device twin (bumping
  `version`) and MUST NOT create duplicates. Re-appearance of a `RETIRED` Device's MAC
  MUST surface as a new candidate for explicit operator decision, not silent revival.
- **CMOS-08-PROV-014** `vendor_key`/`model` inference is a hint, not a trust anchor.
  An implementation MUST NOT grant any capability, config, or network access on the
  basis of self-declared discovery data alone (CMOS-00-ENG-010).
- **CMOS-08-PROV-015** The `DeviceDetected` event MUST NOT carry secrets or credential
  material; it carries only discovery facts and the candidate `subject` id
  (CMOS-05-EVT-041).

## 5. Operator approval workflow (normative)

Approval is where a human authorises *who the Device is for* — the only step that
requires judgement. Everything else is automatic.

- **CMOS-08-PROV-020** Approving a candidate (PENDING → APPROVED) MUST require the
  `provision.devices` capability (CMOS-00-ENG-009) and MUST be recorded as an
  append-only audit entry naming the actor Identity, the Device, and the assigned
  intent (CMOS-00-ENG-012, CMOS-09-SEC audit).
- **CMOS-08-PROV-021** At approval the operator MUST express **intent only**: select
  the Device model (confirm/override the inferred one), and optionally assign a User
  (`assigned_user_id`) or mark the Device shared/hot-desk (`assigned_user_id = null`,
  CMOS-02-DOM-015). The operator MUST NOT be shown, asked for, or able to edit any
  vendor configuration template, SIP profile, or provisioning file
  (CMOS-00-ENG-005). Line keys, codecs, NTP, and dial behaviour are derived by the
  platform + `Provisioner`, never entered by hand.
- **CMOS-08-PROV-022** The approval surface MUST be **approved-devices-only**: a
  Device that has not been explicitly approved MUST NOT receive configuration or be
  permitted to register (CMOS-08-PROV-002). There is no "trust all on VLAN N" bypass.
- **CMOS-08-PROV-023** Rejection (PENDING → REJECTED) MUST quarantine the MAC for the
  tenant so the endpoint is not re-proposed on every re-appearance, and MUST emit
  `DeviceRejected`. A rejected MAC MAY be un-quarantined only by an actor holding
  `provision.devices`.
- **CMOS-08-PROV-024** Approval and provisioning artifacts MUST be **revocable**:
  revoking an approval, retiring the Device, or reassigning it MUST invalidate any
  outstanding provisioning URL, one-time token, and issued device credential before
  the operation is acknowledged (CMOS-00-ENG-010).

> Note (informative): the target UX is a single "Pending devices" inbox. Each row
> shows what the network sees (vendor, model, switch/port, location) and a "who is
> this for?" picker. Approve → the phone reboots into its configured state minutes
> later. The operator never learns the phone is a Yealink T54W versus a Fanvil V65.

## 6. Vendor abstraction — the `Provisioner` plugin (normative)

All vendor knowledge lives behind one contract. The substrate knows only the abstract
interface; each vendor is a plugin.

- **CMOS-08-PROV-040** Every supported vendor MUST be implemented as a **plugin**
  conforming to the `Provisioner` contract
  ([`provisioner-contract.md`](provisioner-contract.md)), exposing exactly the
  methods `build_config`, `reboot`, `factory_reset`, and `firmware_upgrade`. The
  reference vendor set is: Yealink, Grandstream, Fanvil, Cisco, Poly, Snom, Mitel.
  The set is open (`vendor_key`, CMOS-CONV-017); adding a vendor MUST be possible
  without changing the substrate or any other volume.
- **CMOS-08-PROV-041** The substrate MUST select a `Provisioner` solely by the
  Device's `vendor_key`/`model`, MUST pass it a *vendor-neutral desired-state
  document* (the Device twin projection: identity/line assignment, network, firmware
  target, feature intent), and MUST treat the returned artifact as opaque bytes plus
  a content type. The substrate MUST NOT interpret, template, or edit vendor config
  (CMOS-00-ENG-005, CMOS-00-ENG-006 typed-interface separation).
- **CMOS-08-PROV-042** A `Provisioner` MUST be **pure with respect to secrets**: it
  MUST NOT embed long-lived secrets in generated config and MUST reference credentials
  and certificates via the platform's secret/PKI handles (Volume 9), resolved at
  delivery time (CMOS-00-ENG-012, CMOS-09-SEC secrets). Generated artifacts MUST NOT
  be logged verbatim where they would expose those resolved secrets.
- **CMOS-08-PROV-043** `Provisioner` operations MUST be **idempotent** and MUST report
  structured, typed errors (transient vs. permanent, retryable vs. not) so the
  substrate can retry or dead-letter deterministically. Full semantics — inputs,
  outputs, idempotency, and error taxonomy — are in
  [`provisioner-contract.md`](provisioner-contract.md).
- **CMOS-08-PROV-044** A `Provisioner` runs in the sandboxed WASM plugin runtime
  (Volume 12) with declared, least-privilege resource limits and MUST NOT be granted
  ambient network or filesystem access; it produces config, it does not deliver it.
  Delivery, signing, and transport are the substrate's responsibility (§7).

## 7. Secure onboarding (zero-trust) (normative)

Onboarding is the highest-risk surface in any PBX: legacy systems served
`http://pbx/provision/<mac>.cfg` to anyone who asked, leaking SIP credentials to the
whole LAN. CommOS forbids that model outright (CMOS-00-ENG-010).

- **CMOS-08-PROV-050** Configuration MUST be delivered only via a **short-lived,
  signed provisioning URL** bound to a single Device and a single `provisioning_url`
  MUST embed an unguessable token, an `expires_at` (SHORT — minutes-scale, tenant
  configurable within a bounded maximum), and a tenant scope. Predictable,
  MAC-addressable, or long-lived URLs (e.g. `https://pbx/provision/<mac>.cfg`) are
  **prohibited** and an implementation MUST NOT expose them.
- **CMOS-08-PROV-051** Each provisioning URL/token MUST be **one-time**: the first
  successful, authenticated retrieval MUST invalidate it. A second use MUST fail with
  `410 gone` / `code=token_consumed` and MUST raise an audit + security event.
- **CMOS-08-PROV-052** The provisioning endpoint MUST perform **mutual
  authentication**: it authenticates the Device (one-time token and, where the vendor
  supports it, a client certificate) and the Device authenticates the server (TLS
  1.3+, valid server certificate). Plain HTTP provisioning MUST NOT be offered
  (CMOS-04-API-001, CMOS-09-SEC transport).
- **CMOS-08-PROV-053** Where the vendor supports it, the platform MUST support
  **certificate pinning** (pinning the provisioning/server certificate or CA in the
  device profile) and MUST support issuing a per-Device client certificate from the
  platform PKI (Volume 9) for ongoing mTLS. Where a vendor lacks certificate support,
  the one-time token + short TTL + approved-devices-only path MUST still hold, and the
  reduced assurance MUST be recorded on the Device twin.
- **CMOS-08-PROV-054** Trust MUST NOT be inferred from network location: being on the
  provisioning VLAN, having a known MAC, or presenting known vendor headers MUST NOT
  by itself authorise config retrieval (CMOS-00-ENG-010). Only an approved Device
  presenting a valid, unconsumed one-time token (and cert where available) is served.
- **CMOS-08-PROV-055** Provisioning credentials issued to a Device MUST be
  **revocable and rotatable** without physical access: revocation MUST take effect at
  the next registration/provisioning attempt, and rotation MUST be a
  reprovision-and-reboot, not a hand edit.
- **CMOS-08-PROV-056** Every provisioning artifact fetch (success, expiry, replay,
  auth failure) MUST produce an append-only audit entry and, for anomalies (replay,
  auth failure, unknown MAC on a known token), a security-relevant event
  (CMOS-00-ENG-012, Volume 9).
- **CMOS-08-PROV-057** Provisioning artifacts are **Objects** where persisted, stored
  via the Object Storage abstraction with integrity (`sha256`) and retention, never as
  inline blobs or files on a well-known TFTP/HTTP root (CMOS-02-DOM-013,
  CMOS-00-ENG-007).

## 8. Registration & operational state (normative)

- **CMOS-08-PROV-030** A Device MUST be permitted to register only in `PROVISIONED`
  (first registration → `OPERATIONAL`) or `OPERATIONAL`/`REPLACING` states, using
  credentials issued during provisioning. A registration attempt from a Device not in
  an eligible state MUST be refused and MUST raise a security-relevant event
  (CMOS-00-ENG-010).
- **CMOS-08-PROV-031** First successful registration MUST transition PROVISIONED →
  OPERATIONAL and emit `RegistrationSucceeded`. Subsequent refreshes emit
  `RegistrationSucceeded`; loss of registration emits `RegistrationLost` without
  leaving `OPERATIONAL` (transient loss is observed, not a lifecycle change).
- **CMOS-08-PROV-032** The Device twin MUST reflect observed `registration` state and
  `firmware` from the operational Device, so drift between intent and reality is
  visible and reconcilable (CMOS-00-ENG-005, CMOS-02-DOM-005).

## 9. One-click device replacement (normative)

The canonical failure story: a desk phone dies. A replacement of the same or a
different model is plugged in; the operator moves the person over in one action.

- **CMOS-08-PROV-060** The platform MUST provide a **replacement workflow** that binds
  a newly `DETECTED`/`PENDING` candidate to an existing `OPERATIONAL` Device's intent:
  the operator selects the dead Device (or its User) and the replacement candidate;
  the platform transfers the assigned User, Extension/line assignment, and feature
  intent to the replacement, provisions it via its `Provisioner`, and reboots it into
  service. Requires `provision.devices`.
- **CMOS-08-PROV-061** Replacement MUST move the outgoing Device to `REPLACING` and,
  on successful provisioning + registration of the incoming Device, retire the
  outgoing Device (`DeviceRetired`) and bring the incoming Device to `OPERATIONAL`.
  Events emitted MUST be `DeviceReplacementStarted`, then
  `ProvisioningStarted`/`ProvisioningFinished` and `RegistrationSucceeded`, then
  `DeviceRetired` for the outgoing Device — all sharing one `correlation_id`
  (CMOS-05-EVT-020).
- **CMOS-08-PROV-062** Replacement MUST be **vendor-agnostic**: the incoming Device
  MAY be a different vendor/model than the outgoing one. The operator MUST NOT need to
  reconcile templates; the platform re-derives config for the new vendor from the same
  intent (CMOS-00-ENG-005).
- **CMOS-08-PROV-063** On replacement, the outgoing Device's provisioning tokens and
  credentials MUST be revoked (CMOS-08-PROV-024) so a recovered/stolen dead phone
  cannot re-register.

## 10. Firmware management (normative)

- **CMOS-08-PROV-070** Firmware images MUST be stored and served as **Objects**
  (`kind=FIRMWARE`) via the Object Storage abstraction, with `sha256` integrity and
  retention metadata (CMOS-00-ENG-007, CMOS-02-DOM-013). The platform MUST verify the
  image digest before offering it to a Device.
- **CMOS-08-PROV-071** Firmware upgrade MUST be driven through the `Provisioner`'s
  `firmware_upgrade` method against a desired firmware target on the Device twin; the
  admin expresses a target version/policy as intent and the platform reconciles — the
  admin MUST NOT hand-craft firmware URLs into vendor config (CMOS-00-ENG-005).
- **CMOS-08-PROV-072** Firmware upgrade MUST be **staged and reversible in intent**:
  the platform records the intended and observed firmware, MUST support per-tenant and
  per-model target policies (pin, allow, forbid), and SHOULD support phased rollout
  (canary → fleet). A failed upgrade MUST NOT silently loop; after a bounded retry
  budget it MUST surface an operator-visible failure (CMOS-00-ENG-001).
- **CMOS-08-PROV-073** Firmware target changes and upgrade outcomes MUST be audited
  and MUST NOT hard-delete prior firmware records (CMOS-00-ENG-012). Firmware
  delivery MUST use the same authenticated, non-guessable transport as config
  (CMOS-08-PROV-050..054); firmware MUST NOT be exposed at a public well-known path.

## 11. Events emitted (normative)

This volume is a producer of the Device, Provisioning, and Registration families in
the [Volume 5 catalog](../005-events/catalog.md#device-provisioning--registration).

- **CMOS-08-PROV-080** An implementation MUST emit, with a shared `correlation_id` per
  logical operation and correct `sequence` (CMOS-05-EVT-020): `DeviceDetected`,
  `DeviceApproved`, `DeviceRejected`, `ProvisioningStarted`, `ProvisioningFinished`,
  `DeviceReplacementStarted`, `DeviceRetired`, `RegistrationSucceeded`,
  `RegistrationLost` — each on its state transition per §3.
- **CMOS-08-PROV-081** Provisioning events MUST NOT carry secrets, one-time tokens,
  signed URLs, resolved credentials, or raw config bytes (CMOS-05-EVT-041). They carry
  Device ids, `vendor_key`/`model`, outcome, and Object references only.
- **CMOS-08-PROV-082** Security-relevant provisioning anomalies (token replay, auth
  failure, unapproved-device registration attempt, revoked-credential use) MUST be
  surfaced as auditable, security-relevant records for Volume 9 to consume.

## 12. Capabilities used

This volume gates its actions on capabilities defined canonically in
[Volume 9 capabilities](../009-security/capabilities.md) and consistent with
[Volume 4 endpoints](../004-api/endpoints.md):

| Action | Capability |
|--------|-----------|
| View Devices / discovery inbox | `devices.view` |
| Approve / reject / provision / replace / retire / firmware | `provision.devices` |
| Manage per-tenant firmware target policy | `provision.devices` |

> Note: this volume introduces no new capability keys. Firmware policy is gated by
> `provision.devices`; if a future split is wanted, a `provision.firmware` key is
> reserved (Open items).

## 13. Conformance notes

- **L1 (Contract):** emitted Device/Provisioning/Registration events validate against
  their `contracts/json-schema/events/*` schemas and the envelope; the Device twin
  validates against the Device entity schema; no event payload carries secrets or URLs
  (CMOS-08-PROV-081).
- **L2 (Behavioural):** the harness drives DETECTED→…→OPERATIONAL and the replacement
  and retirement paths, asserting (a) illegal transitions are rejected
  (CMOS-08-PROV-001), (b) exactly the specified events fire in `sequence` order
  (CMOS-08-PROV-080), (c) a provisioning URL is single-use and short-lived
  (CMOS-08-PROV-050/051), (d) an unapproved or unprovisioned Device cannot register
  (CMOS-08-PROV-002/030), and (e) revocation/retirement invalidates outstanding tokens
  (CMOS-08-PROV-024/063).
- **L3 (Interoperable):** at least one real device per reference vendor is driven
  end-to-end through its `Provisioner` plugin to `OPERATIONAL`, and a
  cross-vendor replacement (vendor A dead → vendor B replacement) succeeds without
  operator template editing (CMOS-08-PROV-062).
- A `Provisioner` plugin passes the contract conformance suite in
  [`provisioner-contract.md`](provisioner-contract.md) §Conformance.

## 14. Open items

- Bulk approval / auto-approval policies (e.g. "auto-approve known-good models on the
  provisioning VLAN, still one-time-token secured") — pending a Volume 9 policy shape;
  MUST remain compatible with CMOS-08-PROV-022.
- Reserved capability `provision.firmware` for separating firmware authority from
  device provisioning — not yet introduced.
- Redirection/RPS-style vendor redirection services (Yealink RPS, Grandstream/Cisco
  redirection) as a discovery/onboarding source — behavioural profile deferred.
- Site/subnet → tenant attribution model for discovery (CMOS-08-PROV-012) — shape to
  be co-specified with Volume 3 (architecture) and Volume 14 (deployment).
- SCIM/bulk device import surface — cross-reference Volume 9 open items.

## Change log

- **0.3.0** — Initial implementation-grade draft: Device lifecycle behaviour, network
  discovery, operator approval workflow, zero-trust secure onboarding, one-click
  replacement, firmware management, the `Provisioner` vendor abstraction, and the
  emitted event set; requirement IDs `CMOS-08-PROV-001…082` assigned. Companion
  `provisioner-contract.md` added.
