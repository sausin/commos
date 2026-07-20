# Volume 9 — Security

**Status:** DRAFT · **Version:** 0.3.0 · **Subsystem tag:** SEC

Security in CommOS is **identity beyond passwords** and **authorization beyond roles**.
A person is a `User`; a proof that this person is present *now, here, this strongly* is
an `Identity`; what they may do is a set of `Capability` grants evaluated by a
declarative `Policy` engine. This volume freezes the authentication methods and
assurance levels, the capability-based authorization model, the policy engine's
evaluation semantics (including the Emergency Override), multi-tenant isolation,
secrets management, PKI, the append-only audit log, and transport security.

Companion documents:
- [`capabilities.md`](capabilities.md) — the canonical capability catalogue used
  across the API (Volume 4).

Related contracts: Identity/User/Policy/Capability/AuditEntry entities
([Volume 2 entities](../002-domain-model/entities.md)), Identity state machine
([Volume 2 state-machines](../002-domain-model/state-machines.md#identity)),
Identity/Auth events ([Volume 5 catalog](../005-events/catalog.md#identity--user)),
API auth model ([Volume 4 README §3](../004-api/README.md#3-authentication--authorization-normative)),
provisioning PKI/onboarding ([Volume 8 §7](../008-provisioning/README.md#7-secure-onboarding-zero-trust-normative)).

---

## 1. Scope

In scope: how a `User` proves an `Identity` and how strong that proof is; how requests
are authorized by capabilities and policies; how the policy engine decides
allow/deny/require for calls and API actions; tenant isolation as a construction
property; secrets and PKI management; the audit log; and transport security.

Out of scope (owned elsewhere): the API transport/error/versioning conventions
(Volume 4), the event envelope and delivery (Volume 5), the Device onboarding *flow*
(Volume 8 — this volume owns the PKI and secrets it relies on), plugin sandboxing
(Volume 12), and observability pipelines (Volume 15 — this volume defines what MUST be
audited; Volume 15 defines how logs/metrics are shipped).

## 2. Design principles (traceability)

| Principle | Invariant | How this volume honours it |
|-----------|-----------|----------------------------|
| Identity ≠ User ≠ Device ≠ Extension | CMOS-00-ENG-002 | Authentication produces an `Identity`; authorization binds capabilities to `User`; policy may `REQUIRE_IDENTITY` per action. |
| Capabilities, not roles | CMOS-00-ENG-009 | Authorization is fine-grained capability grants; roles are UI-only bundles ([`capabilities.md`](capabilities.md)). |
| Zero-trust | CMOS-00-ENG-010 | Every request authenticated + tenant-scoped; mTLS/PKI for provisioning & admin planes; no trust from network location. |
| Multi-tenant isolation by construction | CMOS-00-ENG-008 | Tenant scope derives from the credential; cross-tenant reference is impossible, not merely denied. |
| Immutable history | CMOS-00-ENG-012 | Append-only audit log; secrets/PKI actions are audited; nothing of record is hard-deleted. |
| No secrets in config | CMOS-00-ENG-005, -012 | Secrets live in a secrets backend and are referenced by handle; never in YAML/config/events. |
| Simpler to operate | CMOS-00-ENG-001 | One capability catalogue, one policy model, one audit log — no per-feature ACL dialects. |

## 3. Identity & authentication (normative)

An `Identity` is a verified authentication assertion binding a `User` to a session on
a `Device` (or an API/SSO context). It is **not** the `User` (CMOS-00-ENG-002,
CMOS-02-DOM-004) and follows the Volume 2 Identity state machine
`REQUESTED → AUTHENTICATED → ACTIVE → EXPIRED|REVOKED`.

- **CMOS-09-SEC-001** Authentication MUST produce an `Identity` entity distinct from
  the `User`; authorization decisions that require presence MUST reference an
  `Identity`, never merely a `User` id (CMOS-00-ENG-002, CMOS-02-DOM-004).
- **CMOS-09-SEC-002** The platform MUST support these authentication `method`s (closed
  enum, aligned with the Identity entity, extensible only by MINOR):
  `PIN`, `RFID`, `NFC`, `QR`, `BLUETOOTH`, `FIDO2`, `PROXIMITY`, `FACE`, `SSO`,
  `OIDC`, `SAML`, `LDAP`, `AD`. Federated methods (`SSO`/`OIDC`/`SAML`/`LDAP`/`AD`)
  MUST map the external subject to exactly one `User` within one tenant.
- **CMOS-09-SEC-003** Every `Identity` MUST carry an `assurance_level` of
  `LOW | MEDIUM | HIGH`. Assurance MUST be derived from the method and its
  verification strength, MUST be recorded on the `Identity`, and MUST drive policy
  (§5). An implementation MUST NOT let a lower-assurance method satisfy a policy that
  requires a higher one.
- **CMOS-09-SEC-004** Assurance mapping (normative floor; an implementation MAY assign
  *higher* where locally justified, MUST NOT assign lower):
  `LOW` — single unverified factor (`PIN` alone, `QR` display);
  `MEDIUM` — a possessed token or verified federation (`RFID`/`NFC`/`BLUETOOTH`/
  `PROXIMITY`/`SSO`/`OIDC`/`SAML`/`LDAP`/`AD`);
  `HIGH` — phishing-resistant or biometric (`FIDO2`, verified `FACE`, or any method
  combined into MFA). `FIDO2` MUST be treated as phishing-resistant.
- **CMOS-09-SEC-005** Authentication attempts MUST emit the Identity events
  (`AuthenticationRequested`, `AuthenticationSucceeded`, `AuthenticationFailed`,
  `IdentityAuthenticated`, `IdentityExpired`, `IdentityRevoked`) per the Volume 2
  state machine, and MUST NOT place secrets, raw biometrics, PINs, or tokens in event
  payloads (CMOS-05-EVT-041). `AuthenticationFailed` MUST NOT reveal which factor
  failed in a way that aids enumeration.
- **CMOS-09-SEC-006** `Identity` is time-bounded: an `ACTIVE` Identity MUST carry an
  `expires_at`, MUST transition to `EXPIRED` at expiry, and MUST be revocable
  (`REVOKED`) immediately by an authorised actor or by session/User deactivation.
  Revocation MUST take effect for subsequent authorization within the tenant's
  read-your-writes bound (CMOS-04-API-051).
- **CMOS-09-SEC-007** Biometric and possession factors (`FACE`, `FIDO2`, `NFC`,
  `RFID`) MUST be verified against material held in the secrets/PKI subsystem or an
  external IdP; the platform MUST NOT store raw biometric templates in structured
  state or config (CMOS-00-ENG-005). Biometric material, where retained at all, is an
  `Object` under retention policy, referenced by handle.

## 4. Capability-based authorization (normative)

Authorization is **capability-based, not role-based** (CMOS-00-ENG-009). The canonical
catalogue is [`capabilities.md`](capabilities.md); its keys are the same ones the API
endpoints declare (Volume 4).

- **CMOS-09-SEC-010** A `Capability` is a fine-grained, grantable permission named by a
  dotted `key` matching `^[a-z][a-z0-9]*(\.[a-z][a-z0-9]*)+$` (e.g. `provision.devices`,
  `billing.export`, `calls.control`). The catalogue is closed within a MAJOR line;
  adding a key is MINOR (CMOS-CONV-002).
- **CMOS-09-SEC-011** Every API action MUST require exactly the capability the endpoint
  declares (Volume 4 [endpoints.md](../004-api/endpoints.md)); a missing capability
  MUST yield `403` with `code=forbidden` (CMOS-04-API-021). Capabilities are granted to
  `User`s (`CapabilityGranted`/`CapabilityRevoked` events) and evaluated against the
  authenticated principal's tenant.
- **CMOS-09-SEC-012** **Roles do not exist as an authorization primitive.** A "role" is
  only a named bundle of capability keys presented in the UI; the enforcement path MUST
  evaluate capabilities, never a role name (CMOS-00-ENG-009). No policy or endpoint may
  branch on a role.
- **CMOS-09-SEC-013** Capability grants are subject to policy: holding `calls.originate`
  authorizes the *action*, but a `Policy` MAY still `REQUIRE_IDENTITY` or `DENY` a
  specific call (§5). Capability and Policy are ANDed: an action proceeds only if the
  capability is held **and** policy evaluation permits it.

### 4.1 Policy composition (normative)

A `Policy` has an `effect` in `ALLOW | DENY | REQUIRE_IDENTITY | REQUIRE_APPROVAL`
(CMOS-02-DOM entities), `conditions`, and a `priority`.

- **CMOS-09-SEC-014** Policy effects MUST compose deterministically:
  `DENY` overrides `ALLOW`; `REQUIRE_IDENTITY` and `REQUIRE_APPROVAL` are **obligations
  layered onto an ALLOW** (they permit only if satisfied); absence of any matching
  ALLOW is an implicit deny (default-deny). Given equal `priority`, `DENY` wins.
- **CMOS-09-SEC-015** Evaluation MUST be **total and deterministic**: for any request
  the engine MUST reach exactly one decision in `PERMIT | DENY | CHALLENGE`
  (CHALLENGE = an obligation such as step-up identity or approval is outstanding).
  Evaluation order is by descending `priority`, then the override rule of
  CMOS-09-SEC-014; it MUST NOT depend on rule insertion order or wall-clock.
- **CMOS-09-SEC-016** `REQUIRE_IDENTITY` is satisfied only by an `ACTIVE` `Identity`
  meeting the condition's minimum `assurance_level` (§3); `REQUIRE_APPROVAL` is
  satisfied only by a recorded approval from an actor holding the required capability.
  An unsatisfied obligation yields `CHALLENGE`, never a silent `PERMIT`.

## 5. Policy engine semantics — the canonical rules (normative)

These are the concrete rules the engine MUST express with the composition model above.
They are the security half of routing; the routing half is Volume 7.

- **CMOS-09-SEC-020** **External calls require an Identity.** An outbound call to an
  external DID/E.164 destination MUST evaluate `REQUIRE_IDENTITY`: it proceeds only
  with an `ACTIVE` `Identity` of at least the tenant-configured minimum assurance
  (default `MEDIUM`). Absence of a resolvable Identity on a chargeable external leg is
  a **policy failure**, not a silent default — the attempt yields `CallRejected` with a
  policy hangup cause (CMOS-02-DOM-010, CMOS-00-ENG-011).
- **CMOS-09-SEC-021** **Internal calls do not require an Identity.** A call whose
  destination resolves within the tenant (Extension/User/Queue) MUST NOT require an
  Identity by default (it is not externally chargeable), though a tenant Policy MAY
  raise the bar.
- **CMOS-09-SEC-022** **International (and other elevated classes) require manager
  approval / capability.** A destination classified as international (or other
  high-cost/high-risk class per tenant policy) MUST additionally evaluate a rule
  requiring an authorised approver — expressed as `REQUIRE_APPROVAL` or a capability
  such as `calls.dial.international` held by the principal or a delegating manager. The
  classification is a policy condition, not a hard-coded dial-prefix table exposed to
  admins (CMOS-00-ENG-005).
- **CMOS-09-SEC-023** **Emergency Override bypasses everything.** A call classified as
  an emergency call MUST be permitted regardless of Identity, assurance, capability,
  approval, balance, or suspension state — `EMERGENCY BYPASSES EVERYTHING`
  (Emergency Override, glossary). The engine MUST short-circuit to `PERMIT` for an
  emergency-classified call before evaluating any `DENY`/`REQUIRE_*` rule, MUST still
  emit the full attributable event trail, and MUST audit the override
  (CMOS-09-SEC-040). An implementation MUST NOT allow any policy, capability check,
  billing state, or tenant configuration to block an emergency call.
- **CMOS-09-SEC-024** Emergency classification MUST be evaluated conservatively and
  MUST be tenant/region aware; a false negative (failing to recognise an emergency
  number) is a critical defect. Where ambiguous, the engine MUST prefer treating a call
  as emergency (fail-open for emergency only). This is the single deliberate exception
  to default-deny and MUST be scoped strictly to emergency classification.

> Note (informative): the Emergency Override is the one place where CommOS chooses
> availability over control, on purpose. Everywhere else the model is default-deny and
> least-privilege. Emergency is fail-open; nothing else is.

## 6. Multi-tenant isolation (normative)

- **CMOS-09-SEC-030** Tenant scope MUST derive from the authenticated credential; a
  request MUST NOT be able to name, read, or affect another tenant's resources
  (CMOS-00-ENG-008, CMOS-04-API-022). Isolation MUST hold by construction (scoped
  identifiers, scoped queries), not by discretionary policy that could be misconfigured.
- **CMOS-09-SEC-031** There is no cross-tenant identifier reuse (CMOS-CONV-015); a
  capability grant, Identity, Policy, secret handle, or certificate is meaningful only
  within its tenant. A global/platform-operator scope, if it exists, MUST be a distinct,
  separately-audited principal class and MUST NOT be reachable via ordinary tenant
  credentials.
- **CMOS-09-SEC-032** Event subscriptions, audit reads, secret reads, and object reads
  MUST all be tenant-gated identically to the API (CMOS-05-EVT-040); no side channel
  (metrics, logs, error detail, timing) may leak another tenant's data
  (CMOS-04-API-042 keeps correlation ids tenant-safe).

## 7. Defence in depth (normative)

- **CMOS-09-SEC-035** Security MUST NOT rely on a single control. Authentication
  (§3), capability authorization (§4), policy evaluation (§5), tenant isolation (§6),
  transport security (§11), secrets isolation (§8), and audit (§10) are independent
  layers; the failure or misconfiguration of one MUST NOT collapse the others. In
  particular, network-level reachability MUST NEVER substitute for authentication
  (CMOS-00-ENG-010).
- **CMOS-09-SEC-036** Every plane (public API, provisioning, admin, event stream) MUST
  authenticate and authorize independently; the admin and provisioning planes MAY
  additionally require mTLS (CMOS-04-API-020, Volume 8 §7). Internal service-to-service
  calls MUST be mutually authenticated, not trusted by co-location (CMOS-00-ENG-006).

## 8. Secrets management (normative)

- **CMOS-09-SEC-050** Secrets (SIP/registration credentials, API keys, webhook signing
  keys, IdP client secrets, private keys, DB/object-store credentials) MUST NEVER be
  stored in YAML, config files, environment-baked images, entity fields, or event
  payloads (CMOS-00-ENG-005, CMOS-05-EVT-041). Structured state and config hold only
  **references/handles** to secrets.
- **CMOS-09-SEC-051** The platform MUST integrate a pluggable secrets backend and MUST
  NOT depend on a specific one. Supported backends: HashiCorp Vault, AWS KMS/Secrets
  Manager, GCP KMS/Secret Manager, 1Password, and — for the single-artifact default
  deployment — process environment injection at start (CMOS-00-ENG-007,
  CMOS-00-ENG-014). The backend is selected by deployment configuration; the resolution
  interface is uniform (`secret_ref → material`).
- **CMOS-09-SEC-052** Secret material MUST be resolved **as late as possible** (at
  delivery/use), held in memory only for as long as needed, and MUST NOT be written to
  logs, traces, or provisioning artifacts in the clear (CMOS-08-PROV-130/131). Secret
  handles MAY appear in config and audit; resolved values MUST NOT.
- **CMOS-09-SEC-053** Secrets MUST be **rotatable without redeployment**: rotating a
  secret updates the backing material behind a stable handle; consumers re-resolve on
  next use. Rotation and access to secret material MUST be audited (handle, actor,
  time — never the value) in the append-only audit log (CMOS-00-ENG-012).
- **CMOS-09-SEC-054** Managing secret backends/handles MUST require the `secrets.manage`
  capability; reading resolved secret *values* MUST NOT be an API-exposed capability at
  all — the platform resolves secrets internally for its own use, and there is no
  endpoint that returns plaintext secret material.

## 9. Certificate & PKI management (normative)

- **CMOS-09-SEC-060** The platform MUST operate (or integrate) a PKI that issues
  certificates for: provisioning/server TLS, per-Device client certificates for mTLS
  (Volume 8 §7), and internal service mTLS (§7). Issuance, renewal, and revocation MUST
  be programmatic; there MUST be no manual certificate copying into config
  (CMOS-00-ENG-005).
- **CMOS-09-SEC-061** Private keys MUST be treated as secrets (§8): stored in the
  secrets/KMS backend, referenced by handle, never emitted in events, logs, or
  provisioning artifacts (CMOS-09-SEC-050/052).
- **CMOS-09-SEC-062** Certificates MUST be **revocable and short-lived where practical**;
  the platform MUST support revocation (CRL/OCSP or backend-native) and MUST honour
  certificate pinning where a Device vendor supports it (CMOS-08-PROV-053). Revoking a
  Device (retirement/replacement, Volume 8) MUST revoke its client certificate
  (CMOS-08-PROV-024/063).
- **CMOS-09-SEC-063** Managing the PKI (issuing/revoking certificates, trust anchors)
  MUST require the `certs.manage` capability and MUST be audited (CMOS-00-ENG-012).

## 10. Audit log (normative)

- **CMOS-09-SEC-040** Every security-relevant action MUST produce an **append-only**
  `AuditEntry`: authentication (success/failure), capability grant/revoke, policy
  change, secret/PKI access and rotation, device approval/rejection/provisioning/
  replacement/retirement, emergency overrides, and every `:verb` mutation (Volume 4).
  An `AuditEntry` MUST NOT be modified or hard-deleted (CMOS-00-ENG-012,
  CMOS-02-DOM-003).
- **CMOS-09-SEC-041** An `AuditEntry` MUST record at minimum `actor_ref` (the acting
  Identity/principal), `action`, `target_ref`, `tenant_id`, `at` (RFC 3339 UTC millis),
  `correlation_id`, and before/after references (not inlined secrets). Audit records
  MUST NOT contain secret values, raw biometrics, PINs, or tokens (CMOS-05-EVT-041).
- **CMOS-09-SEC-042** The audit log MUST be **tamper-evident**: entries SHOULD be
  chained (each entry binding a hash of the prior) or otherwise integrity-protected so
  that removal or alteration is detectable. Reading the audit log requires the
  `audit.view` capability; there is no capability that deletes or edits it.
- **CMOS-09-SEC-043** The emission of an audit entry MAY be surfaced as an
  `AuditEntryRecorded` event for downstream SIEM consumers (CMOS-00-ENG-004); the
  authoritative record is the append-only store, and event delivery loss MUST NOT lose
  the audit record (transactional outbox, CMOS-05-EVT-010).

## 11. Transport security (normative)

- **CMOS-09-SEC-070** All control-plane transport MUST use **TLS 1.3 or higher**;
  earlier TLS versions and plaintext HTTP MUST NOT be offered for API, provisioning,
  admin, or event transports (CMOS-04-API-001, CMOS-08-PROV-052).
- **CMOS-09-SEC-071** Real-time media MUST support and default to **SRTP** with
  encrypted keying (DTLS-SRTP for WebRTC; SDES/DTLS-SRTP for SIP as negotiated).
  Unencrypted RTP MAY be permitted only where a Gateway/Carrier cannot do otherwise and
  MUST be an explicit, audited, per-trunk exception — never the default
  (CMOS-00-ENG-016 SIP-is-one-transport; media plane, CMOS-00-ENG-006).
- **CMOS-09-SEC-072** Certificate validation MUST be enforced on every TLS/mTLS peer
  (no disabled verification, no accept-any-cert mode in production); the provisioning
  and internal planes MUST use mutual authentication (§7, Volume 8 §7). Cipher and
  version policy MUST be centrally configured, not per-endpoint.

## 12. Capabilities used and introduced

This volume authorizes its own actions with capabilities from
[`capabilities.md`](capabilities.md). It **reuses** existing keys already declared by
the API — notably `iam.manage` (grant/revoke capabilities), `policy.manage` (manage
policies), `audit.view` (read the audit log), and `provision.devices` (device
security actions).

It **introduces** the following capability keys, which the API and other volumes
SHOULD adopt:

| New key | Gates |
|---------|-------|
| `secrets.manage` | Managing secret backends and secret references/handles (§8). No key ever returns plaintext secret values. |
| `certs.manage` | PKI: issuing/renewing/revoking certificates and managing trust anchors (§9). |
| `calls.dial.international` | Placing calls to the international (elevated-cost) class where policy gates it by capability rather than per-call approval (§5, CMOS-09-SEC-022). |

## 13. Conformance notes

- **L1 (Contract):** Identity/Auth/Audit events validate against their schemas and the
  envelope; no auth/audit/provisioning payload carries secrets, PINs, tokens, or raw
  biometrics (CMOS-09-SEC-005/041); error bodies remain valid Problem Details.
- **L2 (Behavioural):** the harness asserts (a) `Identity ≠ User` in the authorization
  path (CMOS-09-SEC-001); (b) assurance floors are enforced (CMOS-09-SEC-003/004); (c)
  capability-then-policy AND semantics and deterministic composition
  (CMOS-09-SEC-013/014/015); (d) the canonical policy rules — external requires
  identity, internal does not, international requires approval/capability, and
  **emergency bypasses everything** (CMOS-09-SEC-020..024); (e) cross-tenant access is
  impossible (CMOS-09-SEC-030); (f) audit entries are append-only and tamper-evident
  (CMOS-09-SEC-040/042); (g) secrets never appear in config/logs/events
  (CMOS-09-SEC-050/052).
- **L3 (Interoperable):** federated auth (`OIDC`/`SAML`/`LDAP`/`AD`) against real IdPs;
  `FIDO2` phishing-resistant flows; mTLS provisioning against real devices (Volume 8
  L3); TLS 1.3 + SRTP interop with real endpoints and carriers (CMOS-09-SEC-070/071).

## 14. Open items

- Precise assurance-scoring model for MFA combinations and step-up flows (currently a
  normative floor in CMOS-09-SEC-004) — candidate for a companion `assurance.md`.
- SCIM/bulk user & capability provisioning surface (cross-ref Volume 4 open items).
- Delegation/impersonation model for support principals (a `security.impersonate`-class
  capability) — reserved, not introduced, pending an ADR (Volume 19).
- Audit-chain algorithm and export format for SIEM (CMOS-09-SEC-042/043) — to be
  co-specified with Volume 15.
- Formal machine-readable capability catalogue under `contracts/` — the prose
  [`capabilities.md`](capabilities.md) is the current authority until then.

## Change log

- **0.3.0** — Initial implementation-grade draft: authentication methods & assurance
  levels, capability-based authorization and policy composition, the canonical policy
  rules including the Emergency Override, multi-tenant isolation, defence in depth,
  secrets management, PKI, append-only tamper-evident audit, and transport security;
  requirement IDs `CMOS-09-SEC-001…072` assigned. Companion `capabilities.md` added;
  introduced capability keys `secrets.manage`, `certs.manage`,
  `calls.dial.international`.
