# CommOS Architecture — a contributor's onboarding read

This is the human-facing companion to [`/CLAUDE.md`](../CLAUDE.md). `CLAUDE.md` is the lean,
authoritative orientation (subsystem → files, path conventions, gotchas); this document is the
slower 10-minute read that explains *how the pieces fit* and *where to look* when you pick up a
task. Where the two overlap, `CLAUDE.md` and the specs win.

---

## 1. Overview

CommOS is a single-binary, **pure-Rust SIP/PBX + HTTP API** platform (`commosd`). It runs the
control plane, a loopback of the media plane (RTP/SIP), and an HTTP API in one process, on a small
box (Raspberry Pi 4 and up) with **zero external dependencies by default**: an embedded SQLite
system-of-record and a local filesystem object store. Configuration is code-as-data via
`pbx.yaml`, and secrets are always *referenced*, never inlined.

Design posture — the whole tree is shaped by "small durable appliance":

- **Pure Rust, no C/OpenSSL/native codec libs**, so the primary artifact cross-compiles cleanly to
  arm64 (Pi first) and amd64 with parity. See `reference/Cargo.toml` `[workspace.dependencies]`.
- **Tiny artifact**: `[profile.release]` sets `opt-level = "z"`, `lto = "thin"`,
  `codegen-units = 1`, `strip = true`, `panic = "abort"`.
- **SD-card-kind persistence**: SQLite in WAL mode with `synchronous = NORMAL`, and deliberately
  ephemeral state (registrations, presence, agent state) kept *out* of the durable store.
- **Everything hangs off `data_dir`** (default `.`; installer sets e.g. `/var/lib/commos`). Never
  resolve runtime paths relative to the cwd — see the path convention in `CLAUDE.md`.
- **Operable under systemd**: clean start/stop, graceful drain on SIGTERM, `sysexits.h` exit-code
  contract (`main.rs::exit`).

The workspace (`reference/Cargo.toml`) has two crates: **`commos-core`** (the domain model) and
**`commosd`** (the daemon).

---

## 2. The layer model

```
                        HTTP clients                     SIP phones / carriers
                             │                                    │
                    ┌────────▼─────────┐                 ┌────────▼─────────┐
      commosd  ──▶  │   api/  (HTTP)   │                 │  sip/ (media plane)│
                    └────────┬─────────┘                 └────────┬─────────┘
                             │        request → command           │  media facts
                    ┌────────▼────────────────────────────────────▼─────────┐
                    │                control/  (services)                    │
                    │   routing · voicemail · ringplan/resolve/ring ·        │
                    │   trunking · onboarding · provisioning · queue · …     │
                    └────────┬──────────────────────────────┬────────────────┘
                             │ Tx (entities + events)        │ publish
                    ┌────────▼─────────┐            ┌────────▼─────────┐
                    │  store/ (SQLite/ │            │  bus.rs (Event   │
                    │  Postgres/mem)   │──outbox──▶ │  Bus, in-proc)   │
                    └──────────────────┘  relay.rs  └──────────────────┘

              commos-core  =  entities/ (domain state)  +  events/ (domain facts)
                             (the vocabulary every layer above speaks)
```

| Layer | Crate/dir | Responsibility | What belongs here |
|-------|-----------|----------------|-------------------|
| **Domain** | `commos-core` (`entities/`, `events/`, `common.rs`, `event.rs`) | The vocabulary: entity structs (Call, Extension, CallFlow, Voicemail, …) and event payloads (CallStarted, CallFlowPublished, …). No I/O, no daemon deps. | Pure data types + their invariants. Nothing that touches sockets, the store, or config. |
| **Media plane** | `commosd/src/sip/` | Terminate SIP/RTP; be a B2BUA; turn signalling into **media facts** for the control plane. | Wire protocol handling (SIP parsing, RTP relay, SRTP, DTMF, codecs, IVR/voicemail media). |
| **Control** | `commosd/src/control/` | Stateless services that own business rules; each mutation is one atomic `Tx` (entities + events) against the store. | Routing decisions, provisioning, trunking, voicemail/recording indexing, ring planning, webhooks. |
| **API** | `commosd/src/api/` | The HTTP gateway: authn/z, request → command, JSON in/out under `/v1`. | Axum handlers that validate input and delegate to a `control/` service. No business logic of its own. |
| **Persistence** | `commosd/src/store/` | System of record + transactional outbox behind the `Store` trait. | SQL/storage bindings only. Callers never branch on which binding is live. |

Cross-cutting plumbing (wired once in `main.rs::run`):

| Piece | File | Role |
|-------|------|------|
| `AppState` | `state.rs` | Cheap-to-clone bundle of shared handles (store + every control service + bus) that every HTTP handler receives. Handlers are stateless. |
| Event Bus | `bus.rs` | In-process broadcast of committed events (`EventBus`); pluggable for NATS/Redis/Kafka in a cluster. |
| Outbox relay | `relay.rs` | Background worker draining the outbox to the bus at-least-once; `RelaySignal::wake()` nudges it after each commit. |
| Config | `config.rs` | Loads/validates `pbx.yaml`; enforces "secrets referenced, never inline" and resolves `SecretRef`s. |
| Media↔control seam | `media.rs` | `MediaCommand` / `MediaFact` / `MediaPlane` typed boundary between `sip/` and `control/routing`. |
| Misc | `net.rs`, `objectstore.rs`, `metrics.rs`, `introspect.rs`, `telemetry.rs` | LAN IP detection, blob store (local/S3), Prometheus metrics, recent-event ring, tracing setup. |

**The rule for what goes where:** state changes and business rules live in `control/`; `api/` and
`sip/` are edges that translate a protocol into a control-plane command and translate the result
back out. If you find yourself writing a `Tx` or an entity mutation inside `api/` or `sip/`, it
probably belongs in a `control/` service.

---

## 3. Request lifecycles

### 3a. Inbound INVITE from a softphone

1. **UDP ingress.** `sip/server/mod.rs::SipServer::run` owns the receive loop. Each datagram is
   copied and handed to a detached `tokio::spawn(handle(...))` — never processed inline, because
   `on_invite` blocks for up to `no_answer_timeout` while ringing the callee, and inlining would
   serialize all call setup.
2. **Dispatch.** `handle` parses the message (`sip/message`) and dispatches by method. `INVITE`
   goes to `on_invite` (in `sip/server/invite.rs`).
3. **Dialog check + auth.** `on_invite` first checks for an existing dialog (a retransmit or a
   hold/resume re-INVITE — the latter drives music-on-hold via `rtp::Bridge::set_hold`), then
   applies the digest-auth gate (`sip/server/auth.rs`).
4. **Create the Call + ring fact.** It calls `routing.create_inbound_call(...)` (control plane),
   then reports `MediaFact::Rang` via `routing.apply_fact(...)`. Facts flow through the media-fact
   loop spawned in `main.rs::run`, which calls `routing.apply_fact` to advance Call state and emit
   `CallRinging`/`CallAnswered`/`CallEnded` events.
5. **Destination resolution** (`sip/server/routing.rs`): `resolve_route` (Extension→Route table
   via `routing.resolve_extension`), `resolve_did` (inbound DID → `destination_ref`),
   `resolve_ivr_target` (`ivr:<uuid>`), plus in-SIP feature codes for `*97`/`*98` voicemail
   retrieval. The resolved target selects one of the outcomes below.
6. **Outcome** (all under `sip/server/`):
   - **Bridge** to a registered endpoint: `try_bridge` (`bridge.rs`) returns a `BridgeOutcome`
     (`Answered` / `Declined` / `NoAnswer`); ring groups / follow-me go through
     `execute_ring_plan` → `fork_bridge` (parallel INVITEs, first 2xx wins, losers CANCELled).
   - **Decline handling** (`decline.rs`): a `486`/`600`/`603` is treated per the `on_decline`
     config (`announce` / `voicemail` / `busy`), distinct from a plain no-answer.
   - **IVR** (`ivr_menu.rs`): prompt playout + DTMF collect.
   - **Queue** (`queue.rs`): answer early, then `queue_wait_driver` (greeting + MoH + member ring).
   - **Voicemail** (`voicemail.rs`): no-answer/offline callee → `voicemail_deposit_driver`; stored
     on hangup and an MWI NOTIFY pushed to the phone.
   - **Echo** fallback when nothing else matches (voicemail disabled).
7. **Media plane.** The chosen path sets up RTP: a two-leg `rtp::Bridge`, a single-socket echo, or
   an IVR/voicemail media task. SDP is negotiated in `sip/server/sdp.rs` (`media_sdp`/`reoffer_sdp`,
   SRTP via `sdes`/`srtp` when offered).
8. **CDR.** On BYE/CANCEL (`sip/server/handlers.rs::on_bye`) the Call is hung up in the control
   plane, which produces the CDR and emits `CallEnded`; the outbox relay surfaces it on the bus.

### 3b. An HTTP API request — publishing a CallFlow

`POST /v1/call-flows/{id}/publish` (versioned routing programs):

1. **Handler** — `api/call_flows.rs` (mounted by `api/mod.rs::router`). The `AdminContext` /
   `TenantContext` extractors authenticate and scope the request; the handler validates the id and
   calls the service.
2. **Service** — `control/callflow.rs::CallFlowService::publish`. It snapshots the draft `graph`
   into an immutable `CallFlowRevision`, mutates the `CallFlow`, and builds a `CallFlowPublished`
   event — all in **one** `store::Tx` (the mutated entity, the new revision, and the event land
   atomically).
3. **Store** — `store::Store::commit(Tx { ... })` (binding: `store/sqlite.rs`, `postgres.rs`, or
   `mem.rs`). The event is written to the `outbox` table in the *same* transaction as the state
   change, then `RelaySignal::wake()` nudges the relay.
4. **Bus** — `relay.rs::run` drains the outbox and `EventBus::publish`es the envelope; subscribers
   (metrics, webhooks dispatcher, SSE introspection) see it. Errors map to RFC-7807 `Problem`
   responses via `api/problem.rs`.

This same shape — `api/<handler>` → `control/<service>` (one `Tx`) → `store` → outbox → bus — is
the spine every write follows; `control/routing.rs` is the canonical example.

---

## 4. Feature map (vertical slices)

Each row is a feature; each cell names the real module(s) so you can see its slice top-to-bottom.

| Feature | core (entity / event) | sip/ (media) | control/ | api/ | store |
|---------|----------------------|--------------|----------|------|-------|
| **Calls / routing** | `call`, `cdr` / `call_started`, `call_answered`, `call_ended` | `sip/server/invite.rs`, `bridge.rs` | `control/routing.rs`, `dialplan.rs` | `api/calls.rs`, `cdrs.rs` | `calls`, `cdrs` tables |
| **Registration / auth** | `device` / `device_detected` | `sip/server/handlers.rs` (REGISTER), `auth.rs`, `sip/digest.rs` | `control/registrations.rs` (in-memory) | `api/registrations.rs`, `api/auth.rs` | `sip_credentials` (durable creds); registry is ephemeral |
| **Voicemail** | `voicemail` / `voicemail_received` | `sip/server/voicemail.rs` (deposit, `*97`/`*98`, MWI) | `control/voicemail.rs`, `voicemail_email.rs`, `smtp.rs` | `api/voicemail.rs` | `voicemails`, `objects` |
| **Ring groups / follow-me** | `ring_group`, `forwarding` | `sip/server/bridge.rs` (`fork_bridge`, `execute_ring_plan`) | `control/ringplan.rs`, `ringresolve.rs`, `ringing.rs` | `api/ringing.rs` | `ring_groups`, `forwardings` |
| **IVR** | `ivr` / — | `sip/server/ivr_menu.rs`, `sip/ivr.rs` | `control/ivr.rs` | `api/ivrs.rs` | `ivrs`, `objects` (prompts) |
| **Queues** | `queue` / `agent_state_changed` | `sip/server/queue.rs`, `sip/queuewait.rs`, `sip/moh.rs` | `control/queue.rs`, `agents.rs` | `api/queues.rs`, `agents.rs` | `queues` |
| **CallFlow routing** | `call_flow`, `CallFlowRevision` / `call_flow_published` | — (executed via routing targets) | `control/callflow.rs` | `api/call_flows.rs` | `call_flows`, `call_flow_revisions` |
| **Trunking / outbound** | `carrier`, `gateway`, `trunk`, `did` | `sip/server/routing.rs` (gateway select), `bridge.rs` (trunk INVITE) | `control/trunking.rs`, `dialplan.rs`, `policy.rs` | `api/trunking.rs` | `carriers`, `gateways`, `trunks`, `dids` |
| **Provisioning** | `device`, `extension`, `user`, `route` | — | `control/provisioning.rs`, `onboarding.rs` | `api/provision.rs`, `directory.rs`, `onboarding.rs` | `devices`, `extensions`, `users`, `routes` |
| **Recording** | `recording` / `recording_uploaded` | `sip/rtp.rs` (`Capture`), `sip/server/bridge.rs` | `control/recordings.rs`, `objects.rs` | `api/recordings.rs` | `recordings`, `objects` |
| **Messaging / presence** | `channel`, `thread`, `message`, `presence_state` | — | `control/messaging.rs`, `realtime.rs` | `api/channels.rs`, `threads.rs`, `messages.rs`, `presence.rs` | `channels`, `threads`, `messages`, `presence` |
| **Webhooks** | `webhook` / `webhook_delivered` | — | `control/webhooks.rs`, `webhook_delivery.rs` | `api/webhooks.rs` | `webhooks` |

(The `sip/server/` cells name topic submodules, not individual line ranges — that subsystem is
being reorganized; treat it at the module/behavior level.)

---

## 5. "Where does X live?" conventions

- **Adding a feature threads through the layers in one direction.** Add/adjust the entity (and any
  new event) in `commos-core`; add a `store` table + `Tx` field if it needs persistence; put the
  business rule in a `control/` service that commits one atomic `Tx`; expose it via an `api/`
  handler and/or wire it into the `sip/` media plane. Wire the new service handle once in
  `main.rs::run` and add it to `AppState` (`state.rs`) if handlers need it.
- **The store is a document store with optimistic concurrency.** Every entity table is
  `(id, tenant_id, version, created_at, updated_at, data TEXT)` — `data` is the entity's contract
  JSON; `version` is the optimistic-concurrency column (a stale write → `StoreError::VersionConflict`,
  surfaced as HTTP `409`). See `store/sqlite.rs::SCHEMA`. Never branch on the binding — code to the
  `Store` trait (`store/mod.rs`).
- **No state change without its event.** Mutations and their events commit together to the outbox
  in one `Tx`; the relay delivers at-least-once. Emit events from the `control/` service, not the
  edges.
- **`data_dir` path convention.** All runtime state hangs off `data_dir` (resolved with the
  `data_dir.trim_end_matches('/')` idiom), never the cwd: `{data_dir}/commos.db`,
  `{data_dir}/objects`, `{data_dir}/secrets/jwt.key`, `{data_dir}/sounds`, `{data_dir}/moh`,
  `{data_dir}/display_name.txt`. The config file itself is found via `main.rs::default_config_path`.
- **Secrets referenced, never inline** (CMOS-14-DEP-083). Config carries `SecretRef`s resolved at
  boot (`config.rs`); a bad reference fails startup rather than shipping an insecure default.
  Never log a resolved DSN/secret (see `main.rs::describe_storage`).
- **File-header convention (please follow it).** Every module starts with a `//!` doc comment that
  states **what the file owns · who calls it · what must NOT go here**. This is the fastest way for
  the next contributor to orient, and it keeps responsibilities from drifting across the edge/
  service boundary. `sip/server/mod.rs` is a good exemplar (it documents the struct/dispatch it
  owns and delegates each concern to a named submodule with its own header); `store/mod.rs` and
  `control/callflow.rs` are others.

---

## 6. Pointers (deeper / normative detail)

- [`/CLAUDE.md`](../CLAUDE.md) — the lean, always-current orientation: subsystem → files, path
  conventions, and the history-of-bugs gotchas (Via headers, media_ip, voicemail-in-rings). Read
  it alongside this file.
- [`/spec/`](../spec/) — the **normative** volumes `000`–`019` (philosophy, PRD, domain model,
  architecture, API, events, database, communications, provisioning, security, billing, …). Code
  must conform; `019-adrs/` records the decisions (e.g. ADR-0012, embedded SQLite default).
- [`/contracts/`](../contracts/) — frozen API/event/entity contracts (`openapi/`, `json-schema/`).
  The `data TEXT` column stores exactly these entity shapes.
- [`/conformance/`](../conformance/) — scenarios the implementation must pass (`run.py`,
  `scenarios/`).
- `reference/deploy/` and `reference/scripts/` — `pbx.example.yaml`, systemd unit, and
  `install.sh` / `smoke.sh` for how it actually gets deployed and smoke-tested.
