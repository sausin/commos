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
| Single self-contained binary, PostgreSQL-only hard dep (CMOS-14-DEP-001/020) | `commosd` boots with zero external deps on the in-process store, or on PostgreSQL when configured |
| PostgreSQL is the system of record (CMOS-14-DEP-020) | `store/postgres.rs` — durable binding; entities stored as contract JSON with typed identity/version columns |
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

### Durable mode (PostgreSQL system of record)

```bash
docker compose -f deploy/docker-compose.yml up -d postgres
export DATABASE_URL="postgres://commos:commos-dev-password@localhost:5432/commos"
# pbx.yaml references the secret, never inlines it (CMOS-14-DEP-083):
#   database_url:
#     ref_uri: "env://DATABASE_URL"
./target/debug/commosd --config deploy/pbx.example.yaml   # migrations run at boot
bash scripts/smoke.sh                                     # end-to-end check
```

State persists across restarts and the outbox is a real `BEGIN…COMMIT`. See
[`docs/postgres.md`](docs/postgres.md).

### Zero-dependency mode (in-process store)

```bash
# Boots anywhere, no PostgreSQL needed (omit database_url from pbx.yaml):
./target/x86_64-unknown-linux-gnu/release/commosd --config deploy/pbx.example.yaml

TENANT=01920000-0000-7000-8000-000000000001
AUTH="Authorization: Bearer tenant:$TENANT"     # dev token; JWT verification is Volume 9 work

curl -s localhost:8080/info
curl -s -X POST localhost:8080/v1/calls -H "$AUTH" -H 'content-type: application/json' \
     -d '{"direction":"OUTBOUND","from_ref":"sip:100","to_ref":"+14155550100"}'
curl -s localhost:8080/v1/calls -H "$AUTH"
curl -s localhost:8080/_introspect/events        # watch CallStarted flow through the outbox
```

### Endpoints

- `GET /livez`, `GET /readyz`, `GET /info` — operational signals (unauthenticated).
- **Voice workload** — `GET|POST /v1/calls`, `GET|PATCH /v1/calls/{id}` (`PATCH` is an
  RFC 7386 merge-patch with `If-Match` optimistic concurrency), and actions
  `POST /v1/calls/{id}/{hold,resume,hangup,transfer}`. A Call starts `INITIATED`; ring and
  answer arrive **asynchronously as media facts** (the media plane reports them; the control
  plane applies them), so a fresh call reaches `ANSWERED` a moment after creation.
- **Messaging workload** — `GET|POST /v1/channels`, `/v1/threads`, `/v1/messages` and
  their `/{id}` reads.
- **Real-time workloads** — `GET|POST /v1/video-rooms`, `GET|POST /v1/presence` and their
  `/{id}` reads. Same substrate, same store, same outbox — voice is one workload of many.
- `GET /_introspect/events[/stream]` — **non-normative** view of the event bus for bring-up; not part of the contract.

All `/v1` routes are bearer-authenticated and tenant-scoped.

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
1. **SQLite as the zero-config durable default** — an embedded, single-writer `Store`
   binding so the single binary is durable *and* dependency-free out of the box (ideal for a
   Raspberry Pi / small-business box). In-memory stays the explicit test mode; PostgreSQL
   becomes the opt-in multi-node/HA backend. Keep ephemeral state (registrations, presence)
   in memory to minimise SD-card writes.
2. The real SIP/RTP media engine behind the existing `MediaPlane` trait (the fact channel is
   already in place), and registration handling as in-memory ephemeral state.
3. Richer queries and update/soft-delete across the workloads (thread-scoped message paging,
   presence upsert-by-subject).
4. Multi-node relay: switch the PostgreSQL relay to `SELECT … FOR UPDATE SKIP LOCKED` for
   concurrent control-plane nodes (split-media topology).
5. Real JWT verification against Identity (Volume 9), replacing the dev token.
