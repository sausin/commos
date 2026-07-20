# Domain Model — Entity Field Reference

Companion to [`README.md`](README.md). Field names, types, and required-ness are
**normative** and mirrored by [`contracts/json-schema/entities/`](../../contracts/json-schema/entities/).
Types follow [CONVENTIONS §6](../CONVENTIONS.md#6-identifiers-data-types-and-encoding):
`id`/`*_id` are UUIDv7 strings, timestamps are RFC 3339 UTC millis, money is
`{currency, minor_units}`.

Every entity carries the **common envelope** fields (omitted from the tables below):

| Field | Type | Notes |
|-------|------|-------|
| `id` | uuid | UUIDv7, immutable |
| `tenant_id` | uuid | Owning Organisation; equals `id` for Organisation |
| `version` | int | Monotonic; Digital Twin (CMOS-02-DOM-005) |
| `created_at` / `updated_at` | timestamp | |
| `state` | enum | Per entity's state machine, where applicable |

---

## Organisation
| Field | Type | Req | Notes |
|-------|------|-----|-------|
| `name` | string | ✔ | Display name |
| `slug` | string | ✔ | DNS-safe, unique globally |
| `default_currency` | string(ISO-4217) | ✔ | |
| `region` | string | | Data-residency hint |
| `settings` | object | | Tenant-wide defaults |

## User
| Field | Type | Req | Notes |
|-------|------|-----|-------|
| `display_name` | string | ✔ | |
| `email` | string | | Login/SSO subject |
| `department_id` | uuid | | |
| `cost_centre_id` | uuid | | |
| `capabilities` | array<string> | | Granted capability keys |
| `state` | enum | ✔ | `INVITED\|ACTIVE\|SUSPENDED\|DEACTIVATED` |

## Identity
An authentication assertion — *not* the User.
| Field | Type | Req | Notes |
|-------|------|-----|-------|
| `user_id` | uuid | ✔ | Subject |
| `device_id` | uuid | | Where asserted (null for API/SSO) |
| `method` | enum | ✔ | `PIN\|RFID\|NFC\|QR\|BLUETOOTH\|FIDO2\|PROXIMITY\|FACE\|SSO\|OIDC\|SAML\|LDAP` |
| `state` | enum | ✔ | `REQUESTED\|AUTHENTICATED\|ACTIVE\|EXPIRED\|REVOKED` |
| `expires_at` | timestamp | | |
| `assurance_level` | enum | ✔ | `LOW\|MEDIUM\|HIGH` (drives policy) |

## Device
Owned by the Organisation, optionally assigned to a User.
| Field | Type | Req | Notes |
|-------|------|-----|-------|
| `vendor_key` | string | ✔ | Open set (`yealink`, `grandstream`, `fanvil`, …) |
| `model` | string | ✔ | |
| `mac` | string | | Normalised, lower hex |
| `assigned_user_id` | uuid | | Null ⇒ shared/hot-desk |
| `firmware` | string | | |
| `network` | object | | VLAN, switch port, IP, location |
| `state` | enum | ✔ | `DETECTED\|PENDING\|APPROVED\|PROVISIONED\|OPERATIONAL\|REPLACING\|RETIRED` |
| `registration` | object | | Last registration state/time |

## Extension
| Field | Type | Req | Notes |
|-------|------|-----|-------|
| `number` | string | ✔ | Unique within Organisation |
| `route_id` | uuid | ✔ | Resolves to a destination |
| `label` | string | | |

## DID
| Field | Type | Req | Notes |
|-------|------|-----|-------|
| `e164` | string | ✔ | Canonical external number |
| `carrier_id` | uuid | ✔ | Inbound carrier |
| `destination_ref` | ref | ✔ | Route/CallFlow/Extension/Queue/User |

## Gateway / Carrier / Trunk
| Entity | Key fields |
|--------|-----------|
| **Carrier** | `name`, `kind` (`PSTN\|MOBILE\|SIP_TRUNK\|INTERNAL`), `rating_profile_id` |
| **Gateway** | `carrier_id`, `kind` (`SIP\|4G\|SIM_BANK`), `address`, `health` (`ONLINE\|OFFLINE`) |
| **Trunk** | `carrier_id`, `auth`, `channels_max`, `codecs[]` |

## Route / CallFlow / IVR / Queue
| Entity | Key fields |
|--------|-----------|
| **Route** | `match` (source/condition), `destination_ref`, `priority` |
| **CallFlow** | `name`, `graph` (nodes+edges), `published_version`, `state` (`DRAFT\|PUBLISHED\|SUPERSEDED`) |
| **IVR** | (node) `prompt_object_id`, `options` (`digit → destination_ref`), `timeout_ms`, `invalid_action` |
| **Queue** | `strategy` (`RINGALL\|LEAST_RECENT\|FEWEST_CALLS\|ROUND_ROBIN\|SKILLS`), `members[]`, `sla_seconds`, `max_wait_ms`, `overflow_ref` |

## Call
| Field | Type | Req | Notes |
|-------|------|-----|-------|
| `direction` | enum | ✔ | `INBOUND\|OUTBOUND\|INTERNAL` |
| `from_ref` / `to_ref` | ref | ✔ | Party references |
| `device_id` | uuid | | Physical endpoint |
| `identity_id` | uuid | | Attributed identity (chargeable legs) |
| `state` | enum | ✔ | `INITIATED\|RINGING\|ANSWERED\|HELD\|ENDED\|FAILED\|NO_ANSWER\|BUSY\|REJECTED` |
| `correlation_id` | uuid | ✔ | Shared by all related events (CONVENTIONS §6) |
| `media` | array<MediaStream> | | |
| `answered_at` / `ended_at` | timestamp | | |
| `hangup_cause` | enum | | Normalised cause code |

## MediaStream
| Field | Type | Notes |
|-------|------|-------|
| `kind` | enum | `AUDIO\|VIDEO\|APPLICATION` |
| `codec` | string | Negotiated codec |
| `direction` | enum | `SENDRECV\|SENDONLY\|RECVONLY\|INACTIVE` |
| `stats` | object | MOS, jitter, loss, latency (Volume 15) |

## Object
| Field | Type | Notes |
|-------|------|-------|
| `kind` | enum | `RECORDING\|VOICEMAIL\|FAX\|FIRMWARE\|TRANSCRIPT\|EXPORT\|DIAGNOSTIC\|WALLPAPER\|OTHER` |
| `uri` | string | Backend-opaque (`local://`, `s3://`, …) |
| `bytes` | int | |
| `sha256` | string | Integrity |
| `retention` | object | Policy + expiry |

## CDR
The billable projection of a Call. See [Volume 10](../010-billing/model.md).
| Field | Type | Notes |
|-------|------|-------|
| `call_id`, `organisation_id`, `cost_centre_id`, `department_id`, `user_id`, `identity_id`, `device_id` | uuid | Attribution chain |
| `extension`, `did`, `carrier_id` | mixed | |
| `duration_ms`, `billable_ms` | int | |
| `cost` | money | `{currency, minor_units}` |
| `codec` | string | |
| `recording_object_id`, `transcript_object_id` | uuid | |
| `tags` | array<string> | |

## Policy / Capability
| Entity | Key fields |
|--------|-----------|
| **Capability** | `key` (`provision.devices`, `billing.export`, …), `description` |
| **Policy** | `subject` (capability/identity/route), `effect` (`ALLOW\|DENY\|REQUIRE_IDENTITY\|REQUIRE_APPROVAL`), `conditions`, `priority` |

## Webhook / Automation / AIJob / Plugin / AuditEntry
| Entity | Key fields |
|--------|-----------|
| **Webhook** | `url`, `event_types[]`, `secret_ref`, `active`, delivery stats |
| **Automation** | `trigger` (event type + filter), `action` (declarative), `enabled` |
| **AIJob** | `kind`, `input_refs[]` (events/objects), `status`, `result_object_id`, `consumer_key` |
| **Plugin** | `name`, `version`, `capabilities[]`, `resource_limits`, `state` |
| **AuditEntry** | `actor_ref`, `action`, `target_ref`, `before`/`after` refs, `at` — append-only |

> Note: field lists here are the frozen *surface*; per-entity JSON Schemas are the
> authority for exact types, patterns, and required arrays.
