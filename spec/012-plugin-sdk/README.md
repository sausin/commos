# Volume 12 — Plugin SDK

**Status:** DRAFT · **Version:** 0.3.0 · **Subsystem tag:** PLUG

CommOS is extended by **WASM plugins** run in a Wasmtime-class sandbox. A plugin is
untrusted, capability-scoped code that the **host** loads to implement a well-defined
extension contract — a device Provisioner, a CRM connector, a billing charge source,
a webhook transformer, an authentication method. The cardinal guarantee is
containment: **a plugin can fail, but it can never crash, corrupt, or escalate
privilege in the host** (CMOS-03-ARCH-051). This volume freezes the plugin lifecycle,
the host↔plugin capability/resource contract, the scoped event & API surface a plugin
receives, sandbox guarantees, versioning/compatibility, and the distribution model.

Entity: [`Plugin`](../002-domain-model/entities.md#webhook--automation--aijob--plugin--auditentry).
Events: `PluginInstalled`, `PluginFailed`
([catalog](../005-events/catalog.md#billing-webhook-automation-ai-plugin-audit)).
Provisioner contract referenced: Volume 8 & Glossary (**Provisioner**).

> Note (informative): the plugin model is how CommOS supports "every vendor" (phones,
> CRMs, carriers) without absorbing every vendor's code into the core. A `yealink`
> Provisioner and a `salesforce` CRM connector are plugins; the substrate ships
> neither and depends on neither.

---

## 1. Principles (normative)

- **CMOS-12-PLUG-001** A plugin MUST execute as a **WebAssembly module** in an
  isolated sandbox (Wasmtime-class). It MUST have **no ambient authority**: no direct
  filesystem, network, clock, randomness, or host-memory access except through
  explicitly granted, capability-gated host functions. (Serves CMOS-03-ARCH-051,
  CMOS-00-ENG-010.)
- **CMOS-12-PLUG-002** A plugin **MUST NOT be able to crash, hang, or corrupt the
  host**. A trap, panic, timeout, resource-limit breach, or memory fault MUST be
  contained to the plugin instance and surface as a `PluginFailed` event; the host
  continues serving all other work. (Serves CMOS-03-ARCH-051, CMOS-00-ENG-001.)
- **CMOS-12-PLUG-003** All authority a plugin holds MUST be an explicit **Capability**
  grant (CMOS-00-ENG-009); a plugin acts strictly within its granted capabilities and
  its declared tenant scope. There is no privileged plugin.
- **CMOS-12-PLUG-004** A plugin's contract with the host MUST be **versioned** and
  governed by SemVer (CONVENTIONS §4). The host↔plugin ABI and each extension-point
  interface carry a `MAJOR.MINOR.PATCH` version and honour the compatibility rules of
  §6.
- **CMOS-12-PLUG-005** Plugins interact with the domain only through the **same
  public contracts** as any other client — Events (Volume 5) and the API (Volume 4),
  narrowed by capability (CMOS-00-ENG-003, CMOS-00-ENG-004). A plugin has no private
  back door into the substrate.

## 2. Plugin lifecycle (normative)

```
(registry) ──install──▶ INSTALLED ──enable──▶ ENABLED ⇄ disable──▶ DISABLED
                            │                     │                     │
                            │                  upgrade                uninstall
                            ▼                     ▼                     ▼
                         FAILED  ◀───trap/limit── (new version)     UNINSTALLED
```

| State | Meaning | Event |
|-------|---------|-------|
| `INSTALLED` | Module + manifest stored and verified; not yet running. | `PluginInstalled` |
| `ENABLED` | Instantiated and receiving scoped events/calls. | `PluginEnabled` (planned) |
| `DISABLED` | Retained but inert; no events delivered. | `PluginDisabled` (planned) |
| `FAILED` | Contained fault; quarantined pending operator action. | `PluginFailed` |
| `UNINSTALLED` | Removed; a state transition, not a hard delete. | `PluginUninstalled` (planned) |

- **CMOS-12-PLUG-010** **Install** MUST verify the module's **signature and integrity**
  (`sha256`) against a trusted publisher key before storage, and MUST record the
  manifest (declared capabilities, resource limits, extension points, ABI version).
  Verification failure MUST reject the install. (Serves CMOS-00-ENG-010.)
- **CMOS-12-PLUG-011** **Enable** MUST instantiate the module under its declared limits
  and MUST fail closed if the host cannot satisfy a requested capability or limit — a
  plugin is never enabled with *more* authority than granted, and never *silently
  less*.
- **CMOS-12-PLUG-012** **Disable** MUST stop event/API delivery to the plugin and
  release its resources without affecting other plugins or the host. In-flight calls
  into the plugin are cancelled within the plugin's time budget (§3).
- **CMOS-12-PLUG-013** **Upgrade** MUST install the new version alongside the old,
  validate ABI/interface compatibility (§6), and cut over atomically; a failed cutover
  MUST roll back to the prior version (CMOS-00-ENG-012). Config/state migration is the
  plugin's responsibility across a MAJOR bump.
- **CMOS-12-PLUG-014** **Uninstall** MUST be a state transition to `UNINSTALLED`
  (CMOS-02-DOM-003); the plugin record and its audit history remain resolvable.
- **CMOS-12-PLUG-015** Every lifecycle transition MUST emit its named Plugin event and
  record an `AuditEntry` (CMOS-00-ENG-012). `PluginFailed` MUST carry a structured
  fault (`trap\|timeout\|oom\|limit\|panic`) and the offending capability/limit.

## 3. Capability grants & resource limits (normative)

The host↔plugin boundary is a **deny-by-default** contract. A plugin declares what it
needs in its manifest; an operator grants a subset; the host enforces the grant.

- **CMOS-12-PLUG-020** A plugin MUST declare, and the host MUST enforce, resource
  limits: **CPU/fuel** (bounded execution units per invocation), **memory** (max
  linear-memory pages), **wall-clock time** (per-invocation deadline), and **call
  concurrency**. Exceeding any limit MUST trap the invocation and MUST NOT degrade the
  host (CMOS-12-PLUG-002).
- **CMOS-12-PLUG-021** The **syscall/host-function surface** exposed to a plugin MUST
  be an explicit allow-list of capability-gated host functions (e.g. `http.fetch`
  under `net.egress`, `kv.get/put` under `state.kv`, `log.emit`, `events.subscribe`,
  `api.call`). No WASI capability granting ambient filesystem, socket, environment, or
  clock access MAY be enabled implicitly. (Serves CMOS-00-ENG-010.)
- **CMOS-12-PLUG-022** Host-function calls MUST be **tenant-scoped**: a plugin instance
  acts within one `tenant_id` (or is instantiated per-tenant), and the host MUST
  refuse any host-function argument that references another tenant's entity/Object
  (CMOS-00-ENG-008, CMOS-CONV-015).
- **CMOS-12-PLUG-023** Outbound network egress (`net.egress`) MUST be restricted to a
  declared allow-list of destinations in the manifest; the host MUST reject egress to
  an undeclared host and MUST record it (CMOS-00-ENG-010).
- **CMOS-12-PLUG-024** Persistent plugin state MUST be provided only through a
  host-mediated, tenant-scoped key-value/Object interface; a plugin MUST NOT assume
  local disk or a shared global namespace.
- **CMOS-12-PLUG-025** Resource limits and capability grants are **declarative
  configuration** (CMOS-00-ENG-005) and versioned Digital Twins on the `Plugin`
  entity; changing a grant is an audited transition.

## 4. Event & API access (normative)

- **CMOS-12-PLUG-030** A plugin MAY subscribe only to Event types permitted by its
  grants and only within its tenant scope (CMOS-05-EVT-040); delivery follows the
  bus's at-least-once + idempotency contract (CMOS-05-EVT-010/011). A plugin MUST be
  idempotent on (`type`, `idempotency_key`).
- **CMOS-12-PLUG-031** A plugin consuming Events MUST treat them as **facts, not
  commands** (CMOS-05-EVT-003); to change state it MUST issue a Command through the
  granted API surface, subject to the same authorization as any client
  (CMOS-00-ENG-003).
- **CMOS-12-PLUG-032** Event/API payloads delivered to a plugin MUST carry Object
  **references**, never raw media/blobs (CMOS-02-DOM-013, CMOS-05-EVT-041); Object
  bytes are fetched via a capability-gated, short-lived resolution.
- **CMOS-12-PLUG-033** A plugin's API calls MUST be attributable in the audit log to
  the plugin instance as `actor_ref` (CMOS-00-ENG-012), distinct from the human/user
  actor, so plugin actions are always traceable.

## 5. Sandbox guarantees (normative)

- **CMOS-12-PLUG-040** **Isolation:** a plugin's linear memory and execution stack MUST
  be inaccessible to the host and to other plugins; the only data crossing the
  boundary is copied through typed host-function ABIs (CMOS-12-PLUG-002).
- **CMOS-12-PLUG-041** **Determinism/quiescence:** between invocations a plugin holds
  no ambient timers or background threads; it runs only when the host calls it. Any
  time/random source MUST be a host function (auditable, mockable).
- **CMOS-12-PLUG-042** **Fault containment:** a trap/panic MUST terminate only the
  current invocation; the host MUST be able to re-instantiate or quarantine
  (`FAILED`) the plugin without restart of the host process (CMOS-03-ARCH-051).
- **CMOS-12-PLUG-043** **No privilege escalation:** a plugin MUST NOT be able to grant
  itself capabilities, widen its tenant scope, read another plugin's state, or observe
  host secrets. Attempted escalation MUST fail closed and be audited.
- **CMOS-12-PLUG-044** **Bounded blast radius:** a misbehaving plugin MUST NOT be able
  to exhaust host resources beyond its declared limits (CPU/fuel, memory, time,
  concurrency, egress). The host enforces limits, not the plugin.

## 6. Versioning & compatibility (normative)

- **CMOS-12-PLUG-050** The **host ABI** and each **extension-point interface** are
  versioned `MAJOR.MINOR.PATCH` (CONVENTIONS §4). The host MUST refuse to enable a
  plugin whose required ABI MAJOR differs from the host's, and MUST accept a plugin
  built against an equal-or-lower MINOR of the same MAJOR (tolerant-reader,
  CMOS-CONV-004).
- **CMOS-12-PLUG-051** Adding a host function, an optional manifest field, or a new
  extension point is **MINOR**; removing/narrowing a host function or changing an ABI
  signature is **MAJOR** (CMOS-CONV-005). Interface type names are permanent within a
  MAJOR line (CMOS-CONV-003).
- **CMOS-12-PLUG-052** A plugin MUST declare its own SemVer and the ABI/interface
  versions it targets in its manifest; the registry MUST expose these for compatibility
  resolution before install.
- **CMOS-12-PLUG-053** A plugin upgrade that crosses the plugin's own MAJOR version MAY
  require a migration; the host MUST support running the prior version until cutover
  succeeds (CMOS-12-PLUG-013).

## 7. Plugin categories & extension contracts (normative)

Each category is a frozen **extension-point interface** a plugin implements. The
substrate calls the interface; the plugin never calls in except through granted host
functions.

| Category | Implements | Capability class | Notes |
|----------|-----------|------------------|-------|
| **provisioning** | the **Provisioner** contract (Volume 8): build config, reboot, firmware, factory reset | `provision.devices` | Vendor device support (`yealink`, `fanvil`, …) as plugins. |
| **crm** | contact/enrichment sync on Call/CDR events | `crm.sync` | Consumes `CallEnded`/`BillingGenerated`, calls out under `net.egress`. |
| **billing** | a **charge source** / rating adapter (Volume 10 §4) | `billing.rate` | Supplies rates or reconciliation for a Carrier. |
| **webhook** | event transform/relay | `webhook.transform` | Reshapes events before signed delivery (CMOS-05-EVT-014). |
| **authentication** | an Identity method (Volume 9) | `auth.method` | Adds a `method` for Identity assertion; result feeds Identity assurance. |

- **CMOS-12-PLUG-060** A **provisioning** plugin MUST implement the Provisioner
  contract exactly (Volume 8/Glossary) and MUST NOT expose vendor primitives to users
  (N-1, CMOS-00-ENG-005); it operates within `provision.devices` and zero-trust
  provisioning (CMOS-00-ENG-010).
- **CMOS-12-PLUG-061** A **billing** plugin MUST feed the rating/charge-source contract
  of Volume 10 and MUST NOT mutate CDRs directly (CDRs are append-only,
  CMOS-10-BILL-003); it supplies rates/adjustments that produce new CDRs.
- **CMOS-12-PLUG-062** An **authentication** plugin MUST return an assertion the
  platform maps to an `Identity` with an `assurance_level`; the plugin MUST NOT itself
  mint sessions or bypass Policy (CMOS-00-ENG-009).
- **CMOS-12-PLUG-063** A **crm**/**webhook** plugin's outbound calls MUST honour
  tenant data-residency/egress constraints and PII minimisation (CMOS-05-EVT-042),
  consistent with the AI surface rules (Volume 11 §6).

## 8. Distribution & marketplace (normative)

- **CMOS-12-PLUG-070** Plugins are distributed as **signed, versioned artifacts** with
  a manifest (identity, publisher, SemVer, targeted ABI/interfaces, requested
  capabilities, resource limits, egress allow-list). Install MUST verify signature and
  integrity (CMOS-12-PLUG-010).
- **CMOS-12-PLUG-071** A registry/marketplace MUST present each plugin's requested
  capabilities and limits **before** install so an operator grants informed consent;
  the operator MAY grant a **subset**, and the host enforces the granted subset
  (CMOS-12-PLUG-011).
- **CMOS-12-PLUG-072** A plugin MAY be installed **global** (available to many
  Organisations) or **tenant-scoped** (`Plugin` in Organisation), but at runtime every
  instance is tenant-scoped in its authority (CMOS-12-PLUG-022).
- **CMOS-12-PLUG-073** The marketplace MUST NOT be a privileged install path: a
  side-loaded, signed plugin installed via the API MUST be subject to identical
  verification, capability, and sandbox rules (CMOS-00-ENG-003).
- **CMOS-12-PLUG-074** Publisher key revocation MUST be supported; on revocation the
  host MUST quarantine (`DISABLED`/`FAILED`) affected plugins and audit the action.

## Conformance notes

- **L1 (Contract):** `PluginInstalled`/`PluginFailed` envelopes and `data` validate
  against the frozen schemas; the `Plugin` entity (manifest, `capabilities[]`,
  `resource_limits`, `state`) validates against its schema; ABI/interface versions are
  declared.
- **L2 (Behavioural):** driven scenarios — a plugin that traps is contained and
  transitions to `FAILED` while the host keeps serving (CMOS-12-PLUG-002/042); a
  plugin exceeding its fuel/memory/time limit is trapped, not throttled into the host
  (CMOS-12-PLUG-020); an egress to an undeclared host is refused (CMOS-12-PLUG-023);
  an attempt to touch another tenant's Object is refused (CMOS-12-PLUG-022); enabling a
  plugin with an incompatible ABI MAJOR is refused (CMOS-12-PLUG-050).
- **L3 (Interoperable):** a provisioning plugin drives a real vendor Device through the
  Provisioner contract; a CRM plugin round-trips `CallEnded` to an external system
  under egress limits.
- The harness (`conformance/run.py`) checks Plugin event catalog↔schema↔example
  consistency and manifest schema validity.

## Open items
- Machine-readable **manifest** schema and the frozen **host ABI**/extension-point
  interfaces (WIT-style) — `contracts/` additions required before freeze.
- Non-core Plugin lifecycle events (`PluginEnabled`/`PluginDisabled`/`PluginUninstalled`)
  are `planned` in the catalog — to be added as schemas.
- Inter-plugin composition (one plugin invoking another) and a capability-delegation
  model — reserved for v0.4.
- Fuel-to-wall-clock accounting profile and per-category default limits — reserved.

## Change log
- **0.3.0** — Initial implementation-grade draft: WASM sandbox principles, install/
  enable/disable/upgrade/uninstall lifecycle, deny-by-default capability & resource
  contract, scoped event/API surface, sandbox guarantees, SemVer ABI/interface
  compatibility, the five plugin categories (provisioning/CRM/billing/webhook/auth),
  and a signed marketplace/distribution model.
