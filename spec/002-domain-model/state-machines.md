# Domain Model — State Machines

Companion to [`README.md`](README.md). Each state machine is **normative**
(CMOS-02-DOM-007): an implementation MUST reject any transition not listed, and MUST
emit the named event (Volume 5) on each transition. Terminal states are marked `*`.

Notation: `SOURCE → TARGET  [guard]  ⇒ EventName`.

---

## Device

```
        detect                 approve                 build+push
DETECTED ─────▶ PENDING ───────────────▶ APPROVED ─────────────▶ PROVISIONED
   │               │  reject                 │                        │ register
   │               ▼                         │                        ▼
   │            REJECTED*                     │                    OPERATIONAL
   │                                          │                    │   │
   └──────────────────────────────────────── │ ───────────────────┘   │ replace
                                              │                        ▼
                                              │                    REPLACING
                                              ▼                        │ finish
                                           RETIRED* ◀───────────────────┘
```

| From | To | Guard | Event |
|------|----|-------|-------|
| — | DETECTED | device seen on network | `DeviceDetected` |
| DETECTED | PENDING | queued for review | `DeviceDetected` (pending) |
| PENDING | APPROVED | operator approves + capability `provision.devices` | `DeviceApproved` |
| PENDING | REJECTED* | operator rejects | `DeviceRejected` |
| APPROVED | PROVISIONED | config built & delivered via signed URL | `ProvisioningStarted`→`ProvisioningFinished` |
| PROVISIONED | OPERATIONAL | successful registration | `RegistrationSucceeded` |
| OPERATIONAL | REPLACING | replacement workflow started | `DeviceReplacementStarted` |
| REPLACING | OPERATIONAL | replacement provisioned | `ProvisioningFinished` |
| any (non-terminal) | RETIRED* | operator retires | `DeviceRetired` |
| OPERATIONAL | OPERATIONAL | registration refresh/loss | `RegistrationSucceeded`/`RegistrationLost` |

## Call

```
INITIATED ──▶ RINGING ──▶ ANSWERED ⇄ HELD
    │            │            │
    │            │            └────────▶ ENDED*
    ▼            ▼
  FAILED*   NO_ANSWER* / BUSY* / REJECTED*
```

| From | To | Guard | Event |
|------|----|-------|-------|
| — | INITIATED | new call | `CallStarted` |
| INITIATED | RINGING | destination alerted | `CallRinging` |
| INITIATED/RINGING | FAILED* | signalling/media failure | `CallFailed` |
| RINGING | NO_ANSWER* | timeout | `CallNoAnswer` |
| RINGING | BUSY* | destination busy | `CallBusy` |
| RINGING | REJECTED* | destination/policy rejects | `CallRejected` |
| RINGING | ANSWERED | answered | `CallAnswered` |
| ANSWERED | HELD | hold | `CallHeld` |
| HELD | ANSWERED | resume | `CallResumed` |
| ANSWERED/HELD | ANSWERED | transfer/bridge change | `CallTransferred` |
| ANSWERED/HELD | ENDED* | hangup | `CallEnded` (→ triggers `BillingGenerated`) |

Invariant CMOS-02-DOM-010: transition to `ANSWERED` on a chargeable leg requires a
resolvable Identity per Policy, else the attempt yields `CallRejected` with a
policy hangup cause.

## Identity

```
REQUESTED ──auth ok──▶ AUTHENTICATED ──activate──▶ ACTIVE ──┬─expire─▶ EXPIRED*
    │ auth fail                                             └─revoke─▶ REVOKED*
    ▼
 (no state; AuthenticationFailed event)
```

| From | To | Event |
|------|----|-------|
| — | REQUESTED | `AuthenticationRequested` |
| REQUESTED | AUTHENTICATED | `AuthenticationSucceeded` |
| REQUESTED | (rejected) | `AuthenticationFailed` |
| AUTHENTICATED | ACTIVE | `IdentityAuthenticated` |
| ACTIVE | EXPIRED* | `IdentityExpired` |
| ACTIVE | REVOKED* | `IdentityRevoked` |

## CallFlow (versioned; Time Machine)

```
DRAFT ──publish──▶ PUBLISHED ──publish newer──▶ SUPERSEDED*
  ▲                    │
  └──── rollback ───────┘   (rollback republishes a prior version as a new PUBLISHED)
```

Publishing MUST create an immutable version and emit `CallFlowPublished`. Rollback
is republication of a prior version (never mutation), preserving append-only history
(CMOS-00-ENG-012).

## User

```
INVITED ──accept──▶ ACTIVE ──suspend──▶ SUSPENDED ──reactivate──▶ ACTIVE
                      │                                 │
                      └──────── deactivate ─────────────┴──▶ DEACTIVATED
```

Events: `UserCreated` (INVITED), `UserActivated`, `UserSuspended`, `UserDeactivated`.
`DEACTIVATED` is soft; the record is retained (CMOS-02-DOM-003).

## Gateway health (observed, not commanded)

```
ONLINE ⇄ OFFLINE      ⇒ GatewayOffline / GatewayRecovered
```

## AIJob (external processing lifecycle)

```
QUEUED ──▶ RUNNING ──▶ COMPLETED*
   │           │
   └────────── └──▶ FAILED*
```

Events: `AIJobQueued`, `AIJobStarted`, `AIJobCompleted`, `AIJobFailed`. The platform
only tracks status; computation is external (CMOS-00-ENG-013).

---

## Conformance

The behavioural suite (L2) drives each entity through legal and illegal transitions
and asserts (a) illegal transitions are rejected and (b) exactly the specified event
is emitted with a shared `correlation_id`. See Volume 16 and `conformance/`.
