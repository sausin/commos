# Canonical Event Catalogue

All events use the envelope in
[`envelope.schema.json`](../../contracts/json-schema/envelope.schema.json)
(see [README §2](README.md#2-the-envelope-normative)). The **Schema** column links
the `data` payload schema. `core` = frozen with schema + example (validated by the
conformance harness); `planned` = named and reserved, schema to be added.

Naming is `PascalCase`, past tense (CMOS-05-EVT-001). Type names are permanent within
a MAJOR line (CMOS-CONV-003). Adding an event is a MINOR change.

## Identity & User
| Event | Subject | Schema | Notes |
|-------|---------|--------|-------|
| `UserCreated` | User | [core](../../contracts/json-schema/events/UserCreated.schema.json) | INVITED state |
| `UserUpdated` | User | planned | |
| `UserActivated` / `UserSuspended` / `UserDeactivated` | User | planned | |
| `AuthenticationRequested` | Identity | planned | |
| `AuthenticationSucceeded` | Identity | planned | |
| `AuthenticationFailed` | Identity | planned | assurance failure/PII-min |
| `IdentityAuthenticated` | Identity | [core](../../contracts/json-schema/events/IdentityAuthenticated.schema.json) | becomes ACTIVE |
| `IdentityExpired` / `IdentityRevoked` | Identity | planned | |

## Device, Provisioning & Registration
| Event | Subject | Schema | Notes |
|-------|---------|--------|-------|
| `DeviceDetected` | Device | [core](../../contracts/json-schema/events/DeviceDetected.schema.json) | zero-touch discovery |
| `DeviceApproved` | Device | [core](../../contracts/json-schema/events/DeviceApproved.schema.json) | operator approval |
| `DeviceRejected` / `DeviceRetired` | Device | planned | |
| `DeviceReplacementStarted` | Device | planned | |
| `ProvisioningStarted` | Device | planned | |
| `ProvisioningFinished` | Device | [core](../../contracts/json-schema/events/ProvisioningFinished.schema.json) | config delivered |
| `RegistrationSucceeded` | Device | [core](../../contracts/json-schema/events/RegistrationSucceeded.schema.json) | endpoint registered |
| `RegistrationLost` | Device | planned | |

## Call
| Event | Subject | Schema | Notes |
|-------|---------|--------|-------|
| `CallStarted` | Call | [core](../../contracts/json-schema/events/CallStarted.schema.json) | INITIATED |
| `CallRinging` | Call | planned | |
| `CallAnswered` | Call | [core](../../contracts/json-schema/events/CallAnswered.schema.json) | ANSWERED |
| `CallHeld` / `CallResumed` | Call | planned | |
| `CallTransferred` | Call | [core](../../contracts/json-schema/events/CallTransferred.schema.json) | bridge/transfer |
| `CallEnded` | Call | [core](../../contracts/json-schema/events/CallEnded.schema.json) | ENDED → triggers billing |
| `CallFailed` / `CallNoAnswer` / `CallBusy` / `CallRejected` | Call | planned | terminal branches |

## Media, Conference, Gateway
| Event | Subject | Schema | Notes |
|-------|---------|--------|-------|
| `RecordingUploaded` | Recording | [core](../../contracts/json-schema/events/RecordingUploaded.schema.json) | Object reference |
| `VoicemailReceived` | Voicemail | planned | |
| `ConferenceStarted` / `ConferenceEnded` | Conference | planned | |
| `CallFlowPublished` | CallFlow | planned | versioned publish |
| `GatewayOffline` / `GatewayRecovered` | Gateway | planned | health |
| `MediaQualityReported` | MediaStream | planned | MOS/jitter/loss facts, media→control (Vol 15/17) |

## Billing, Webhook, Automation, AI, Plugin, Audit
| Event | Subject | Schema | Notes |
|-------|---------|--------|-------|
| `BillingGenerated` | CDR | [core](../../contracts/json-schema/events/BillingGenerated.schema.json) | CDR emitted |
| `WebhookDelivered` | Webhook | [core](../../contracts/json-schema/events/WebhookDelivered.schema.json) | delivery receipt |
| `WebhookDeliveryFailed` | Webhook | planned | dead-letter |
| `AutomationTriggered` | Automation | planned | |
| `AIJobQueued` / `AIJobStarted` / `AIJobFailed` | AIJob | planned | |
| `AIJobCompleted` | AIJob | [core](../../contracts/json-schema/events/AIJobCompleted.schema.json) | result Object ref |
| `PluginInstalled` / `PluginEnabled` / `PluginDisabled` / `PluginUninstalled` / `PluginFailed` | Plugin | planned | lifecycle (Vol 12) |
| `SecretReferenceUpdated` | Secret | planned | reference only, never values (Vol 9) |
| `CertificateIssued` / `CertificateRenewed` / `CertificateRevoked` | Certificate | planned | PKI/mTLS (Vol 9) |
| `AuditEntryRecorded` | AuditEntry | planned | append-only |

## Envelope invariants (recap)
Every event carries: `id` (UUIDv7), `time` (UTC), `tenant_id`, `subject`,
`correlation_id`, `idempotency_key`, `sequence`, `specversion`. See
[README §2–§5](README.md#2-the-envelope-normative).
