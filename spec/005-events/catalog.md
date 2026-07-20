# Canonical Event Catalogue

All events use the envelope in
[`envelope.schema.json`](../../contracts/json-schema/envelope.schema.json)
(see [README §2](README.md#2-the-envelope-normative)). As of v0.4 **every catalogued
event is schema-backed** (`core`) with an example instance validated by the
conformance harness.

Naming is `PascalCase`, past tense (CMOS-05-EVT-001). Type names are permanent within
a MAJOR line (CMOS-CONV-003). Adding an event is a MINOR change.

## Identity & User
| Event | Schema |
|-------|--------|
| `UserCreated` | [core](../../contracts/json-schema/events/UserCreated.schema.json) |
| `UserUpdated` | [core](../../contracts/json-schema/events/UserUpdated.schema.json) |
| `UserActivated` | [core](../../contracts/json-schema/events/UserActivated.schema.json) |
| `UserSuspended` | [core](../../contracts/json-schema/events/UserSuspended.schema.json) |
| `UserDeactivated` | [core](../../contracts/json-schema/events/UserDeactivated.schema.json) |
| `AuthenticationRequested` | [core](../../contracts/json-schema/events/AuthenticationRequested.schema.json) |
| `AuthenticationSucceeded` | [core](../../contracts/json-schema/events/AuthenticationSucceeded.schema.json) |
| `AuthenticationFailed` | [core](../../contracts/json-schema/events/AuthenticationFailed.schema.json) |
| `IdentityAuthenticated` | [core](../../contracts/json-schema/events/IdentityAuthenticated.schema.json) |
| `IdentityExpired` | [core](../../contracts/json-schema/events/IdentityExpired.schema.json) |
| `IdentityRevoked` | [core](../../contracts/json-schema/events/IdentityRevoked.schema.json) |
| `OrganisationUpdated` | [core](../../contracts/json-schema/events/OrganisationUpdated.schema.json) |
| `PolicyUpdated` | [core](../../contracts/json-schema/events/PolicyUpdated.schema.json) |
| `CapabilityGranted` | [core](../../contracts/json-schema/events/CapabilityGranted.schema.json) |
| `CapabilityRevoked` | [core](../../contracts/json-schema/events/CapabilityRevoked.schema.json) |

## Device, Provisioning & Registration
| Event | Schema |
|-------|--------|
| `DeviceDetected` | [core](../../contracts/json-schema/events/DeviceDetected.schema.json) |
| `DeviceApproved` | [core](../../contracts/json-schema/events/DeviceApproved.schema.json) |
| `DeviceRejected` | [core](../../contracts/json-schema/events/DeviceRejected.schema.json) |
| `DeviceRetired` | [core](../../contracts/json-schema/events/DeviceRetired.schema.json) |
| `DeviceReplacementStarted` | [core](../../contracts/json-schema/events/DeviceReplacementStarted.schema.json) |
| `ProvisioningStarted` | [core](../../contracts/json-schema/events/ProvisioningStarted.schema.json) |
| `ProvisioningFinished` | [core](../../contracts/json-schema/events/ProvisioningFinished.schema.json) |
| `RegistrationSucceeded` | [core](../../contracts/json-schema/events/RegistrationSucceeded.schema.json) |
| `RegistrationLost` | [core](../../contracts/json-schema/events/RegistrationLost.schema.json) |

## Call
| Event | Schema |
|-------|--------|
| `CallStarted` | [core](../../contracts/json-schema/events/CallStarted.schema.json) |
| `CallRinging` | [core](../../contracts/json-schema/events/CallRinging.schema.json) |
| `CallAnswered` | [core](../../contracts/json-schema/events/CallAnswered.schema.json) |
| `CallHeld` | [core](../../contracts/json-schema/events/CallHeld.schema.json) |
| `CallResumed` | [core](../../contracts/json-schema/events/CallResumed.schema.json) |
| `CallTransferred` | [core](../../contracts/json-schema/events/CallTransferred.schema.json) |
| `CallEnded` | [core](../../contracts/json-schema/events/CallEnded.schema.json) |
| `CallFailed` | [core](../../contracts/json-schema/events/CallFailed.schema.json) |
| `CallNoAnswer` | [core](../../contracts/json-schema/events/CallNoAnswer.schema.json) |
| `CallBusy` | [core](../../contracts/json-schema/events/CallBusy.schema.json) |
| `CallRejected` | [core](../../contracts/json-schema/events/CallRejected.schema.json) |

## Media, Conference, Gateway, Routing
| Event | Schema |
|-------|--------|
| `RecordingUploaded` | [core](../../contracts/json-schema/events/RecordingUploaded.schema.json) |
| `VoicemailReceived` | [core](../../contracts/json-schema/events/VoicemailReceived.schema.json) |
| `ConferenceStarted` | [core](../../contracts/json-schema/events/ConferenceStarted.schema.json) |
| `ConferenceEnded` | [core](../../contracts/json-schema/events/ConferenceEnded.schema.json) |
| `CallFlowPublished` | [core](../../contracts/json-schema/events/CallFlowPublished.schema.json) |
| `GatewayOffline` | [core](../../contracts/json-schema/events/GatewayOffline.schema.json) |
| `GatewayRecovered` | [core](../../contracts/json-schema/events/GatewayRecovered.schema.json) |
| `MediaQualityReported` | [core](../../contracts/json-schema/events/MediaQualityReported.schema.json) |

## Billing, Webhook, Automation, AI, Plugin, Security, Audit
| Event | Schema |
|-------|--------|
| `BillingGenerated` | [core](../../contracts/json-schema/events/BillingGenerated.schema.json) |
| `WebhookDelivered` | [core](../../contracts/json-schema/events/WebhookDelivered.schema.json) |
| `WebhookDeliveryFailed` | [core](../../contracts/json-schema/events/WebhookDeliveryFailed.schema.json) |
| `AutomationTriggered` | [core](../../contracts/json-schema/events/AutomationTriggered.schema.json) |
| `AIJobQueued` | [core](../../contracts/json-schema/events/AIJobQueued.schema.json) |
| `AIJobStarted` | [core](../../contracts/json-schema/events/AIJobStarted.schema.json) |
| `AIJobFailed` | [core](../../contracts/json-schema/events/AIJobFailed.schema.json) |
| `AIJobCompleted` | [core](../../contracts/json-schema/events/AIJobCompleted.schema.json) |
| `PluginInstalled` | [core](../../contracts/json-schema/events/PluginInstalled.schema.json) |
| `PluginEnabled` | [core](../../contracts/json-schema/events/PluginEnabled.schema.json) |
| `PluginDisabled` | [core](../../contracts/json-schema/events/PluginDisabled.schema.json) |
| `PluginUninstalled` | [core](../../contracts/json-schema/events/PluginUninstalled.schema.json) |
| `PluginFailed` | [core](../../contracts/json-schema/events/PluginFailed.schema.json) |
| `SecretReferenceUpdated` | [core](../../contracts/json-schema/events/SecretReferenceUpdated.schema.json) |
| `CertificateIssued` | [core](../../contracts/json-schema/events/CertificateIssued.schema.json) |
| `CertificateRenewed` | [core](../../contracts/json-schema/events/CertificateRenewed.schema.json) |
| `CertificateRevoked` | [core](../../contracts/json-schema/events/CertificateRevoked.schema.json) |
| `ExportReady` | [core](../../contracts/json-schema/events/ExportReady.schema.json) |
| `AuditEntryRecorded` | [core](../../contracts/json-schema/events/AuditEntryRecorded.schema.json) |

## Messaging, Video, Presence, Contact-centre (second workloads)
| Event | Schema |
|-------|--------|
| `ChannelCreated` | [core](../../contracts/json-schema/events/ChannelCreated.schema.json) |
| `ThreadOpened` | [core](../../contracts/json-schema/events/ThreadOpened.schema.json) |
| `ThreadClosed` | [core](../../contracts/json-schema/events/ThreadClosed.schema.json) |
| `MessageSent` | [core](../../contracts/json-schema/events/MessageSent.schema.json) |
| `MessageDelivered` | [core](../../contracts/json-schema/events/MessageDelivered.schema.json) |
| `MessageRead` | [core](../../contracts/json-schema/events/MessageRead.schema.json) |
| `VideoRoomStarted` | [core](../../contracts/json-schema/events/VideoRoomStarted.schema.json) |
| `VideoRoomEnded` | [core](../../contracts/json-schema/events/VideoRoomEnded.schema.json) |
| `ParticipantJoined` | [core](../../contracts/json-schema/events/ParticipantJoined.schema.json) |
| `ParticipantLeft` | [core](../../contracts/json-schema/events/ParticipantLeft.schema.json) |
| `PresenceChanged` | [core](../../contracts/json-schema/events/PresenceChanged.schema.json) |
| `AgentStateChanged` | [core](../../contracts/json-schema/events/AgentStateChanged.schema.json) |

## Envelope invariants (recap)
Every event carries: `id` (UUIDv7), `time` (UTC), `tenant_id`, `subject`,
`correlation_id`, `idempotency_key`, `sequence`, `specversion`. See
[README §2–§5](README.md#2-the-envelope-normative).
