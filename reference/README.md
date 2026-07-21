# CommOS reference implementation

This is the **reference implementation** of CommOS — the first real code behind the
[specification suite](../spec/). It realises the **single self-contained binary**
(`commosd`) that the spec makes the primary deliverable (CMOS-14-DEP-001/010): one process
running the control plane and media plane, with PostgreSQL as its only intended hard
dependency at scale.

> **Status: first vertical slice.** The frozen contract spine (Volumes 0/2/3/4/5) is
> implemented end-to-end for the *originate-a-call* flow, chosen because it exercises every
> load-bearing invariant at once: the domain model, the event envelope, the transactional
> outbox, the two-plane split, tenant isolation, and config-as-code. Breadth (the other 35
> entities, 74 events, 91 API paths) is added by extending the same shapes — the hard
> architecture is in place.

**The design priority is fidelity to the contract, not feature count.** Conformance in
CommOS is defined against contracts, not code (Volume 3), so every type here is a faithful
projection of a frozen schema, and the runtime output is validated back against those
schemas (see [Conformance evidence](#conformance-evidence)).

CommOS is a **communications substrate** on which *voice is one workload* — not a PBX with
extras. The code reflects that: `Call` is modelled as "one workload instance", and the same
Identity / Routing / Event / Media spine it rides on is what will carry messaging, video,
presence, and AI agents without a redesign.

## Layout

| Path | What it is |
|------|-----------|
| `crates/commos-core` | Rust projection of the frozen contracts (`contracts/json-schema/*`): primitives, the CloudEvents envelope, entities (Call + the messaging workload: Channel/Thread/Message), and event payloads (the full Call lifecycle). Constraints are enforced at the type boundary. |
| `crates/commosd` | The single binary: API Gateway (Axum), control-plane services, transactional outbox + Event Bus, the typed control→media boundary, config-as-code, graceful drain. The `Store` has two bindings — in-memory (zero-dependency) and **PostgreSQL** (durable system of record). |
| `build/targets.toml` · `build/build.sh` | The architecture registry and a parametric cross-build. Raspberry Pi 4 (arm64) is the primary target; amd64 and armv7 are one row each. |
| `deploy/` | Reference `systemd` unit, example `pbx.yaml`, and a Docker Compose for PostgreSQL. |
| `scripts/smoke.sh` · `docs/postgres.md` | End-to-end smoke test and the operator guide for running against PostgreSQL. |

## How the code maps to the spec

| Spec requirement | Where |
|------------------|-------|
| Single self-contained binary, zero external deps by default (CMOS-14-DEP-001/021, ADR-0012) | `commosd` defaults to embedded SQLite (`store/sqlite.rs`) — durable with no server; PostgreSQL for multi-node |
| System of record behind one interface (CMOS-14-DEP-020/042) | `store/{sqlite,postgres,mem}.rs` — drop-in bindings; entities stored as contract JSON with typed id/version columns |
| Two planes over a typed interface, never shared memory (CMOS-03-ARCH-001/002) | `media.rs` — `MediaPlane` trait + `MediaCommand`/`MediaAck`; splittable to a media node unchanged |
| Stateless control plane; all state in the store (CMOS-03-ARCH-010) | `store/` — `Store` trait; handlers hold only shared handles |
| Transactional outbox: no state change without its event (CMOS-03-ARCH-030 / CMOS-05-EVT-010) | `store/*` `commit()` writes entity + event in one DB transaction; `relay.rs` drains at-least-once |
| Same source, any topology/binding, no caller change (CMOS-14-DEP-042) | in-memory and PostgreSQL `Store` bindings are drop-in; Routing/API/relay are identical |
| Canonical event envelope (Volume 5) | `commos-core::event::Envelope` — derives `type`/`source`/`subject`, all required fields |
| Domain entity + state machine (Volume 2) | `commos-core::entities::call` — enforced legal transitions, versioned twin |
| Tenant isolation, defence in depth (CMOS-03-ARCH-050) | every `Store` read is tenant-scoped; `TenantContext` auth extractor |
| Bearer auth, Problem Details, idempotency, pagination (Volume 4) | `api/auth.rs`, `api/problem.rs`, `api/calls.rs` |
| Config-as-code, secrets never in YAML (CMOS-14-DEP-080/083) | `config.rs` — `pbx.yaml`, inline-secret rejection |
| Readiness gating + graceful drain (CMOS-14-DEP-033/051) | `/readyz`, SIGTERM drain, final outbox flush |
| arm64 + amd64 parity (CMOS-14-DEP-004/060) | `build/` — same source, per-target artifact |

## Build

```bash
cd reference
./build/build.sh          # primary target: Raspberry Pi 4 (arm64)
./build/build.sh amd64    # or a named target
./build/build.sh all      # every registered architecture
./build/build.sh --list
```

Cross toolchain for the arm64 (Raspberry Pi 4) build on a Debian/Ubuntu host:

```bash
rustup target add aarch64-unknown-linux-gnu
sudo apt-get install -y gcc-aarch64-linux-gnu libc6-dev-arm64-cross
```

The produced `target/aarch64-unknown-linux-gnu/release/commosd` is a stripped ~1.7 MB
`ARM aarch64` ELF that runs on 64-bit Raspberry Pi OS / Ubuntu. Rust makes portability a
property of building right, so the same source yields every other target with no code
change — the architecture is not baked into the implementation.

## Run

### Set-up wizard (rapid onboarding)

Open **`http://localhost:8080/onboarding`**. Answer two questions — *what kind of place* (office /
hotel / hospital / home) and *how many phones* — and CommOS auto-detects and proposes the rest:

- **Extension plan** — a suggested starting series (100 / 200 / … with a dropdown) whose digit
  length scales to the fleet, environment-aware service numbers (reception **9** in an office,
  **0** at a hotel front desk), and default feature codes.
- **Network** — detects the host's IP, proposes the LAN subnet and a phone DHCP pool, and warns
  when the fleet won't fit (e.g. 300 phones → "use a /23 or a voice VLAN").
- **Discovered phones** — reads the ARP table and flags likely IP phones by MAC vendor.
- **Auto-provisioning** — generates the exact **DNS (BIND A + SRV)** and **DHCP (option 66/67)**
  lines to paste, so phones provision themselves.

One **"Create it"** button then applies the choice: `POST /v1/onboarding/apply` mints the people
and extensions in a single transaction (and binds any discovered phones so they auto-provision).
The created directory is browsable at `/v1/{users,extensions,devices}`, exportable as a
Git-reviewable `pbx.yaml` (`GET /v1/config`), and re-importable (`POST /v1/config`). A phone then
fetches its own config from **`GET /provision/{mac}.cfg`** (the DHCP-option-66 target) and registers.

The philosophy: **good defaults everywhere; the operator confirms rather than fills in forms.**

### Default — embedded SQLite (durable, zero external dependency)

Just run it. With no `database_url` configured, the binary opens/creates a local SQLite
database (WAL mode) — durable across restarts with **no server to install** (ADR-0012).
This is the right default for a single box / Raspberry Pi.

```bash
./target/debug/commosd                            # creates ./commos.db, boots on :8080

TENANT=01920000-0000-7000-8000-000000000001
AUTH="Authorization: Bearer tenant:$TENANT"       # dev token; JWT verification is Volume 9 work
curl -s localhost:8080/info
curl -s -X POST localhost:8080/v1/calls -H "$AUTH" -H 'content-type: application/json' \
     -d '{"direction":"OUTBOUND","from_ref":"sip:100","to_ref":"+14155550100"}'
curl -s localhost:8080/_introspect/events         # watch the lifecycle flow through the outbox
```

Then open **`http://localhost:8080/dashboard`** — a self-contained live view of the
platform (workloads, calls, and the event stream).

### PostgreSQL (multi-node / HA)

```bash
docker compose -f deploy/docker-compose.yml up -d postgres
export DATABASE_URL="postgres://commos:commos-dev-password@localhost:5432/commos"
# pbx.yaml references the secret, never inlines it (CMOS-14-DEP-083):
#   database_url: { ref_uri: "env://DATABASE_URL" }
./target/debug/commosd --config deploy/pbx.example.yaml   # migrations run at boot
bash scripts/smoke.sh
```

PostgreSQL is the system of record when multiple stateless control-plane nodes share one
database (CMOS-14-DEP-011/030). See [`docs/postgres.md`](docs/postgres.md). For an
ephemeral in-process store (tests), set `database_url` to `memory://`.

### Endpoints

- `GET /livez`, `GET /readyz`, `GET /info` — operational signals (unauthenticated).
- `GET /dashboard` — live operations dashboard; `GET /onboarding` — setup wizard (both unauthenticated, self-contained HTML).
- `GET /v1/onboarding/environments`, `GET /v1/onboarding/suggest`, `POST /v1/onboarding/apply` — the wizard's detect-and-apply API.
- `GET|POST /v1/config` — export/import the `pbx.yaml`; `GET /provision/{mac}.cfg` — phone auto-provisioning (unauthenticated).
- `GET /v1/{users,extensions,devices}[/{id}]` — the provisioning directory.
- **Voice workload** — `GET|POST /v1/calls`, `GET|PATCH /v1/calls/{id}` (`PATCH` is an
  RFC 7386 merge-patch with `If-Match` optimistic concurrency), and actions
  `POST /v1/calls/{id}/{hold,resume,hangup,transfer}`. A Call starts `INITIATED`; ring and
  answer arrive **asynchronously as media facts** (the media plane reports them; the control
  plane applies them), so a fresh call reaches `ANSWERED` a moment after creation.
- **Messaging workload** — `GET|POST /v1/channels`, `/v1/threads`, `/v1/messages` and
  their `/{id}` reads.
- **Real-time workloads** — `GET|POST /v1/video-rooms`, `GET|POST /v1/presence` and their
  `/{id}` reads. Same substrate, same store, same outbox — voice is one workload of many.
- **Registrations** — `GET|POST /v1/registrations`, `GET|DELETE /v1/registrations/{id}`.
  Device registrations are **ephemeral in-memory** state (not the durable store), so a
  re-REGISTER storm never touches disk — SD cards last (CMOS-14-DEP-021). A real softphone
  can also register over **SIP/UDP** (see below).
- **Billing** — `GET /v1/cdrs`, `GET /v1/cdrs/{id}`. A CDR + `BillingGenerated` event are
  produced atomically when a Call ends; cost comes from a destination-aware **rating engine**
  (E.164 longest-prefix table, per-minute rounding — Volume 10, the `Rating` interface).
- **Contact-centre** — `GET|POST /v1/queues`, `GET /v1/queues/{id}`; `GET|POST /v1/agents`
  (agent state), and `POST /v1/queues/{id}/enqueue` which assigns a Call to an available
  agent (basic ACD) and emits `AgentStateChanged`.
- `GET /_introspect/events[/stream]` — **non-normative** view of the event bus for bring-up; not part of the contract.

### SIP signalling (Volume 7)

`commosd` listens for **SIP over UDP** (default `0.0.0.0:5060`, `sip_listen: null` disables;
`media_ip` sets the address advertised in SDP). A real softphone can **register and place a
call** end-to-end:

- **REGISTER** binds the AoR → appears in `/v1/registrations` and the dashboard.
- **INVITE** creates an inbound `Call`, drives it to `ANSWERED`, sets up an **RTP echo** path,
  and answers `200 OK` with an SDP body — the caller hears themselves (classic echo test).
- **BYE/CANCEL** hangs the Call up (producing its **CDR**) and tears down the RTP.

An INVITE aimed at a **registered** endpoint is bridged B2BUA-style: CommOS places an outbound
INVITE to the callee, then relays RTP between the two legs (symmetric-RTP latching) so two
softphones can talk; an INVITE to an external/unregistered number falls back to the echo test.
Verified: `INVITE → 100 Trying → 200 OK (SDP)`, RTP echoed back, `BYE → 200 OK`, Call `ENDED` + CDR;
the two-leg bridge relays A↔B in a unit test. Full mid-dialog B2BUA correctness (transactions,
re-INVITE/hold, transcoding, conferencing) are the next media steps.

Media is **encrypted with SRTP** (`AES_CM_128_HMAC_SHA1_80`, RFC 3711) whenever a phone offers the
secure `RTP/SAVP` profile with an SDES key (`a=crypto`, RFC 4568): on the endpoint paths CommOS
terminates (echo test, voicemail), and across the **two-leg bridge/trunk relay**, where SRTP is
terminated independently per leg — CommOS decrypts the caller leg and re-encrypts for the callee
leg, so the two legs never share key material and the media is only plaintext inside CommOS. A
plain-RTP caller is answered in the clear exactly as before. The crypto is pure-Rust (RustCrypto),
validated against the RFC 3711 key-derivation vectors and cross-checked end-to-end (endpoint *and*
two-phone bridge) by an independent SRTP implementation.

The signalling channel itself can run over **SIP-over-TLS** (SIPS) — build with `--features tls`,
set `sips_listen` plus `sip_tls_cert`/`sip_tls_key`, and CommOS serves the same request handlers
over a TLS stream (rustls, ring provider), re-framing messages by `Content-Length` and replying on
the same connection. This encrypts every header and the SDES SRTP keys against a passive observer.
TLS stays behind a feature so the default binary is pure-Rust and cross-compiles clean, exactly
like `s3`. Two-leg bridge/trunk SRTP, and *outbound* TLS on trunk legs, come next.

All `/v1` routes are bearer-authenticated and tenant-scoped. Auth verifies **HS256 JWTs** when a
`jwt_secret` is configured (tenant from the `tenant_id` claim); with none configured it accepts the
`tenant:<uuidv7>` dev token, so local dev and the dashboards work with zero setup.

## Conformance evidence

- **Unit + contract tests:** `cargo test` (core primitives match the schema patterns; the
  Call state machine rejects illegal transitions; both `Store` bindings enforce tenant
  scoping, optimistic concurrency, and atomic outbox commit).
- **Runtime-event conformance:** the `CallStarted` envelope emitted at runtime validates
  against the **frozen** `contracts/json-schema/events/CallStarted.schema.json` (envelope +
  common defs) using the same `referencing` registry the spec's harness uses.
- **Live PostgreSQL:** `commosd` boots against PostgreSQL, runs migrations, and `smoke.sh`
  passes end-to-end; state **survives a process restart**, the idempotency key returns the
  same Call without duplicating a row, and the outbox drains to empty after relay.
- **Multi-arch:** the same source cross-compiles to a Raspberry Pi 4 `aarch64` ELF
  (verified end-to-end under emulation) and to amd64.
- **Spec harness unaffected:** `python3 conformance/run.py` remains green (504 checks).

## What's next

Extend the same shapes, not the architecture:
1. **Onboarding depth** — reconciling import (update existing rather than create), route wiring so
   an extension actually rings its device, and vendor-specific provisioning templates.
2. **RTP/B2BUA depth** — full mid-dialog correctness (client transactions, re-INVITE/hold),
   transcoding, and conferencing.
3. **Contact-centre depth** — skills-based and least-recent distribution, wrap-up, and a real
   rating profile (versioned tariffs) behind the CDR.
4. **Multi-node relay** — switch the PostgreSQL relay to `SELECT … FOR UPDATE SKIP LOCKED` for
   concurrent control-plane nodes (split-media topology).
5. **Identity** — JWKS/OIDC verification and SIP-domain→tenant mapping (multi-tenant SIP).
