# Communications — SIP/WebRTC → Domain State & Event Mapping

Companion to [`README.md`](README.md). This is the **normative** binding that makes
CMOS-00-ENG-016 ("SIP is one transport") operational: every externally-observable
signalling/registration transition maps to exactly one domain state transition and
one canonical Event, all events of a Call sharing a `correlation_id`
(CMOS-07-SIP-026, CMOS-05-EVT-020).

Domain state machines: [Volume 2 state-machines.md](../002-domain-model/state-machines.md).
Events & envelope: [Volume 5](../005-events/README.md) and
[catalog.md](../005-events/catalog.md).

Reading rule: the **SIP** and **WebRTC** columns are *illustrative bindings*; the
**Domain transition** and **Event** columns are the contract. A future transport
conforms by filling in its own left-hand columns against the same right-hand
contract (CMOS-07-SIP-002).

> Note (informative): SIP identifiers (Call-ID, dialog tags, branch) are **not** in
> the event payload. They live only in an optional expert-mode `transport` object
> (CMOS-07-SIP-004). The cross-event join key is always `correlation_id`.

---

## 1. Registration (subject = Device)

| SIP | WebRTC equivalent | Guard | Domain transition (Device) | Event |
|-----|-------------------|-------|----------------------------|-------|
| `REGISTER` → `200 OK` | secure session established + endpoint bound | authenticated, credential valid (CMOS-07-SIP-011) | `PROVISIONED → OPERATIONAL` (first) / `OPERATIONAL` refresh | `RegistrationSucceeded` |
| `REGISTER` → `401/403` | session auth rejected | revoked/unknown credential | (no transition) | *(none; auth failure → audit)* |
| `REGISTER Expires:0` / de-register | endpoint closes / unbinds | explicit de-register | `OPERATIONAL` (loss loop) | `RegistrationLost` |
| refresh missed past grace | ICE consent / keepalive lost past grace | expiry without refresh (CMOS-07-SIP-012) | `OPERATIONAL` (loss loop) | `RegistrationLost` |
| `OPTIONS` keepalive ping | ICE consent / DTLS keepalive | reachability maintained | *(no transition)* | *(none — heartbeats are not events, CMOS-07-SIP-014)* |

Registration binds a **Device**, never a User/Extension (CMOS-07-SIP-013,
CMOS-00-ENG-002). Live binding state is ephemeral (Redis/NATS-class); the durable
`device` row is checkpointed on each success/loss (CMOS-06-DB-082).

## 2. Call setup (subject = Call)

All rows below share one `correlation_id` for the Call. `sequence` increments
per event within the correlation.

| SIP | WebRTC equivalent | Guard | Domain transition (Call) | Event |
|-----|-------------------|-------|--------------------------|-------|
| inbound/outbound `INVITE` (offer) | `createOffer` / offer exchanged | new call | `— → INITIATED` | `CallStarted` |
| `180 Ringing` / `183 Session Progress` | remote alerting / early media | destination alerted | `INITIATED → RINGING` | `CallRinging` |
| `200 OK` (answer) + ACK | answer applied, media connected | leg actually connected **and** chargeable-leg Identity resolved (CMOS-07-SIP-025) | `RINGING → ANSWERED` | `CallAnswered` |

If a chargeable leg would answer without a resolvable Identity, it MUST instead
take the `REJECTED` row below with a policy hangup cause (CMOS-02-DOM-010,
CMOS-07-SIP-025).

## 3. Call termination branches (subject = Call)

| SIP response / event | WebRTC equivalent | Guard | Domain transition | Event |
|----------------------|-------------------|-------|-------------------|-------|
| request timeout / `408` / no final response | no answer before timeout | ring timeout | `RINGING → NO_ANSWER*` | `CallNoAnswer` |
| `486 Busy Here` / `600 Busy Everywhere` | remote busy | destination busy | `RINGING → BUSY*` | `CallBusy` |
| `603 Decline` / `403` / policy denial | call declined / policy deny | destination or Policy rejects | `RINGING → REJECTED*` | `CallRejected` |
| `4xx/5xx/6xx` (other) / media setup failure | negotiation or media failure | signalling/media failure | `INITIATED/RINGING → FAILED*` | `CallFailed` |
| `BYE` | session close / `close()` | normal hangup | `ANSWERED/HELD → ENDED*` | `CallEnded` → triggers `BillingGenerated` |
| media/ICE timeout on established call | ICE disconnected past window | unrecovered media loss (CMOS-07-SIP-053) | `ANSWERED/HELD → ENDED*` (or `FAILED*`) | `CallEnded` / `CallFailed` |

`CallEnded` records a normalised `hangup_cause` and triggers CDR assembly
(Volume 10). Terminal states are marked `*`.

## 4. In-call state changes (subject = Call)

| SIP | WebRTC equivalent | Guard | Domain transition | Event |
|-----|-------------------|-------|-------------------|-------|
| re-INVITE → `sendonly`/`inactive` (SDP) | transceiver direction → `sendonly`/`inactive` | hold | `ANSWERED → HELD` | `CallHeld` |
| re-INVITE → `sendrecv` (SDP) | transceiver direction → `sendrecv` | resume | `HELD → ANSWERED` | `CallResumed` |
| re-INVITE (codec/media change) | renegotiation | bridge/media change | `ANSWERED/HELD → ANSWERED` | *(MediaStream update; `CallTransferred` only if re-targeted)* |

DTMF digits (RFC 2833/4733 telephone-event **or** SIP `INFO`) are normalised into
one internal digit representation and consumed by IVR/Call Flow nodes; individual
digits are not Call state transitions (CMOS-07-SIP-042).

## 5. Transfer (subject = Call; shared `correlation_id`)

| SIP | WebRTC equivalent | Guard | Domain transition | Event |
|-----|-------------------|-------|-------------------|-------|
| `REFER` (no consultation) | transfer API, no consult | blind transfer (CMOS-07-SIP-030) | `ANSWERED/HELD → ANSWERED` (re-targeted) | `CallTransferred` |
| consult `INVITE` answered, then `REFER`/`Replaces` | consult call + attended transfer | attended transfer (CMOS-07-SIP-031); consult leg linked via `causation_id` | `ANSWERED/HELD → ANSWERED` (joined) | `CallTransferred` |

The new/surviving legs then progress through §2/§3 with their own events, all on
the original `correlation_id`. Each chargeable leg stays attributable (Device +
Identity + Organisation) so every leg yields a correct CDR (CMOS-07-SIP-032,
CMOS-00-ENG-011).

## 6. Gateway / Trunk health (subject = Gateway; observed, not commanded)

| SIP | WebRTC / other | Guard | Domain transition (Gateway) | Event |
|-----|----------------|-------|-----------------------------|-------|
| `OPTIONS` ping fails / registration to carrier lost | trunk transport down | reachability lost | `ONLINE → OFFLINE` | `GatewayOffline` |
| `OPTIONS` ping restored / re-registered | trunk transport up | reachability restored | `OFFLINE → ONLINE` | `GatewayRecovered` |

A **mobile/4G gateway or SIM bank is just-another-Trunk**: identical health events
and Call events as a SIP trunk; routing MUST NOT special-case the medium
(CMOS-07-SIP-060, CMOS-00-ENG-016).

## 7. Correlation & ordering rules (recap, normative)

- **All** events of one Call — across setup, hold/resume, transfer, and every
  spawned leg — share one `correlation_id` (CMOS-07-SIP-026).
- Events within a `correlation_id` are totally ordered by `sequence`
  (CMOS-05-EVT-020); consumers order by `sequence`, not receipt time.
- `causation_id` links a caused event to its cause (e.g. the consultation leg that
  produced an attended `CallTransferred`).
- Re-delivery of any event is idempotent on `id` / `idempotency_key`
  (CMOS-05-EVT-011); a duplicate `sequence` for a correlation is ignored
  (CMOS-05-EVT-022).

## 8. Conformance

The `voice` behavioural suite (L2) drives an implementation through each row above
via SIP and via WebRTC and asserts: (a) the exact domain transition occurs, (b)
exactly the named event is emitted with the shared `correlation_id` and correct
`sequence`, (c) illegal transitions are rejected, (d) no SIP/SDP identifier appears
outside an optional `transport` object (CMOS-07-SIP-004), and (e) a SIP endpoint
and a WebRTC endpoint produce identical event streams for the same scenario
(CMOS-07-SIP-002). L3 repeats against real vendor devices and real SIP endpoints.
