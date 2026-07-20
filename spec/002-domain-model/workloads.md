# Domain Model — Communication Workloads

**Status:** REVIEW · **Version:** 0.4.0 · **Subsystem tag:** DOM

Companion to [`README.md`](README.md) (and to [`entities.md`](entities.md) /
[`state-machines.md`](state-machines.md)). This file is the **normative** prose for
the non-voice **Workloads** — messaging, video, presence, and contact centre — that
run as peers of the voice/telephony workload on the shared **Substrate**. It exists
to prove the domain model is *workload-general*: the second workload reuses the same
entity envelope, the same event model, and the same tenant boundary as the first.

Machine-readable form:
[`contracts/json-schema/entities/`](../../contracts/json-schema/entities/) and
[`contracts/json-schema/events/`](../../contracts/json-schema/events/). Where this
prose and a schema disagree about *shape*, the schema wins (CONVENTIONS §8). Field
names, types, and required-ness below mirror the entity schemas
(`Channel`, `Thread`, `Message`, `VideoRoom`, `Participant`, `PresenceState`,
`Agent`) and the event schemas named in §7 of each workload.

All requirements in this file are numbered **CMOS-02-DOM-100…199**. Requirements
`CMOS-02-DOM-001…099` (in [`README.md`](README.md)) are unchanged and continue to
govern every entity named here — in particular the common envelope
(CMOS-02-DOM-001), tenant scoping (CMOS-02-DOM-002), soft deletion
(CMOS-02-DOM-003), Digital Twin versioning (CMOS-02-DOM-005), and closed-set state
transitions (CMOS-02-DOM-007).

---

## 1. The Workload concept

A **Workload** is an application built on the **Substrate** (Glossary). Voice is one
Workload; messaging, video, presence, and contact centre are others. They are
**peers**: no Workload is privileged in the architecture, and the substrate embeds
no knowledge specific to any one of them.

- **CMOS-02-DOM-100** The domain model is **protocol-neutral and workload-general**.
  Messaging, video, presence, and contact-centre entities MUST be modelled as peer
  Workloads on the same Substrate as voice, reusing the common entity envelope
  (CMOS-02-DOM-001) and the canonical event model (Volume 5). No Workload MAY require
  a substrate service that is defined only for its own use. (Serves CMOS-00-ENG-016
  "SIP is one transport" and CMOS-00-ENG-004 "event-first".)
- **CMOS-02-DOM-101** A Workload MUST NOT be given a private data path. Every
  state change of consequence in any Workload MUST emit a canonical Event through the
  shared Event Bus (CMOS-00-ENG-004); every large binary artifact MUST be stored as
  an **Object** (CMOS-02-DOM-013); every chargeable unit of work MUST be attributable
  through the same Device→Identity→User→Organisation chain as voice
  (CMOS-00-ENG-011).
- **CMOS-02-DOM-102** A Call (voice) and a Message, VideoRoom, or contact-centre
  interaction are **peer Workload instances**. An implementation MUST NOT special-case
  voice in the substrate services (Identity, Routing, Presence, Media, Object Storage,
  Event Bus, Billing, Policy); each such service is consumed through its
  Workload-neutral contract.

### 1.1 What each Workload reuses vs. adds (informative)

> Note: this table is informative; the normative shapes are the schemas and §§2–5.

| Substrate service | Voice uses | Messaging adds | Video adds | Presence adds | Contact centre adds |
|---|---|---|---|---|---|
| **Identity** | per-Call Identity on a chargeable leg | `sender_ref`/attribution of a Message | `Participant.identity_id` per join | `PresenceState.user_id` subject | `Agent.user_id` subject |
| **Routing** | Route/CallFlow/Queue → Call | inbound Channel → Thread/Agent | room invite → VideoRoom | *feeds* routing (availability) | Queue selection by `Agent.state`/`skills` |
| **Presence** | ON_CALL derived from Call state | delivery/read affects availability | ON_CALL while in a room | **owns** `PresenceState` | Agent readiness gates queue offer |
| **Media** | RTP/SRTP MediaStream | — (text) | SFU/P2P media for the room | — | reuses voice Media for agent legs |
| **Object Storage** | Recording, Voicemail | `Message.attachments[]` Objects | room recording Objects | — | transcripts, recordings |
| **Event Bus** | `Call*` events | `Channel*`/`Thread*`/`Message*` | `VideoRoom*`/`Participant*` | `PresenceChanged` | `AgentStateChanged` |
| **Billing** | CDR per Call | per-Message / per-segment usage | per-participant-minute | — | agent-time / interaction usage |
| **Policy** | REQUIRE_IDENTITY on chargeable call | channel-kind allow/deny, DLP | room admission, recording consent | visibility scoping | queue/skill authorisation |

- **CMOS-02-DOM-103** Where a Workload needs a capability the voice Workload already
  models, it MUST reuse the existing entity rather than introduce a parallel one. In
  particular the contact-centre Workload MUST reuse the voice **Queue**
  ([`entities.md`](entities.md) §Route/CallFlow/IVR/Queue) and MUST NOT define a
  second queue entity.

## 2. Messaging Workload

Asynchronous, thread-structured, multi-channel text (and attachment) exchange. Three
entities: **Channel**, **Thread**, **Message**.

### 2.1 Entities

**Channel** — a durable conversation surface bound to a transport kind. Fields
(schema `Channel.schema.json`):

| Field | Type | Req | Notes |
|-------|------|-----|-------|
| `name` | string | | Display name |
| `kind` | enum | ✔ | `CHAT\|SMS\|WHATSAPP\|EMAIL\|INTERNAL` |
| `members` | array<string> | | Member references (User/Identity/external addresses) |
| `state` | enum | ✔ | `ACTIVE\|ARCHIVED` |

**Thread** — an ordered conversation within a Channel. Fields (`Thread.schema.json`):

| Field | Type | Req | Notes |
|-------|------|-----|-------|
| `channel_id` | uuid | ✔ | Owning Channel (same tenant) |
| `subject` | string | | Topic line |
| `state` | enum | ✔ | `OPEN\|CLOSED` |

**Message** — a single utterance. Fields (`Message.schema.json`):

| Field | Type | Req | Notes |
|-------|------|-----|-------|
| `channel_id` | uuid | ✔ | Owning Channel |
| `thread_id` | uuid | | Owning Thread (a Channel-level message MAY omit it) |
| `sender_ref` | string | ✔ | Sender reference; the attribution anchor (§2.4) |
| `body` | string | | Text body |
| `attachments` | array<uuid> | | **Object** ids only, never inline blobs (CMOS-02-DOM-013) |
| `state` | enum | ✔ | `SENT\|DELIVERED\|READ\|FAILED` |

- **CMOS-02-DOM-110** A Thread MUST reference exactly one Channel via `channel_id`,
  and a Message MUST reference the same Channel; a Message's `thread_id`, when
  present, MUST resolve to a Thread whose `channel_id` equals the Message's
  `channel_id`. All three MUST share the Message's `tenant_id` (CMOS-02-DOM-002); no
  reference may cross a tenant boundary.
- **CMOS-02-DOM-111** `Message.attachments[]` MUST contain **Object** identifiers
  only. A Message payload or event MUST NOT carry raw bytes (CMOS-02-DOM-013,
  CMOS-05-EVT-041).

### 2.2 Ownership

`Organisation` ─owns─▶ `Channel` ─contains─▶ `Thread` ─contains─▶ `Message`. Every
node is tenant-scoped; the ownership graph is acyclic with `Organisation` at the root
(CMOS-02-DOM-006).

### 2.3 Lifecycles (state machines)

Notation as in [`state-machines.md`](state-machines.md): `SOURCE → TARGET ⇒ EventName`.
Terminal states are marked `*`. An implementation MUST reject any transition not
listed and MUST emit the named Event (CMOS-02-DOM-007, CMOS-05-EVT-002).

**Channel**
```
— ────────▶ ACTIVE ──archive──▶ ARCHIVED*
```
| From | To | Guard | Event |
|------|----|-------|-------|
| — | ACTIVE | channel created | `ChannelCreated` |
| ACTIVE | ARCHIVED* | operator/policy archives | *(state change; see note)* |

> Note: `ARCHIVED` is a soft-terminal state (CMOS-02-DOM-003); history remains
> resolvable. v0.4.0 defines `ChannelCreated` as the only canonical messaging Channel
> event; archival is observable via the entity `state` and audit log.

**Thread**
```
— ──open──▶ OPEN ──close──▶ CLOSED* ──(reopen)──▶ OPEN
```
| From | To | Guard | Event |
|------|----|-------|-------|
| — | OPEN | thread opened in a Channel | `ThreadOpened` |
| OPEN | CLOSED* | resolved/closed | `ThreadClosed` |
| CLOSED | OPEN | reopened (new activity) | `ThreadOpened` |

**Message**
```
— ──send──▶ SENT ──deliver──▶ DELIVERED ──read──▶ READ*
                 │
                 └──fail──▶ FAILED*
```
| From | To | Guard | Event |
|------|----|-------|-------|
| — | SENT | accepted for transport | `MessageSent` |
| SENT | DELIVERED | transport confirms receipt | `MessageDelivered` |
| DELIVERED | READ* | recipient reads | `MessageRead` |
| SENT/DELIVERED | FAILED* | permanent transport failure | *(state change; `FAILED` is terminal)* |

- **CMOS-02-DOM-112** A Message MUST progress only along
  `SENT → DELIVERED → READ` (with `SENT`/`DELIVERED → FAILED`). `MessageDelivered`
  and `MessageRead` MUST reference the Message via `message_id`, and events sharing
  the Message's lifecycle MUST carry a common `correlation_id` (CMOS-05-EVT-020) so
  the send→deliver→read chain is totally ordered.

### 2.4 Mapping onto shared services

- **CMOS-02-DOM-113** Messaging attribution MUST reuse the voice attribution chain:
  `Message.sender_ref` resolves to a Device and/or Identity, hence a User, hence an
  Organisation, exactly as a chargeable Call leg does (CMOS-00-ENG-011). Per-Message
  or per-segment billing MUST be derived from this chain, not from the Channel
  `kind` alone.
- **CMOS-02-DOM-114** Channel `kind` selects the **transport binding** only (CHAT,
  SMS, WHATSAPP, EMAIL, INTERNAL); it MUST NOT change the entity contract. A
  consumer MUST treat all Channel kinds through the same Channel/Thread/Message
  shapes (CMOS-00-ENG-016 generalised: the transport is one transport).

### 2.5 Canonical events

`ChannelCreated`, `ThreadOpened`, `ThreadClosed`, `MessageSent`, `MessageDelivered`,
`MessageRead` (`contracts/json-schema/events/`).

## 3. Video Workload

Real-time multi-party media rooms. Two entities: **VideoRoom** and **Participant**
(Participant is shared with the contact-centre and conference surfaces, §3.4).

### 3.1 Entities

**VideoRoom** — a media room. Fields (`VideoRoom.schema.json`):

| Field | Type | Req | Notes |
|-------|------|-----|-------|
| `name` | string | | Display name |
| `mode` | enum | ✔ | `SFU\|P2P` (topology of the Media Plane) |
| `state` | enum | ✔ | `ACTIVE\|ENDED` |
| `participants` | array<string> | | Participant/session references |

**Participant** — a party's membership in a session. Fields
(`Participant.schema.json`):

| Field | Type | Req | Notes |
|-------|------|-----|-------|
| `session_ref` | string | ✔ | The session (VideoRoom / Conference / Call) joined |
| `identity_id` | uuid | | Attributed Identity for the join (chargeable / audited) |
| `role` | enum | ✔ | `HOST\|GUEST\|AGENT\|OBSERVER` |
| `joined_at` | timestamp | | |
| `left_at` | timestamp | | Set on leave |

- **CMOS-02-DOM-120** A VideoRoom MUST reference its parties through Participant
  entities; a Participant's `session_ref` MUST resolve to a session (VideoRoom,
  Conference, or Call) within the same `tenant_id` (CMOS-02-DOM-002).
- **CMOS-02-DOM-121** `VideoRoom.mode` selects the Media-Plane topology (`SFU`
  forwarding vs. `P2P`) and MUST NOT alter the control-plane contract
  (CMOS-00-ENG-006 control/media separation). Room recording, when present, MUST be
  stored as an **Object** (CMOS-02-DOM-013).

### 3.2 Ownership

`Organisation` ─owns─▶ `VideoRoom` ─has─▶ `Participant`. Tenant-scoped and acyclic
(CMOS-02-DOM-006).

### 3.3 Lifecycles (state machines)

**VideoRoom**
```
— ──start──▶ ACTIVE ──end──▶ ENDED*
```
| From | To | Guard | Event |
|------|----|-------|-------|
| — | ACTIVE | room started | `VideoRoomStarted` |
| ACTIVE | ENDED* | last party leaves / host ends | `VideoRoomEnded` |

**Participant** (membership within a live session)
```
— ──join──▶ (joined) ──leave──▶ (left)*
```
| From | To | Guard | Event |
|------|----|-------|-------|
| — | joined | party admitted (`joined_at` set) | `ParticipantJoined` |
| joined | left* | party leaves (`left_at` set) | `ParticipantLeft` |

- **CMOS-02-DOM-122** `ParticipantJoined`/`ParticipantLeft` MUST carry the
  `session_ref`, and MUST share the session's `correlation_id` so a room's join/leave
  sequence is totally ordered (CMOS-05-EVT-020). A VideoRoom MUST transition to
  `ENDED` only after emitting `VideoRoomEnded`; `duration_ms` in that event, when
  present, is derived, not authoritative.

### 3.4 Mapping onto shared services

- **CMOS-02-DOM-123** The **Participant** entity is Workload-neutral: it models
  membership in *any* multi-party session — VideoRoom, voice **Conference**
  ([`entities.md`](entities.md)), or a bridged **Call**. An implementation MUST NOT
  define a per-Workload participant entity; `session_ref` discriminates the session
  kind.
- **CMOS-02-DOM-124** Video media MUST run on the same Media Plane as voice, as typed
  MediaStreams behind the control/media interface (CMOS-00-ENG-006); the VideoRoom is
  a peer of Call, not a subtype of it.

### 3.5 Canonical events

`VideoRoomStarted`, `VideoRoomEnded`, `ParticipantJoined`, `ParticipantLeft`.

## 4. Presence Workload

The availability signal that other Workloads and Routing consume. One entity:
**PresenceState**.

### 4.1 Entity

**PresenceState** — a User's current availability. Fields
(`PresenceState.schema.json`):

| Field | Type | Req | Notes |
|-------|------|-----|-------|
| `user_id` | uuid | ✔ | Subject User |
| `status` | enum | ✔ | `AVAILABLE\|BUSY\|AWAY\|DND\|OFFLINE\|ON_CALL` |
| `since` | timestamp | | When the current status began |
| `device_id` | uuid | | Device the status is asserted from |

- **CMOS-02-DOM-130** A PresenceState MUST be tenant-scoped and MUST reference a
  `user_id` within the same Organisation (CMOS-02-DOM-002). `ON_CALL` SHOULD be
  derived from live Call/VideoRoom state rather than set directly, so presence
  reflects observed reality (Digital Twin, CMOS-02-DOM-005).

### 4.2 Lifecycle (state machine)

PresenceState is a single mutable status cell; every distinct `status` value is a
state, and any status MAY transition to any other. Each transition emits
`PresenceChanged` and updates `since`.

```
AVAILABLE ⇄ BUSY ⇄ AWAY ⇄ DND ⇄ ON_CALL ⇄ OFFLINE      ⇒ PresenceChanged
```
| From | To | Guard | Event |
|------|----|-------|-------|
| any | any (distinct) | user action / derived signal | `PresenceChanged` |

- **CMOS-02-DOM-131** A PresenceState transition MUST emit `PresenceChanged` carrying
  `user_id` and the new `status` (CMOS-05-EVT-002). A no-op "transition" to the same
  status MUST NOT emit an event.

### 4.3 Mapping onto shared services

- **CMOS-02-DOM-132** Presence **feeds Routing**: Route/CallFlow/Queue selection and
  Agent offering (§5) MUST be able to consult PresenceState so that `OFFLINE`, `DND`,
  or `BUSY` Users are not offered work they cannot take. Presence is an input to
  routing decisions, never a routing destination itself.
- **CMOS-02-DOM-133** Presence is derived from, and consistent with, other Workloads:
  entering a Call or VideoRoom SHOULD drive `ON_CALL`; leaving SHOULD restore the
  prior status. Consumers subscribe to `PresenceChanged`; the substrate embeds no
  presence-consumer-specific logic (CMOS-00-ENG-004).

### 4.4 Canonical event

`PresenceChanged`.

## 5. Contact-centre Workload

Queue-served, skills-routed interactions handled by human **Agents**. One new entity:
**Agent** — which composes the existing voice **Queue** rather than replacing it.

### 5.1 Entity

**Agent** — a User acting as a contact-centre resource. Fields
(`Agent.schema.json`):

| Field | Type | Req | Notes |
|-------|------|-----|-------|
| `user_id` | uuid | ✔ | The User behind the Agent (attribution anchor) |
| `queues` | array<uuid> | | **Queue** ids the Agent is a member of (voice Queue) |
| `state` | enum | ✔ | `READY\|BUSY\|WRAP_UP\|PAUSED\|OFFLINE` |
| `skills` | array<string> | | Skill keys matched by `SKILLS` queue strategy |

- **CMOS-02-DOM-140** An Agent MUST reference a `user_id` within the same
  Organisation, and every id in `Agent.queues[]` MUST reference a **Queue** in that
  same Organisation (CMOS-02-DOM-002). The Agent entity is the contact-centre
  *projection* of a User; it MUST NOT duplicate User identity or bypass the
  Device→Identity→User attribution chain (CMOS-00-ENG-011).

### 5.2 Lifecycle (state machine)

**Agent**
```
OFFLINE ──login──▶ READY ⇄ BUSY ──▶ WRAP_UP ──▶ READY
   ▲                 │  │                          │
   │                 │  └──────── PAUSED ◀──────────┘
   └───────── logout ─┴──────────────────────────── (any → OFFLINE)
```
| From | To | Guard | Event |
|------|----|-------|-------|
| OFFLINE | READY | agent logs in / goes available | `AgentStateChanged` |
| READY | BUSY | interaction offered & accepted | `AgentStateChanged` |
| BUSY | WRAP_UP | interaction ends, after-call work | `AgentStateChanged` |
| WRAP_UP | READY | wrap-up complete | `AgentStateChanged` |
| READY/WRAP_UP | PAUSED | agent pauses (break) | `AgentStateChanged` |
| PAUSED | READY | agent resumes | `AgentStateChanged` |
| any | OFFLINE | agent logs out / connection lost | `AgentStateChanged` |

- **CMOS-02-DOM-141** Every Agent state transition MUST emit `AgentStateChanged`
  carrying `agent_user_id` and the new `state` (CMOS-05-EVT-002). Only an Agent in
  `READY` MAY be offered a queued interaction; an implementation MUST NOT route work
  to an Agent in `BUSY`, `WRAP_UP`, `PAUSED`, or `OFFLINE`.

### 5.3 Mapping onto shared services

- **CMOS-02-DOM-142** The contact-centre Workload MUST reuse the voice **Queue**
  entity and its `strategy`/`members[]`/`sla_seconds` contract
  ([`entities.md`](entities.md)); `Agent.queues[]` are references *into* those
  Queues, and the `SKILLS` strategy matches `Agent.skills[]`. There is exactly one
  Queue concept across voice and contact centre (CMOS-02-DOM-103).
- **CMOS-02-DOM-143** Agent availability MUST be reconcilable with **Presence**:
  entering `BUSY` SHOULD be consistent with the User's `ON_CALL`/`BUSY`
  PresenceState, and `OFFLINE` with presence `OFFLINE`. Routing consults both Agent
  `state` (workload-readiness) and PresenceState (person-availability, §4.3).
- **CMOS-02-DOM-144** Contact-centre interactions are peer Workload instances: a
  queued interaction MAY be a Call, a Message Thread, or a VideoRoom. The Agent
  entity and Queue are media-neutral; the substrate MUST NOT assume a contact-centre
  interaction is voice (CMOS-00-ENG-016).

### 5.4 Canonical event

`AgentStateChanged`.

## 6. Cross-Workload invariants

- **CMOS-02-DOM-150** Every second-Workload entity (Channel, Thread, Message,
  VideoRoom, Participant, PresenceState, Agent) carries the common envelope
  (CMOS-02-DOM-001) and is tenant-scoped (CMOS-02-DOM-002). No relationship —
  `channel_id`, `thread_id`, `session_ref`, `user_id`, `queues[]`, `identity_id`,
  `device_id`, `attachments[]` — MAY cross a `tenant_id` boundary.
- **CMOS-02-DOM-151** Terminal states across Workloads (`ARCHIVED`, `CLOSED`,
  `READ`, `FAILED`, `ENDED`, `left`, `OFFLINE`) are soft (CMOS-02-DOM-003): the
  record is retained and history remains resolvable; a reopen (Thread) or
  re-login (Agent) is a new legal transition, never a resurrection of a hard-deleted
  record.
- **CMOS-02-DOM-152** No second-Workload event payload carries secrets, raw media,
  or bodies larger than a reference; attachments and recordings are **Object**
  references only (CMOS-02-DOM-013, CMOS-05-EVT-041). `Message.body` text is the sole
  permitted inline content and is subject to PII minimisation (CMOS-05-EVT-042).

## Conformance notes

- **L1** — Entities emitted for the messaging, video, presence, and contact-centre
  Workloads MUST validate against
  `contracts/json-schema/entities/{Channel,Thread,Message,VideoRoom,Participant,PresenceState,Agent}.schema.json`;
  emitted events MUST validate against the envelope and the named event schemas in
  `contracts/json-schema/events/`.
- **L2** — State transitions MUST match the state machines in §§2–5: illegal
  transitions are rejected, and exactly the specified Event is emitted with a shared
  `correlation_id` (CMOS-05-EVT-020). The behavioural scenario
  [`conformance/scenarios/message-send.json`](../../conformance/scenarios/message-send.json)
  (profile `messaging`, entity `Message`) drives `send → SENT ⇒ MessageSent`,
  `deliver → DELIVERED ⇒ MessageDelivered`, `read → READ ⇒ MessageRead`, and cites
  `CMOS-00-ENG-004` and `CMOS-00-ENG-016` — the proof that the substrate carries a
  non-voice Workload with the same event model as voice.
- The harness (`conformance/run.py`) checks that every entity and event named here
  has a schema and that example instances validate (CONVENTIONS §8).

## Change log

- **0.4.0** — Second-Workload entities and events added: **Channel**, **Thread**,
  **Message** (messaging); **VideoRoom**, **Participant** (video); **PresenceState**
  (presence); **Agent** (contact centre), the latter composing the existing voice
  **Queue**. Introduced requirement block `CMOS-02-DOM-100…152` proving the domain
  model is Workload-general (traces to CMOS-00-ENG-004 and CMOS-00-ENG-016). Closes
  the "Messaging/Video workload entities" open item from [`README.md`](README.md) §7.
