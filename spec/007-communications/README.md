# Volume 7 — Communications Workload (Voice)

**Status:** DRAFT · **Version:** 0.3.0 · **Subsystem tag:** SIP

This volume specifies the **voice communications workload**: how real-time
signalling and media are realised, and — critically — how every
externally-observable protocol transition maps onto the protocol-neutral domain
model ([Volume 2](../002-domain-model/README.md)) and the canonical events
([Volume 5](../005-events/README.md)). It is a Media-Plane volume
(CMOS-00-ENG-006, CMOS-03-ARCH §1).

The governing invariant is **CMOS-00-ENG-016 — SIP is one transport.** SIP/RTP is
*an implementation* of the Communications workload, not the workload itself. The
domain model knows Calls, Registrations, and Media Streams; it does not know
dialogs, INVITEs, or SDP. WebRTC and future transports are peers, not
second-class citizens.

Companion: [`state-mapping.md`](state-mapping.md) — the exhaustive mapping from
SIP dialog and registration events to CommOS state transitions and events.

> Note (informative): a reader from a legacy PBX will look for "the dialplan" and
> "SIP profiles" here and not find them. Routing is declarative (Volume 2 Route /
> CallFlow); this volume is only about turning a resolved routing decision into
> live media and reporting facts back as events.

---

## 1. Scope & the transport-abstraction boundary (normative)

- **CMOS-07-SIP-001** The Communications workload MUST be modelled against the
  domain entities **Call**, **MediaStream**, **Device registration**, **Gateway**,
  and **Trunk** — never against SIP constructs. SIP, WebRTC, and any future
  transport are pluggable **transport bindings** behind one internal signalling
  interface; adding a transport MUST NOT change the Call/Registration state
  machines or the event contract. (Serves CMOS-00-ENG-016.)
- **CMOS-07-SIP-002** Every transport binding MUST map its native
  session/registration lifecycle onto the *same* Call and Registration state
  machines ([Volume 2 state-machines.md](../002-domain-model/state-machines.md))
  and MUST emit the *same* canonical events. A subscriber MUST NOT be able to tell
  from the event stream whether a Call ran over SIP, WebRTC, or a 4G gateway,
  except via explicit, optional transport-detail fields. (Serves CMOS-00-ENG-016,
  CMOS-00-ENG-004.)
- **CMOS-07-SIP-003** Users and administrators MUST NOT be exposed to SIP headers,
  SDP, dialog identifiers, or codec negotiation in the default experience. These
  MAY be surfaced only under an explicit **expert mode** (protocol trace / SDP
  inspection). (Serves N-5, CMOS-00-ENG-001, CMOS-00-ENG-005.)
- **CMOS-07-SIP-004** No SIP-specific identifier (Call-ID, tags, branch, AoR) may
  appear in a canonical event `data` payload except inside an optional,
  clearly-namespaced `transport` object consumed only by expert tooling. The
  `correlation_id` (Volume 5) — not the SIP Call-ID — is the cross-event join key.
  (Serves CMOS-00-ENG-016, CMOS-05-EVT-020.)

## 2. Registration (normative)

A Device registers to become reachable; registration is a Media-Plane fact that
checkpoints Device state (Volume 2 Device machine; Volume 6 §CMOS-06-DB-082).

- **CMOS-07-SIP-010** A successful registration (SIP `REGISTER` → `200 OK`, or the
  equivalent WebRTC session establishment) MUST transition the Device toward
  `OPERATIONAL` and emit **`RegistrationSucceeded`** (subject = Device). Loss of
  registration (expiry without refresh, transport failure, explicit de-register)
  MUST emit **`RegistrationLost`**. (Serves CMOS-00-ENG-004; maps Volume 2 Device
  `PROVISIONED→OPERATIONAL` and the `OPERATIONAL` refresh/loss loop.)
- **CMOS-07-SIP-011** Registration MUST be authenticated per zero-trust
  provisioning: credentials/tokens are Device-scoped, delivered via the
  provisioning flow (Volume 8), and revocable; trust MUST NOT be implied by source
  IP or network location. A registration presenting revoked or unknown
  credentials MUST be rejected and MUST NOT emit `RegistrationSucceeded`. (Serves
  CMOS-00-ENG-010, CMOS-03-ARCH-050.)
- **CMOS-07-SIP-012** The implementation MUST refresh registrations before expiry
  and MUST treat a missed refresh (beyond a bounded grace) as `RegistrationLost`.
  The live registration binding (contact, transport, expiry) is **ephemeral
  distributed state** (Redis/NATS-class), never the durable system of record
  (CMOS-03-ARCH-010, CMOS-06-DB-001); the durable `device` row is checkpointed on
  each success/loss.
- **CMOS-07-SIP-013** Registration binds a **Device**, not a User or Extension. Who
  is on a call is resolved per-call via Identity (§4, CMOS-00-ENG-002); a shared
  Device stays registered across successive Users. An implementation MUST NOT
  conflate "registered" with "a specific user is present". (Serves
  CMOS-00-ENG-002.)
- **CMOS-07-SIP-014** Keepalive (SIP `OPTIONS` ping, WebRTC ICE consent /
  DTLS keepalive, NAT-binding refresh) MUST maintain reachability without emitting
  per-ping events; only the *state change* (success↔loss) is an event
  (CMOS-05-EVT-001 — events are facts of consequence, not heartbeats).

## 3. Call signalling lifecycle (normative)

Each externally-observable signalling transition maps 1:1 to a Call state
transition and its named event ([Volume 2 Call machine](../002-domain-model/state-machines.md);
full table in [`state-mapping.md`](state-mapping.md)).

- **CMOS-07-SIP-020** Sending/receiving an INVITE (or WebRTC offer) for a new Call
  MUST create the Call in `INITIATED` and emit **`CallStarted`**. Alerting
  (`180 Ringing`/`183`) MUST transition `INITIATED→RINGING` and emit
  **`CallRinging`**.
- **CMOS-07-SIP-021** A `200 OK` accepting the INVITE (answer) MUST transition
  `RINGING→ANSWERED` and emit **`CallAnswered`**. Answer MUST NOT be reported until
  media is negotiated and the leg is actually connected.
- **CMOS-07-SIP-022** Failure/rejection responses MUST map to the correct terminal
  branch and event: no answer (timeout) → `NO_ANSWER` / **`CallNoAnswer`**;
  `486 Busy` → `BUSY` / **`CallBusy`**; `603`/`403`/policy denial → `REJECTED` /
  **`CallRejected`**; `4xx/5xx/6xx` signalling or media failure → `FAILED` /
  **`CallFailed`**. (See [`state-mapping.md`](state-mapping.md) for the SIP
  response-code table.)
- **CMOS-07-SIP-023** `BYE` (or WebRTC session close, or media timeout) MUST
  transition `ANSWERED/HELD→ENDED` and emit **`CallEnded`**, which triggers CDR
  assembly (**`BillingGenerated`**, Volume 10). A normalised `hangup_cause` MUST be
  recorded (CMOS-02-DOM). (Serves CMOS-00-ENG-011.)
- **CMOS-07-SIP-024** Hold (re-INVITE to `sendonly`/`inactive`, or SDP direction
  change) MUST transition `ANSWERED→HELD` and emit **`CallHeld`**; resume
  (re-INVITE to `sendrecv`) MUST transition `HELD→ANSWERED` and emit
  **`CallResumed`**.
- **CMOS-07-SIP-025** A chargeable leg MUST NOT reach `ANSWERED` without a
  resolvable Identity per Policy (CMOS-02-DOM-010); the attempt MUST instead yield
  `CallRejected` with a policy hangup cause. Every answered chargeable Call
  therefore carries Device + Identity (→ User) + Organisation. (Serves
  CMOS-00-ENG-011.)
- **CMOS-07-SIP-026** All events of one Call — across every leg, transfer, and
  bridge — MUST share one `correlation_id` and MUST carry a per-correlation
  monotonic `sequence` (CMOS-05-EVT-020) so the whole call is reconstructable in
  order regardless of SIP dialog fan-out.

## 4. Transfer (blind & attended) (normative)

- **CMOS-07-SIP-030** **Blind (unattended) transfer** (SIP `REFER` without prior
  consultation) MUST re-target the Call to the new destination and emit
  **`CallTransferred`** on the same `correlation_id`; the resulting new leg MUST
  progress through the normal `RINGING→ANSWERED` transitions with its own events,
  all sharing the original Call's `correlation_id`. (Serves CMOS-00-ENG-004.)
- **CMOS-07-SIP-031** **Attended (consultative) transfer** (consultation Call
  answered, then `REFER`/replaces to join) MUST emit **`CallTransferred`** at the
  moment the transfer completes, linking the consultation leg via `causation_id`.
  Intermediate hold of the primary party MUST emit `CallHeld`/`CallResumed` as
  normal (§3).
- **CMOS-07-SIP-032** Transfer MUST preserve attribution: the surviving/created
  legs remain individually attributable (Device + Identity + Organisation) so each
  billable leg produces a correct CDR (CMOS-00-ENG-011, CMOS-02-DOM-014). A
  transfer MUST NOT orphan a chargeable leg from its Identity.
- **CMOS-07-SIP-033** The transport-neutral rule holds: a transfer originated from
  a WebRTC endpoint MUST produce the same `CallTransferred` semantics as a SIP
  `REFER` (CMOS-07-SIP-002).

## 5. Media, codecs & DTMF (normative)

- **CMOS-07-SIP-040** Media transport MUST support **RTP** and, where negotiated,
  **SRTP** for confidentiality/integrity. Each negotiated flow is a domain
  **MediaStream** (`AUDIO|VIDEO|APPLICATION`, with `direction` and negotiated
  `codec`); the platform reports MediaStreams, not SDP m-lines (CMOS-07-SIP-003).
- **CMOS-07-SIP-041** Codec negotiation (SDP offer/answer, WebRTC transceiver
  negotiation) is internal Media-Plane behaviour and MUST NOT be surfaced to users
  by default (N-5). The *outcome* — the selected codec per MediaStream — is
  recorded on the Call/CDR for quality and billing (Volume 10, Volume 15). The
  implementation SHOULD prefer a common codec to avoid transcoding, and MUST route
  transcoding to the Transcoding subsystem off the RTP hot path
  (CMOS-03-ARCH-021).
- **CMOS-07-SIP-042** **DTMF** MUST be conveyed reliably. The implementation MUST
  support **RFC 2833 / RFC 4733 telephone-event** RTP and **SIP INFO** DTMF, and
  MUST normalise both into a single internal digit-event representation so
  Call Flow / IVR nodes (Volume 2) consume DTMF transport-neutrally. In-band
  audio-tone DTMF MAY be detected but MUST NOT be assumed. (Serves
  CMOS-00-ENG-016, CMOS-00-ENG-005.)
- **CMOS-07-SIP-043** Media never traverses the Control Plane (CMOS-03-ARCH-003).
  Media-quality facts (MOS, jitter, loss, latency) flow Media→Control as
  MediaStream stats and events (Volume 15), never as a media path through control
  logic.
- **CMOS-07-SIP-044** Recordings, voicemail, and fax artifacts MUST be written to
  Object Storage as Objects and referenced by URI; the platform MUST NOT carry raw
  media inline in state or events (CMOS-00-ENG-007, CMOS-02-DOM-013,
  CMOS-05-EVT-041).

## 6. NAT traversal & WebRTC (normative)

- **CMOS-07-SIP-050** The implementation MUST perform NAT traversal for endpoints
  behind NAT: for SIP/RTP via symmetric RTP / rport / media relay as needed, and
  for WebRTC via **ICE** with **STUN** (candidate discovery) and **TURN** (relay
  fallback). Traversal MUST be automatic; users MUST NOT be asked to reason about
  NAT (N-5, CMOS-00-ENG-001). (Serves CMOS-00-ENG-001.)
- **CMOS-07-SIP-051** **WebRTC endpoints are first-class Devices/registrations**,
  not a bolt-on: a browser endpoint registers, calls, holds, transfers, and emits
  the identical Call/Registration events as a hardware phone (CMOS-07-SIP-002).
  WebRTC media MUST use SRTP with DTLS keying; signalling is carried over the
  platform's own secure transport, not exposed as raw SIP to the browser.
- **CMOS-07-SIP-052** TURN relay credentials MUST be short-lived and per-session
  (zero-trust, CMOS-00-ENG-010); a leaked static TURN secret MUST NOT grant
  standing relay access.
- **CMOS-07-SIP-053** ICE connectivity loss on an established Call MUST be treated
  as a media failure and, if unrecovered within a bounded window, MUST end the
  Call (`CallEnded` / `CallFailed` as appropriate) rather than leaving a zombie
  session.

## 7. Gateways, trunks, fax & mobile (normative)

- **CMOS-07-SIP-060** External connectivity is modelled uniformly as **Trunks on
  Carriers reached via Gateways** (Volume 2). **A mobile/4G gateway (or SIM bank)
  is just-another-Trunk**: it presents the same Trunk contract (channels, codecs,
  auth, health) and produces the same Call events as a SIP trunk. Routing logic
  MUST NOT special-case the transport medium. (Serves CMOS-00-ENG-016,
  CMOS-00-ENG-005.)
- **CMOS-07-SIP-061** Gateway/Trunk health is **observed, not commanded**:
  reachability transitions MUST emit **`GatewayOffline`** / **`GatewayRecovered`**
  (Volume 2 Gateway machine) and routing MUST react to health (overflow/failover)
  declaratively via Routes, not via hand-edited config. (Serves CMOS-00-ENG-005,
  CMOS-00-ENG-004.)
- **CMOS-07-SIP-062** **Fax** MUST be supported via **T.38** where the path allows,
  with a G.711 pass-through fallback; a received/sent fax is stored as an Object
  (`kind = FAX`) and referenced, never inlined (CMOS-07-SIP-044). Fax negotiation
  detail is expert-mode-only (N-5).
- **CMOS-07-SIP-063** Outbound calls over a Trunk MUST present the correct external
  identity (E.164 CLI) and remain attributable to the originating Device +
  Identity + Organisation so the resulting CDR is correct (CMOS-00-ENG-011,
  CMOS-CONV-016). Internal addressing uses opaque references, not dial strings
  (CMOS-CONV-016).

## 8. Externally-observable mapping (summary)

The binding is: **one signalling transition → one Call/Registration state
transition → one canonical event**, all sharing a `correlation_id`
(CMOS-07-SIP-026). The exhaustive table — SIP method/response, WebRTC equivalent,
domain transition, emitted event — is
[`state-mapping.md`](state-mapping.md). This is what makes CMOS-00-ENG-016 real: a
consumer integrates against Calls and events, and a future transport slots in
under the same table.

## Conformance notes

- Profile: `voice`. L1: transport bindings emit envelopes and `data` that validate
  against the Volume 5 schemas. L2: for a driven SIP/WebRTC scenario, the exact set
  and `sequence` order of Call/Registration events match
  [`state-mapping.md`](state-mapping.md), and re-delivery is idempotent
  (CMOS-05-EVT-011). L3 (Interoperable): the mapping holds against **real SIP
  endpoints and real vendor devices**, and a WebRTC and a SIP endpoint interoperate
  through the platform producing identical event streams (CMOS-07-SIP-002).
- A conforming implementation MUST demonstrate that no SIP/SDP identifier leaks
  into a non-`transport` event field (CMOS-07-SIP-004) and that the default UI/API
  surface exposes no SDP/codec-negotiation controls (CMOS-07-SIP-003, N-5).
- Attribution check: every answered chargeable Call in a driven scenario yields a
  CDR carrying Device + Identity + Organisation (CMOS-07-SIP-025, CMOS-00-ENG-011).

## Open items

- Video and screen-share as MediaStream kinds beyond audio — track Volume 2 v0.4
  Video workload entities.
- Presence/BLF and message-waiting indication mapping to events — reserved
  (Presence subsystem, Volume 3).
- SIP over WebSocket vs. native transport binding profiles — implementation note.
- Emergency Override interaction with CMOS-07-SIP-025 (identity bypass for
  emergency calls) — align with Policy volume.
- Formal expert-mode `transport` sub-object schema — candidate for `contracts/` in
  v0.4.

## Change log
- **0.3.0** — Initial implementation-grade draft: transport-abstraction boundary
  (SIP is one transport), registration & keepalive, full call signalling lifecycle,
  blind/attended transfer, RTP/SRTP media + codec negotiation + DTMF (RFC
  2833/4733 + SIP INFO), NAT traversal + ICE/STUN/TURN + WebRTC endpoints, fax
  (T.38), mobile/4G gateways as just-another-trunk — each mapped to the Volume 2
  Call/Registration state machines and Volume 5 events, with no SIP/SDP exposure by
  default.
