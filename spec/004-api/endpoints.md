# API — Endpoint Catalogue

Companion to [`README.md`](README.md) and the OpenAPI document
([`contracts/openapi/commos.openapi.yaml`](../../contracts/openapi/commos.openapi.yaml)).
All paths are under `/v1`. Every endpoint declares its required **Capability** and
the **events** it emits. Standard collection behaviour (cursor pagination, filtering,
`Idempotency-Key`, `If-Match`, Problem Details errors) applies per README §3–§6.

Legend: 🔒 requires the named capability · ⚡ emits event(s).

## Tenancy & people
| Method & path | Capability 🔒 | Emits ⚡ |
|---------------|--------------|----------|
| `GET/POST /organisations` · `GET/PATCH /organisations/{id}` | `org.manage` | `OrganisationUpdated` |
| `GET/POST /users` · `GET/PATCH/DELETE /users/{id}` | `users.manage` | `UserCreated`, `User*` |
| `GET/POST /departments` · `GET/PATCH/DELETE /departments/{id}` | `org.manage` | — |
| `GET/POST /cost-centres` | `billing.manage` | — |
| `GET/POST /capabilities` · `POST /users/{id}:grant` `:revoke` | `iam.manage` | `CapabilityGranted/Revoked` |
| `GET/POST /policies` · `GET/PATCH/DELETE /policies/{id}` | `policy.manage` | `PolicyUpdated` |
| `POST /identities:authenticate` | (public/edge) | `AuthenticationSucceeded\|Failed`, `IdentityAuthenticated` |

## Endpoints & numbering
| Method & path | Capability 🔒 | Emits ⚡ |
|---------------|--------------|----------|
| `GET/POST /devices` · `GET/PATCH /devices/{id}` | `devices.view` / `provision.devices` | — |
| `POST /devices/{id}:approve` | `provision.devices` | `DeviceApproved` |
| `POST /devices/{id}:reject` | `provision.devices` | `DeviceRejected` |
| `POST /devices/{id}:provision` | `provision.devices` | `ProvisioningStarted`, `ProvisioningFinished` |
| `POST /devices/{id}:replace` | `provision.devices` | `DeviceReplacementStarted` |
| `POST /devices/{id}:retire` | `provision.devices` | `DeviceRetired` |
| `GET/POST /extensions` | `numbering.manage` | — |
| `GET/POST /dids` | `numbering.manage` | — |
| `GET/POST /carriers` · `/gateways` · `/trunks` | `carriers.manage` | `GatewayOffline/Recovered` (observed) |

## Routing & call control
| Method & path | Capability 🔒 | Emits ⚡ |
|---------------|--------------|----------|
| `GET/POST /routes` | `routing.manage` | — |
| `GET/POST /call-flows` · `GET/PATCH /call-flows/{id}` | `routing.manage` | — |
| `POST /call-flows/{id}:publish` | `routing.manage` | `CallFlowPublished` |
| `GET/POST /ivrs` · `/queues` | `routing.manage` | — |
| `GET /calls` · `GET /calls/{id}` | `calls.view` | — |
| `POST /calls` (originate) | `calls.originate` (+ `calls.dial.international` for intl/elevated-cost) | `CallStarted` |
| `POST /calls/{id}:transfer` | `calls.control` | `CallTransferred` |
| `POST /calls/{id}:hold` `:resume` | `calls.control` | `CallHeld`/`CallResumed` |
| `POST /calls/{id}:hangup` | `calls.control` | `CallEnded` |
| `GET/POST /conferences` · `POST /conferences/{id}:invite` | `conf.manage` | `Conference*` |

## Media & objects
| Method & path | Capability 🔒 | Emits ⚡ |
|---------------|--------------|----------|
| `GET /recordings` · `GET /recordings/{id}` | `recordings.view` | — |
| `GET /voicemails` | `voicemail.view` | `VoicemailReceived` (ingest) |
| `POST /objects` (presigned upload) · `GET /objects/{id}` | `objects.rw` | `RecordingUploaded` |

## Billing
| Method & path | Capability 🔒 | Emits ⚡ |
|---------------|--------------|----------|
| `GET /cdrs` · `GET /cdrs/{id}` | `billing.view` | `BillingGenerated` (on Call end) |
| `POST /billing/exports` | `billing.export` | `ExportReady` |

## Integration & extensibility
| Method & path | Capability 🔒 | Emits ⚡ |
|---------------|--------------|----------|
| `GET/POST /webhooks` · `GET/PATCH/DELETE /webhooks/{id}` | `integrations.manage` | `WebhookDelivered\|Failed` |
| `GET/POST /automations` | `integrations.manage` | `AutomationTriggered` |
| `GET/POST /ai/jobs` · `GET /ai/jobs/{id}` | `ai.jobs` | `AIJobQueued`, `AIJobCompleted\|Failed` |
| `GET/POST /plugins` · `POST /plugins/{id}:enable` `:disable` | `plugins.manage` | `PluginInstalled\|Failed` |
| `GET /audit` | `audit.view` | (read-only, append-only source) |

## Security & platform (Volume 9)
| Method & path | Capability 🔒 | Emits ⚡ |
|---------------|--------------|----------|
| `GET/POST /secrets` · `GET/DELETE /secrets/{id}` | `secrets.manage` | `SecretReferenceUpdated` |
| `GET/POST /certificates` · `POST /certificates/{id}:renew` `:revoke` | `certs.manage` | `CertificateIssued`/`CertificateRenewed`/`CertificateRevoked` |

> `/secrets` stores and manages secret **references** only — plaintext secret values
> are never returned by the API (Volume 9, CMOS-09-SEC-*). The capability keys
> `secrets.manage`, `certs.manage`, and `calls.dial.international` are defined in the
> canonical catalogue at [`spec/009-security/capabilities.md`](../009-security/capabilities.md).

## Real-time & discovery
| Method & path | Notes |
|---------------|-------|
| `GET /stream` (WebSocket) | Subscribe to Volume 5 envelopes; filter by `type`, `subject`, `correlation_id`. Capability-gated per tenant. |
| `GET /events` (SSE) | Same payloads over SSE for simple consumers. |
| `GET /openapi.json` | The live OpenAPI; MUST match the frozen contract for the version. |
| `GET /events/schemas` | Event schema registry; MUST match `contracts/json-schema/events`. |

> All `:verb` actions are idempotent under `Idempotency-Key` and MUST be reflected in
> the Audit log (Volume 9) and the emitted event's `causation_id`.
