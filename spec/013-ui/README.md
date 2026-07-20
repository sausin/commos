# Volume 13 — User Interface

**Status:** DRAFT · **Version:** 0.3.0 · **Subsystem tag:** UI

The administrative and operational UI is the human-facing embodiment of the
constitution's operability mandate (CMOS-00-ENG-001) and progressive-complexity
principle (Volume 0 §5, N-5). It is a **client of the public API and nothing else**
(CMOS-00-ENG-003): every screen is a projection of the API surface (Volume 4) and
the event stream (Volume 5). This volume specifies the information architecture,
primary workflows, the Call Flow builder, search, keyboard interaction,
accessibility, theming, and responsiveness that a conforming UI MUST provide. It
does **not** specify pixels; it specifies capabilities, contracts, and quality
floors that any UI implementation is measured against.

> Note (informative): the reference implementation is Vue 3 + TypeScript + Tailwind
> CSS. This stack is **recommended, not mandated** (Volume 3 preamble); a CLI, a
> mobile app, a Terraform provider, and a third-party console are all equally valid
> clients of the same API and are equally bound by the normative requirements here
> that concern behaviour rather than presentation.

---

## 1. Scope

In scope: the operator/administrator web console and the end-user self-service
surface — navigation, workflows, the visual Call Flow editor, live-call and reporting
views, search, shortcuts, accessibility, dark mode, and responsive behaviour. Out of
scope: the API itself (Volume 4), the event envelope (Volume 5), device firmware UIs
(Volume 8), and endpoint softphone media (Volume 7). The desk-phone/softphone *call
experience* is a Device concern; this volume covers the **management** UI and the
web **operator** views (live calls, supervision) that ride the API.

## 2. Foundational constraints (normative)

- **CMOS-13-UI-001** The UI MUST consume **only** the public API (Volume 4), its
  real-time transports (`/v1/stream` WebSocket, `/v1/events` SSE — CMOS-04-API-004),
  and the Object Storage presigned-URL flow (CMOS-03-ARCH-041). It MUST NOT reach any
  private endpoint, database, or back channel. Anything the UI can do, an API client
  can do, and vice versa (CMOS-00-ENG-003). A feature that cannot be expressed against
  the public API MUST NOT be added to the UI; the gap is a Volume 4 defect.
- **CMOS-13-UI-002** The UI MUST authenticate with the same credentials and be subject
  to the same capability checks as any client (CMOS-04-API-020/021). The UI MUST
  render only actions the authenticated principal holds the Capability for, and MUST
  degrade gracefully (hide or disable with explanation) rather than issue calls it
  knows will `403` — but the server remains the authority; a UI check is never a
  substitute for the API check (defence in depth, CMOS-03-ARCH-050).
- **CMOS-13-UI-003** The UI MUST be tenant-scoped from its credential
  (CMOS-04-API-022). It MUST NOT offer any affordance to name, enumerate, or cross into
  another Organisation; tenant selection for multi-org operators is a switch between
  distinct scoped credentials, never a shared view.
- **CMOS-13-UI-004** The UI MUST treat the API as the system of record and hold **no
  authoritative state**. Local state is a cache; the UI MUST reconcile it against
  events (Volume 5) and conditional reads, and MUST use `If-Match: <version>` on
  edits so a stale write fails closed with `412 precondition_failed`
  (CMOS-04-API-050) rather than clobbering a concurrent change.
- **CMOS-13-UI-005** The UI SHOULD attach an `Idempotency-Key` to every mutating
  request it originates (CMOS-04-API-023) so that user retries, double-clicks, and
  network replays apply at most once.

## 3. Progressive complexity & Expert Mode (normative)

The UI is the primary place the "zero-training" promise is kept or broken.

- **CMOS-13-UI-010** The **default surface** MUST express the system exclusively in
  operator vocabulary — **People, Phones, Departments, Call Flows, Business Hours,
  Queues, Numbers, Devices, Reports** — mapped to the canonical glossary entities
  (People→User/Identity, Phones/Devices→Device, Numbers→DID/Extension, Call
  Flows→CallFlow, Business Hours→time-condition nodes, Queues→Queue). The default
  surface MUST NOT require the operator to understand SIP, SDP, codecs, NAT, dialplans,
  or protocol internals to complete any common task (CMOS-00-ENG-001, CMOS-00-ENG-005,
  Volume 0 §5).
- **CMOS-13-UI-011** SIP headers, SDP bodies, codec negotiation, NAT/ICE behaviour,
  registration internals, raw SIP/RTP traces, and PCAP are **Expert Mode** surfaces
  (N-5). They MUST be hidden by default and revealed only on explicit opt-in per
  principal (a preference, never a global default). Enabling Expert Mode MUST NOT
  change any behaviour of the platform; it only reveals additional read-only diagnostic
  views and the trace/PCAP controls of Volume 15.
- **CMOS-13-UI-012** Complexity MUST be **revealed, never required**
  (Volume 0 §5). Advanced fields MUST be collapsed behind progressive disclosure with
  safe defaults; the common case MUST be completable without opening any advanced
  panel (CMOS-00-ENG-001 design tenet).
- **CMOS-13-UI-013** Expert Mode visibility MUST itself be capability-gated: surfacing
  SIP traces, PCAP, and diagnostic bundles MUST require the relevant observability
  Capability (Volume 15), so revealing internals never bypasses authorization.
- **CMOS-13-UI-014** Every error surfaced to a user MUST name the intent that failed
  and the next action, derived from the Problem Details `code` and `detail`
  (CMOS-04-API-040/041), never a raw stack trace or an opaque status number
  (Volume 0 §6 design tenet). The `correlation_id` (CMOS-04-API-042) MUST be
  copyable from any error so a user can hand it to support.

## 4. Information architecture & navigation (normative)

- **CMOS-13-UI-020** The primary navigation MUST be organised by **operator intent**,
  not by subsystem. The reference top-level map is:
  - **People** — Users, Identities, Departments, Cost Centres.
  - **Phones & Devices** — Devices, the device **approval inbox**, provisioning status,
    firmware.
  - **Numbers** — DIDs, Extensions, Carriers, Gateways, Trunks.
  - **Call Flows** — the visual editor, Business Hours, IVRs, Queues, Routes.
  - **Live** — active Calls, Conferences, queue/agent boards, registrations.
  - **Reports** — CDRs, usage, quality, exports.
  - **Billing** — Cost Centre allocation, rating, invoices/exports.
  - **Settings** — Capabilities/roles, policies, webhooks, automations, plugins,
    audit, Expert Mode toggle.
- **CMOS-13-UI-021** Navigation MUST be **URL-addressable and deep-linkable**: every
  primary screen and every entity detail view MUST have a stable, shareable URL that
  restores the same view (subject to capability), so a support ticket or runbook can
  link directly to a Device, Call Flow, or CDR.
- **CMOS-13-UI-022** Any list view backed by a collection endpoint MUST use
  **cursor pagination** (CMOS-04-API-030); the UI MUST NOT assume offset/page-number
  paging and MUST tolerate concurrent inserts without duplicate or skipped rows.
- **CMOS-13-UI-023** "Roles" MAY be presented as named bundles of Capabilities for
  human convenience, but the UI MUST treat the underlying Capability grants as
  authoritative (CMOS-00-ENG-009); a role is a UI-side label over capabilities, never a
  distinct authorization primitive.

## 5. Primary workflows (normative)

- **CMOS-13-UI-030 — Device approval inbox.** Newly detected Devices
  (`DeviceDetected`, Volume 5) MUST appear in an approval inbox in near-real-time via
  the event stream, without a manual refresh. The inbox MUST let an operator with
  `provision.devices` **approve** or **reject** a Device via the `:approve`/`:reject`
  sub-resource actions (CMOS-04-API-001), surfacing zero-trust attributes (identity of
  the requesting endpoint, one-time token status) so approval is an informed decision
  (CMOS-00-ENG-010).
- **CMOS-13-UI-031 — User & Device management.** The UI MUST keep People, Identities,
  and Devices as **distinct** objects that can be associated but never conflated
  (CMOS-00-ENG-002; Glossary note). Assigning an Identity to a Device, or a User to a
  Department, MUST be an explicit association, and the UI MUST make clear that a Device
  is owned by the Organisation and merely *carries* Identities.
- **CMOS-13-UI-032 — Live calls & supervision.** The Live view MUST subscribe to the
  `Call/*` and `Conference/*` event families and reflect state transitions (ringing,
  answered, held, transferred, ended) as they occur. Supervisory actions (transfer,
  hangup, barge/whisper where supported by Volume 7) MUST be issued through the
  documented `:transfer`/`:hold`/`:hangup` actions, and MUST be capability-gated.
- **CMOS-13-UI-033 — Reports & CDRs.** The Reports surface MUST read CDRs and usage
  through `/v1/cdrs` and expose **export** through `/v1/billing/exports`; large exports
  MUST be delivered as Objects (`ExportReady`) fetched via presigned URL, never streamed
  through a bespoke endpoint (CMOS-03-ARCH-040/041). Every CDR row MUST be traceable to
  its three attributed identities — Device, User (via Identity), Organisation
  (CMOS-00-ENG-011).
- **CMOS-13-UI-034 — Billing.** The Billing surface MUST present rating and Cost-Centre
  allocation as read/report views over Volume 10 data; it MUST NOT expose raw rating
  internals to a non-expert operator and MUST render Money as
  `{currency, minor_units}` (CMOS-CONV-013), never as a float.
- **CMOS-13-UI-035 — Real-time freshness.** Any view labelled "live" MUST derive its
  state from the event stream (CMOS-04-API-004) and MUST visibly indicate stream
  health (connected / reconnecting / stale) so an operator never mistakes a frozen
  socket for an idle system.

## 6. The Call Flow builder (normative)

The Call Flow builder is the flagship expression of declarative configuration
(CMOS-00-ENG-005): the operator draws intent; the platform reconciles reality. It
edits the **CallFlow** entity (Volume 2 — `graph` of nodes+edges, `published_version`,
`state` `DRAFT|PUBLISHED|SUPERSEDED`).

- **CMOS-13-UI-040** The builder MUST present a Call Flow as a **visual directed
  graph** of typed nodes (time condition / Business Hours, IVR menu, Queue, Route,
  ring group, voicemail, external transfer, hang-up, and Volume-7 node types) connected
  by edges — a Node-RED-class canvas. It MUST NOT expose XML dialplans, Lua, or SIP
  profiles (CMOS-00-ENG-005, N-1).
- **CMOS-13-UI-041** The edited graph MUST map **losslessly** to and from the CallFlow
  `graph` field over the API. The UI MUST NOT hold flow semantics the API cannot
  represent; the canvas is a rendering of the entity, not a superset of it
  (CMOS-00-ENG-003).
- **CMOS-13-UI-042** Editing MUST operate on a **DRAFT**. The builder MUST support
  **undo/redo** across the editing session and MUST validate the graph
  (unreachable nodes, dangling edges, missing terminal, unresolved references) before
  allowing publish, surfacing each problem against the offending node.
- **CMOS-13-UI-043** Publishing MUST invoke `POST /v1/call-flows/{id}:publish`, which
  creates an **immutable new version** and emits `CallFlowPublished` (Volume 2
  state-machine; Volume 5). The UI MUST NOT mutate a published version in place
  (CMOS-00-ENG-012).
- **CMOS-13-UI-044 — Time Machine.** The builder MUST expose the version history of a
  Call Flow and MUST support **rollback** as a **republication of a prior version**
  (a new PUBLISHED version), never as a destructive edit (Volume 2 CallFlow machine;
  CMOS-00-ENG-012). History MUST be presented as an append-only timeline with author,
  time, and a visual diff between versions where feasible.
- **CMOS-13-UI-045** The builder SHOULD offer a **dry-run / test-call** affordance that
  evaluates a hypothetical inbound against the DRAFT and shows the traversed path,
  without placing a real Call, so intent can be verified before publish.
- **CMOS-13-UI-046** Concurrent edits to the same DRAFT MUST be detected via the
  Digital-Twin `version` / `If-Match` (CMOS-04-API-050); the builder MUST warn and
  offer reconciliation rather than silently overwrite a co-editor's work.

## 7. Search (normative)

- **CMOS-13-UI-050** The UI MUST provide a global search that resolves People,
  Devices, Numbers (DID/Extension), Call Flows, Queues, and recent Calls/CDRs, backed
  by documented list/filter endpoints (CMOS-04-API-031). Search MUST be
  capability-scoped: results MUST NOT reveal entities the principal cannot read.
- **CMOS-13-UI-051** Search MUST be **tenant-scoped** by construction
  (CMOS-04-API-022); no query may return or hint at another Organisation's data.
- **CMOS-13-UI-052** Search results MUST be deep-linkable (CMOS-13-UI-021) and SHOULD
  support keyboard navigation end-to-end (open, arrow, select — §8).

## 8. Keyboard & interaction (normative)

- **CMOS-13-UI-060** The UI MUST be fully operable by keyboard alone. Every action
  reachable by pointer MUST be reachable by keyboard, with a visible focus indicator
  (WCAG 2.2 SC 2.4.7, 2.4.11) and a logical focus order.
- **CMOS-13-UI-061** The UI MUST provide a **command palette** (a single keyboard
  entry point to search + actions) and MUST document its shortcut map in-product. Any
  single-key shortcut MUST be disable-able or remappable (WCAG 2.2 SC 2.1.4 Character
  Key Shortcuts).
- **CMOS-13-UI-062** Destructive or irreversible-looking actions (retire a Device,
  publish a Call Flow, revoke an Identity) MUST require explicit confirmation or be
  undoable, and MUST NOT be bound to an accidental single keypress
  (CMOS-00-ENG-001 operability).

## 9. Accessibility (normative)

- **CMOS-13-UI-070** The UI MUST conform to **WCAG 2.2 Level AA**. This is a release
  gate, not a backlog item.
- **CMOS-13-UI-071** All interactive controls MUST expose correct name, role, and
  value to assistive technology (WCAG 4.1.2), and dynamic regions (live calls, approval
  inbox, stream health) MUST use appropriate live-region semantics so screen-reader
  users receive real-time updates.
- **CMOS-13-UI-072** Text and essential UI MUST meet AA contrast (SC 1.4.3 / 1.4.11) in
  **both** light and dark themes (§11); colour MUST NOT be the sole carrier of meaning
  (SC 1.4.1) — call state, quality, and errors MUST also use text or iconography.
- **CMOS-13-UI-073** The UI MUST remain usable at 200% zoom and 320 CSS-px reflow
  width without loss of content or function (SC 1.4.4 / 1.4.10), and MUST honour
  reduced-motion preferences (SC 2.3.3).
- **CMOS-13-UI-074** Accessibility conformance MUST be part of the UI conformance
  evidence (automated axe-class checks plus documented manual audit); a regression
  below AA MUST block release.

## 10. Responsiveness (normative)

- **CMOS-13-UI-080** The management UI MUST be usable across **desktop, tablet, and
  mobile** viewports. Core read and operational tasks (approve a Device, answer/monitor
  the Live board, look up a CDR, roll back a Call Flow) MUST be completable on a small
  viewport.
- **CMOS-13-UI-081** The Call Flow builder MAY offer a reduced (read/inspect, limited
  edit) experience on small viewports, but MUST provide at minimum a **read-only** and
  **rollback**-capable view everywhere it renders (CMOS-13-UI-044); it MUST NOT silently
  hide the existence of nodes.
- **CMOS-13-UI-082** Layout MUST reflow rather than require horizontal scrolling of the
  page body (aligns with SC 1.4.10); wide content (graphs, wide tables, traces) MUST
  scroll within its own container.

## 11. Theming & dark mode (normative)

- **CMOS-13-UI-090** The UI MUST support **light and dark** themes and MUST honour the
  viewer's OS-level preference by default, with an explicit per-principal override.
- **CMOS-13-UI-091** Both themes MUST independently satisfy the AA contrast and
  non-colour-only requirements (CMOS-13-UI-072). Theme is presentation only and MUST
  NOT alter behaviour, available actions, or data.

## 12. Performance & resilience (informative → normative floor)

- **CMOS-13-UI-100** The UI MUST degrade gracefully when the event stream drops:
  it MUST fall back to polling documented endpoints and MUST surface the degraded state
  (CMOS-13-UI-035) rather than present stale data as live.
- **CMOS-13-UI-101** The UI SHOULD meet the interaction-latency and initial-load
  budgets defined in Volume 17 (Performance); those budgets are the authoritative
  numeric targets and are not restated here.

## Conformance notes

- **L1 (Contract):** The UI issues only documented API calls with valid bodies and
  headers (`If-Match`, `Idempotency-Key`, `Idempotency-Key` retry semantics), consumes
  the Volume 5 envelope verbatim over `/v1/stream` or `/v1/events`, and renders
  Problem-Details errors (CMOS-13-UI-001/004/014).
- **L2 (Behavioural):** Driven UI scenarios exercise the flagship workflows — device
  approval from `DeviceDetected` to `DeviceApproved`; Call Flow draft→publish→rollback
  producing `CallFlowPublished` and an append-only history; concurrent-edit conflict
  yielding `412`; capability-gated hiding of Expert Mode and unauthorized actions
  (CMOS-13-UI-002/013/030/043/044/046).
- **Accessibility:** WCAG 2.2 AA is verified by automated checks plus a documented
  manual audit; a below-AA regression blocks release (CMOS-13-UI-070/074).
- Because the UI is a pure API client (CMOS-13-UI-001), any UI behaviour that cannot be
  reproduced by an equivalent API script is a conformance defect in the UI, not a
  platform capability.

## Open items

- A machine-readable **UI capability manifest** (which screens map to which
  Capabilities and endpoints) — candidate for `contracts/` in a later version, to let
  the conformance harness assert screen↔capability↔endpoint coverage.
- Standard **CallFlow node-type catalogue** shared with Volume 7 so the builder's node
  palette and the routing engine stay in lockstep.
- In-product **onboarding / guided setup** flow definition (zero-training first-run).
- Localisation / i18n and right-to-left layout requirements — deferred.

## Change log

- **0.3.0** — Initial implementation-grade draft: API-only client constraint,
  progressive-complexity/Expert-Mode surface, intent-based information architecture,
  flagship workflows (device approval, live calls, reports, billing), the visual
  versioned/undoable Call Flow builder with Time Machine rollback, search, keyboard
  and command palette, WCAG 2.2 AA accessibility, responsiveness, and dark mode
  specified with stable requirement IDs.
