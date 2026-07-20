# Volume 1 — Product Requirements

**Status:** DRAFT · **Version:** 0.3.0 · **Subsystem tag:** PRD

This volume states *what* the product does and *for whom*, as personas, epics, and
measurable acceptance criteria. It is subordinate to Volume 0 (Philosophy): every
requirement here serves an invariant there. The feature catalogue is in
[`features.md`](features.md); UX detail is Volume 13.

Requirements use RFC-2119 language and IDs `CMOS-01-PRD-NNN`. Each acceptance
criterion is written to be executable as a conformance scenario (Volume 16).

---

## 1. Personas

| Persona | Cares about | Primary jobs-to-be-done |
|---------|-------------|-------------------------|
| **SMB owner/admin** (10–50 people, no telecom expertise) | It just works; no training | Add people & phones, set hours, a simple menu, see who called |
| **IT administrator** (mid-market) | Control without XML | Provisioning at scale, SSO, call flows, policy, upgrades |
| **MSP operator** | Many tenants, one pane | Onboard orgs, template configs, bill customers, monitor fleets |
| **Enterprise admin** | Scale, compliance, audit | Departments, cost centres, capabilities, DR, SCIM |
| **Contact-centre supervisor** | Live control & metrics | Queues, agents, SLAs, barge/whisper, wallboards |
| **Finance** | Attributable spend | Per-user/department/cost-centre cost, exports, quotas |
| **Mobile / remote worker** | Presence & mobility | Same identity across devices, softphone, hot-desking |
| **AI system / integrator** | Clean events & data | Subscribe to events, fetch recordings/transcripts, act |
| **CRM/ERP integration** | Sync & trigger | React to calls, enrich, log, click-to-dial |
| **SIP carrier / mobile gateway** | Reliable trunking | Register/route as just-another-trunk |
| **IP phone / device** | Zero-touch onboarding | Boot, be discovered, be approved, provision securely |

## 2. Product goals (normative)

- **CMOS-01-PRD-001** A non-technical SMB admin MUST be able to go from a fresh
  install to a working inbound call (number → menu → ring a person) **without editing
  any XML, Lua, or SIP profile** and without reading a manual (serves
  CMOS-00-ENG-005, CMOS-00-ENG-001).
- **CMOS-01-PRD-002** Every product capability MUST be reachable through the public
  API; the shipped UI MUST NOT use any private endpoint (serves CMOS-00-ENG-003).
- **CMOS-01-PRD-003** The product MUST operate multi-tenant on a single deployment
  with true isolation between Organisations (serves CMOS-00-ENG-008).
- **CMOS-01-PRD-004** Every chargeable call MUST be attributable to Device, User
  (via Identity), and Organisation (serves CMOS-00-ENG-011).
- **CMOS-01-PRD-005** The default deployment MUST be a single binary plus PostgreSQL,
  installable and started in minutes (serves CMOS-00-ENG-014; perf in Volume 17).

## 3. Epics and acceptance criteria

Each epic lists representative user stories and **measurable** acceptance criteria
(AC). ACs reference the entities/events/endpoints they exercise.

### E1 — People & Organisation
*As an admin, I add people and organise them so calls reach the right person.*
- **AC-E1.1** Creating a User via `POST /v1/users` returns `201` in < 100 ms (p95,
  Volume 17) and emits `UserCreated`. `CMOS-01-PRD-010`
- **AC-E1.2** Users can be grouped into Departments and Cost Centres; a User's
  attribution chain is resolvable for billing. `CMOS-01-PRD-011`

### E2 — Devices & zero-touch provisioning
*As an IT admin, I approve a phone that appears and it configures itself securely.*
- **AC-E2.1** A newly connected supported phone appears in the approval inbox
  (`DeviceDetected`) without manual data entry. `CMOS-01-PRD-020`
- **AC-E2.2** Approving it (`POST /v1/devices/{id}:approve`) provisions and registers
  the device in < 30 s (Volume 17), using a short-lived signed URL — never a static
  `mac.cfg` URL (serves CMOS-00-ENG-010; detail in Volume 8). `CMOS-01-PRD-021`
- **AC-E2.3** Replacing a dead phone is ≤ 3 clicks: detect replacement → select the
  existing User → provision. `CMOS-01-PRD-022`

### E3 — Numbers, routing & call flows
*As an admin, I build a call flow visually and roll back mistakes.*
- **AC-E3.1** A DID can be routed to a Route/CallFlow/Queue/User declaratively; many
  DIDs MAY target one destination and one User MAY hold many DIDs. `CMOS-01-PRD-030`
- **AC-E3.2** Publishing a CallFlow is versioned and reversible; rollback restores a
  prior version without editing text (Time Machine; emits `CallFlowPublished`).
  `CMOS-01-PRD-031`
- **AC-E3.3** Time conditions, holidays, IVR menus, ring groups, and queues are
  expressible without scripting. `CMOS-01-PRD-032`

### E4 — Calling (the voice workload)
*As a user, I make and receive reliable calls; as an admin I can control them.*
- **AC-E4.1** An internal call sets up in < 150 ms (Volume 17) and emits
  `CallStarted → CallAnswered → CallEnded` with a shared `correlation_id`.
  `CMOS-01-PRD-040`
- **AC-E4.2** Transfer, hold/resume, and conference are available via API and UI and
  emit the corresponding events. `CMOS-01-PRD-041`

### E5 — Identity, security & policy
*As a security-conscious admin, I control who can do what and who can call where.*
- **AC-E5.1** Authorization is capability-based; a request lacking the required
  Capability returns `403 forbidden` (serves CMOS-00-ENG-009). `CMOS-01-PRD-050`
- **AC-E5.2** Policy can require an authenticated Identity for external/international
  calls while emergency calls bypass all such requirements (Emergency Override).
  `CMOS-01-PRD-051`
- **AC-E5.3** Every configuration or security action is recorded in an append-only
  audit log (serves CMOS-00-ENG-012). `CMOS-01-PRD-052`

### E6 — Billing & cost attribution
*As finance, I see attributable spend and can export it.*
- **AC-E6.1** Ending a call emits `BillingGenerated` and produces a CDR carrying the
  full attribution chain and cost as `{currency, minor_units}`. `CMOS-01-PRD-060`
- **AC-E6.2** Costs roll up to User, Department, Cost Centre, and Organisation and are
  exportable (`POST /v1/billing/exports`). `CMOS-01-PRD-061`

### E7 — AI & integration
*As an integrator, I react to events and enrich calls without touching the core.*
- **AC-E7.1** Any consumer can subscribe to the event stream (`GET /v1/stream`) and a
  signed webhook, receiving the Volume 5 envelope. `CMOS-01-PRD-070`
- **AC-E7.2** Recordings/transcripts are delivered as Object references, not raw
  media in payloads; an AIJob's result is retrievable as an Object (serves
  CMOS-00-ENG-013). `CMOS-01-PRD-071`

### E8 — Operate at any scale
*As an operator, I run this for 10 or 100,000 users with the same product.*
- **AC-E8.1** The system runs as a single binary for SMB and scales horizontally
  (split media) for enterprise with identical observable contracts (serves
  CMOS-00-ENG-015; Volume 3/14). `CMOS-01-PRD-080`
- **AC-E8.2** A rolling upgrade completes with no dropped calls (Volume 17).
  `CMOS-01-PRD-081`

### E9 — Multi-tenant MSP operation
*As an MSP, I manage many customers cleanly.*
- **AC-E9.1** A tenant's users/devices/data are never visible to another tenant, by
  construction (serves CMOS-00-ENG-008). `CMOS-01-PRD-090`
- **AC-E9.2** Config-as-code: an Organisation exports to a reviewable declarative file
  and re-imports deterministically (Volume 14). `CMOS-01-PRD-091`

## 4. Non-functional requirements
Performance (Volume 17), security (Volume 9), observability (Volume 15), and
upgradeability (Volume 14) requirements apply to every epic above. Where an AC cites
a numeric target, the number is normative and owned by the referenced volume.

## 5. Out of scope (this version)
Messaging, video, and contact-centre epics are sketched but their entities/flows are
reserved for v0.4+ (see Volume 2 Open items). This volume currently freezes the voice
+ platform surface.

## 6. Conformance notes
Each AC maps to one or more Volume 16 conformance scenarios; the scenario cites the
`CMOS-01-PRD-*` id and the entities/events/endpoints it drives.

## 7. Open items
- Per-persona detailed journey maps → Volume 13.
- Messaging/video/contact-centre epics → v0.4.

## Change log
- **0.3.0** — Personas, product goals, and nine epics with measurable, event-linked
  acceptance criteria.
