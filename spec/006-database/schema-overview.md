# Database — Logical Schema Overview

Companion to [`README.md`](README.md). A concise, **logical** relation-by-relation
listing of the core CommOS entities plus the transactional outbox. This is a
storage view of the entities in
[Volume 2 entities.md](../002-domain-model/entities.md): field *shape* is owned by
the JSON Schemas in [`contracts/`](../../contracts/); this listing owns *relational
concerns* — keys, foreign keys, uniqueness, partitioning, and indexes.

Type classes are portable (`uuid`, `text`, `bigint`, `int`, `bool`, `timestamptz`,
`jsonb`, `enum`, `money{currency,minor_units}`, `ref{kind,id}`), not vendor DDL
(N-6). Every entity relation also carries the **universal columns** from
[README §2](README.md#2-universal-row-model-normative) — `id` (uuid v7, **PK**),
`tenant_id`, `version`, `created_at`, `updated_at`, `deleted_at` — omitted per-row
below except where notable.

Convention in the Notes column: **PK** primary key · **FK→** foreign key ·
**U(...)** unique constraint · **IX(...)** index · **RLS** row-level-security
scoped by `tenant_id`.

---

## organisation
The tenant root; `tenant_id = id`.

| Column | Type | Notes |
|--------|------|-------|
| `name` | text | display name |
| `slug` | text | **U(slug)** globally unique, DNS-safe |
| `default_currency` | text | ISO-4217 |
| `region` | text | data-residency hint |
| `settings` | jsonb | tenant defaults |

Notes: root of the ownership graph (CMOS-06-DB-030); not RLS-restricted to itself
only for the platform-admin role (CMOS-06-DB-023).

## user
| Column | Type | Notes |
|--------|------|-------|
| `department_id` | uuid | **FK→** department |
| `cost_centre_id` | uuid | **FK→** cost_centre |
| `display_name` | text | |
| `email` | text null | **U(tenant_id, email)** where not null |
| `state` | enum | `INVITED\|ACTIVE\|SUSPENDED\|DEACTIVATED` |

Notes: RLS. `capabilities` grants live in a join table `user_capability
(tenant_id, user_id FK, capability_key)`, **U(tenant_id,user_id,capability_key)**.
Deactivation is soft (CMOS-06-DB-050).

## identity
An authentication assertion — **not** the User (CMOS-00-ENG-002).

| Column | Type | Notes |
|--------|------|-------|
| `user_id` | uuid | **FK→** user |
| `device_id` | uuid null | **FK→** device (null for API/SSO) |
| `method` | enum | `PIN\|RFID\|NFC\|QR\|BLUETOOTH\|FIDO2\|PROXIMITY\|FACE\|SSO\|OIDC\|SAML\|LDAP` |
| `assurance_level` | enum | `LOW\|MEDIUM\|HIGH` — drives Policy |
| `state` | enum | `REQUESTED\|AUTHENTICATED\|ACTIVE\|EXPIRED\|REVOKED` |
| `expires_at` | timestamptz null | |

Notes: RLS. **IX(tenant_id, user_id)**, **IX(tenant_id, device_id)**. Secrets
(PIN hashes, FIDO2 keys) live in a separate credential relation, never inline.

## device
Owned by the Organisation, optionally assigned to a User (CMOS-02-DOM-004/015).

| Column | Type | Notes |
|--------|------|-------|
| `vendor_key` | text | open set `^[a-z0-9_.-]+$` |
| `model` | text | |
| `mac` | text null | normalised lower hex; **U(tenant_id, mac)** where not null |
| `assigned_user_id` | uuid null | **FK→** user; null ⇒ shared/hot-desk |
| `firmware` | text | |
| `network` | jsonb | VLAN, switch port, IP, location |
| `state` | enum | `DETECTED\|PENDING\|APPROVED\|PROVISIONED\|OPERATIONAL\|REPLACING\|RETIRED` |

Notes: RLS. Live registration state is **ephemeral** (Redis/NATS-class,
CMOS-06-DB-001); the durable `device` row is checkpointed on
`RegistrationSucceeded`/`RegistrationLost` (Volume 7). **IX(tenant_id, state)**.

## extension
The canonical "unique per tenant, never a principal" relation (CMOS-00-ENG-002).

| Column | Type | Notes |
|--------|------|-------|
| `number` | text | **U(tenant_id, number)** — unique per tenant, NOT global (CMOS-06-DB-031) |
| `route_id` | uuid | **FK→** route |
| `label` | text null | |

Notes: RLS. A label, resolved via Route to a destination — carries no identity.

## did
| Column | Type | Notes |
|--------|------|-------|
| `e164` | text | **U(e164)** — one live tenant at a time (CMOS-06-DB-032) |
| `carrier_id` | uuid | **FK→** carrier |
| `destination_ref` | ref{kind,id} | Route/CallFlow/Extension/Queue/User (CMOS-06-DB-033) |

Notes: RLS. Porting/release retains history via soft-delete; the freed E.164 is a
new row (CMOS-06-DB-051). **IX(tenant_id, carrier_id)**.

## carrier / gateway / trunk
| Relation | Key columns | Notes |
|----------|-------------|-------|
| `carrier` | `name`, `kind` (`PSTN\|MOBILE\|SIP_TRUNK\|INTERNAL`), `rating_profile_id` | RLS |
| `gateway` | `carrier_id` **FK→** carrier, `kind` (`SIP\|4G\|SIM_BANK`), `address`, `health` (`ONLINE\|OFFLINE`) | RLS; health is observed, checkpointed on `GatewayOffline\|GatewayRecovered` |
| `trunk` | `carrier_id` **FK→** carrier, `auth`(ref to secret), `channels_max` int, `codecs` jsonb | RLS; a mobile/4G gateway is just-another-trunk (Volume 7) |

## route / call_flow / ivr / queue
| Relation | Key columns | Notes |
|----------|-------------|-------|
| `route` | `match` jsonb, `destination_ref` ref, `priority` int | RLS; **IX(tenant_id, priority)** |
| `call_flow` | `name`, `graph` jsonb (nodes+edges), `published_version` bigint, `state` (`DRAFT\|PUBLISHED\|SUPERSEDED`) | RLS; publish creates an immutable version row, never overwrites (CMOS-06-DB-052) |
| `call_flow_version` | `call_flow_id` **FK→**, `version` bigint, `graph` jsonb, `published_at` | **U(tenant_id, call_flow_id, version)**; append-only history for Time Machine |
| `ivr` | node within `call_flow.graph` (not a top-level table) | `prompt_object_id` **FK→** object |
| `queue` | `strategy` enum, `members` jsonb, `sla_seconds` int, `max_wait_ms` int, `overflow_ref` ref | RLS |

## call
| Column | Type | Notes |
|--------|------|-------|
| `direction` | enum | `INBOUND\|OUTBOUND\|INTERNAL` |
| `from_ref` / `to_ref` | ref{kind,id} | party references |
| `device_id` | uuid null | **FK→** device |
| `identity_id` | uuid null | **FK→** identity — REQUIRED on chargeable legs (CMOS-02-DOM-010) |
| `state` | enum | `INITIATED\|RINGING\|ANSWERED\|HELD\|ENDED\|FAILED\|NO_ANSWER\|BUSY\|REJECTED` |
| `correlation_id` | uuid | shared by all related events (CMOS-05-EVT-020) |
| `answered_at` / `ended_at` | timestamptz null | |
| `hangup_cause` | enum null | normalised cause code |

Notes: RLS. **IX(tenant_id, correlation_id)**, **IX(tenant_id, created_at)**.
In-flight call state is an in-memory actor (CMOS-06-DB-082); rows are checkpointed
at each transition. High volume — a candidate for time partitioning if retained
long-term, though the durable billable record is the `cdr`.

## media_stream
| Column | Type | Notes |
|--------|------|-------|
| `call_id` | uuid | **FK→** call |
| `kind` | enum | `AUDIO\|VIDEO\|APPLICATION` |
| `codec` | text | negotiated codec |
| `direction` | enum | `SENDRECV\|SENDONLY\|RECVONLY\|INACTIVE` |
| `stats` | jsonb | MOS, jitter, loss, latency (Volume 15) |

Notes: RLS via `call`. **IX(tenant_id, call_id)**.

## object
Metadata only — the artifact lives in Object Storage (CMOS-02-DOM-013).

| Column | Type | Notes |
|--------|------|-------|
| `kind` | enum | `RECORDING\|VOICEMAIL\|FAX\|FIRMWARE\|TRANSCRIPT\|EXPORT\|DIAGNOSTIC\|WALLPAPER\|OTHER` |
| `uri` | text | backend-opaque (`local://`, `s3://`, …) — never a blob column |
| `bytes` | bigint | |
| `sha256` | text | integrity |
| `retention` | jsonb | policy + expiry (CMOS-06-DB-053) |

Notes: RLS. `recording`/`voicemail` entities hold a `object_id` **FK→** object.

## cdr
The billable projection of a Call (Volume 10). Retention- and partition-governed.

| Column | Type | Notes |
|--------|------|-------|
| `call_id` | uuid | **FK→** call |
| `cost_centre_id`,`department_id`,`user_id`,`identity_id`,`device_id` | uuid null | attribution chain (CMOS-02-DOM-014) |
| `extension`,`did` | text null | denormalised labels at time of call |
| `carrier_id` | uuid null | **FK→** carrier |
| `duration_ms`,`billable_ms` | bigint | |
| `cost` | money | `{currency, minor_units}` (CMOS-06-DB-035) |
| `codec` | text | |
| `recording_object_id`,`transcript_object_id` | uuid null | **FK→** object |
| `tags` | jsonb | |

Notes: RLS. **Range-partitioned by `created_at`** (monthly), optionally
sub-partitioned by `tenant_id` (CMOS-06-DB-040/041). **IX(tenant_id, created_at)**,
attribution indexes on `user_id`/`department_id`/`cost_centre_id`. Append-only in
practice; corrections are new rating rows, not overwrites.

## audit_entry
Append-only record of security-relevant actions (CMOS-00-ENG-012).

| Column | Type | Notes |
|--------|------|-------|
| `actor_ref` | ref{kind,id} | who acted (User/Identity/system/plugin) |
| `action` | text | verb |
| `target_ref` | ref{kind,id} | what was acted on |
| `before_ref` / `after_ref` | ref | state snapshots/refs |
| `at` | timestamptz | |

Notes: RLS. **Strictly append-only** — no UPDATE/DELETE except audited partition
retention (CMOS-06-DB-054). **Range-partitioned by `at`** (CMOS-06-DB-040).

## event_outbox
Backs at-least-once event delivery (CMOS-06-DB-060, CMOS-03-ARCH-030).

| Column | Type | Notes |
|--------|------|-------|
| `id` | uuid | **PK** — also the event envelope `id` |
| `tenant_id` | uuid | RLS |
| `type` | text | event type, PascalCase |
| `source` | text | emitting subsystem URI |
| `subject` | uuid/text | primary entity id |
| `correlation_id` | uuid | intra-operation ordering key |
| `causation_id` | uuid null | causing event/command id |
| `idempotency_key` | text | **U(tenant_id, idempotency_key)** (CMOS-06-DB-062) |
| `sequence` | bigint | per-`correlation_id` monotonic (CMOS-06-DB-063) |
| `specversion` | text | event-spec version |
| `traceparent` | text null | W3C trace context |
| `time` | timestamptz | when the fact occurred |
| `data` | jsonb | event payload (validates vs. event schema) |
| `status` | enum | `PENDING\|PUBLISHED\|DEAD` |
| `attempts` | int | retry count |
| `next_attempt_at` | timestamptz | backoff schedule |

Notes: written in the *same transaction* as the state change it records
(CMOS-06-DB-060). Relay claims `PENDING` rows with `SKIP LOCKED`
(CMOS-06-DB-064). **IX(status, next_attempt_at)** for the poller.
**Range-partitioned by `time`** for archival retention (CMOS-06-DB-065); doubles
as a replayable local event log.

## schema_migrations
Forward-only migration ledger (CMOS-06-DB-070).

| Column | Type | Notes |
|--------|------|-------|
| `version` | text | **PK/U** — ordered, immutable id |
| `checksum` | text | detects edited/replayed migrations |
| `applied_at` | timestamptz | |

Notes: not tenant-scoped (platform-global). Append-only; a recorded row is never
edited or removed (CMOS-06-DB-070/072).

---

> Note (informative): join tables (`user_capability`), credential/secret stores,
> and read-model projections are named where load-bearing but their full shape is
> implementation detail. This overview lists what constraints and partitioning a
> conforming store MUST honour, not the exact DDL a given engine emits.
