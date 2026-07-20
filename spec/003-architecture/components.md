# Components

Companion to [`README.md`](README.md). Each component names its **plane**, the
**Capabilities** it enforces, the **events** it produces/consumes, and its
**dependencies**. This is the inventory; interactions are in the README.

## Control Plane
| Component | Produces events | Consumes | Key deps |
|-----------|-----------------|----------|----------|
| **API Gateway** | (per action) | — | Identity, Policy |
| **Identity** | `AuthenticationSucceeded/Failed`, `IdentityAuthenticated/Expired/Revoked`, `UserCreated/*` | auth methods, SSO/OIDC/SAML/LDAP | PostgreSQL |
| **Policy Engine** | `PolicyUpdated` | Identity, Routing | PostgreSQL |
| **Routing** | `CallStarted`, `CallFlowPublished` | Policy, Presence | PostgreSQL, Media (commands) |
| **Provisioning** | `DeviceDetected/Approved/Rejected/Retired`, `Provisioning*` | discovery, vendor plugins | Object Storage, PKI |
| **Billing** | `BillingGenerated`, `ExportReady` | `CallEnded` | PostgreSQL, Object Storage |
| **Automation** | `AutomationTriggered` | all events | Event Bus, Plugin Runtime |
| **Presence** | registration/presence updates | `Registration*` | Redis/NATS |
| **Cluster Manager** | node/placement events | health | Redis/NATS |
| **Event Bus** | (relays all) | outbox | Redis/NATS ∥ Kafka ∥ JetStream |
| **Plugin Runtime** | `PluginInstalled/Failed` | scoped events/API | Wasmtime-class sandbox |

## Media Plane
| Component | Produces events | Consumes commands | Notes |
|-----------|-----------------|-------------------|-------|
| **SIP** | `CallRinging/Answered/Held/Resumed/Transferred/Ended`, `Registration*` | originate/transfer/hangup | signalling only |
| **RTP/SRTP** | media-quality facts (MOS/jitter/loss) | start/stop stream | hot path; no blocking |
| **Transcoding** | — | negotiate/convert | codec matrix |
| **Recording** | `RecordingUploaded` | start/stop record | background worker → Object Storage |
| **Conferencing** | `Conference*` | join/leave/mix | SFU-like, no global lock |
| **WebRTC** | `Call*` (browser) | ICE/offer/answer | STUN/TURN |

## Shared state
| Component | Role | Contract |
|-----------|------|----------|
| **PostgreSQL** | System of record for structured entities | Volume 6 |
| **Object Storage** (S3-compatible) | Large artifacts as Objects | CMOS-03-ARCH-040 |
| **Redis/NATS abstraction** | Distributed ephemeral state, pub/sub, locks, cursors | CMOS-03-ARCH-030 |

## External integration edges
- **Carriers/Gateways** (PSTN, mobile/4G, SIP trunks, SIM banks) via SIP.
- **Identity providers** (OIDC/SAML/LDAP/AD) via Identity.
- **AI systems / CRM / ERP / automation** via Event Bus, Webhooks, and the API
  (never embedded; CMOS-00-ENG-013).
- **Secret managers** (Vault, AWS/GCP KMS, 1Password) — secrets never in config
  (Volume 9).
