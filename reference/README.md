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
| `crates/commos-core` | Rust projection of the frozen contracts (`contracts/json-schema/*`): primitives, the CloudEvents envelope, entities, and event payloads. Constraints are enforced at the type boundary. |
| `crates/commosd` | The single binary: API Gateway (Axum), control-plane services, transactional outbox + Event Bus, the typed control→media boundary, config-as-code, graceful drain. |
| `build/targets.toml` · `build/build.sh` | The architecture registry and a parametric cross-build. Raspberry Pi 4 (arm64) is the primary target; amd64 and armv7 are one row each. |
| `deploy/` | Reference `systemd` unit and an example `pbx.yaml`. |

## How the code maps to the spec

| Spec requirement | Where |
|------------------|-------|
| Single self-contained binary, PostgreSQL-only hard dep (CMOS-14-DEP-001/020) | `commosd` boots with zero external deps on the in-process store |
| Two planes over a typed interface, never shared memory (CMOS-03-ARCH-001/002) | `media.rs` — `MediaPlane` trait + `MediaCommand`/`MediaAck`; splittable to a media node unchanged |
| Stateless control plane; all state in the store (CMOS-03-ARCH-010) | `store.rs` — `Store` trait; handlers hold only shared handles |
| Transactional outbox: no state change without its event (CMOS-03-ARCH-030 / CMOS-05-EVT-010) | `store.rs` `commit()` writes entity + event atomically; `relay.rs` drains at-least-once |
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

```bash
# Zero-dependency mode (in-process store) — boots anywhere, no PostgreSQL needed:
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
- `GET /v1/calls`, `POST /v1/calls`, `GET /v1/calls/{id}` — the Routing resource (bearer + tenant-scoped).
- `GET /_introspect/events[/stream]` — **non-normative** view of the event bus for bring-up; not part of the contract.

## Conformance evidence

- **Unit + contract tests:** `cargo test` (core primitives match the schema patterns; the
  Call state machine rejects illegal transitions; the store enforces tenant scoping,
  optimistic concurrency, and atomic outbox commit).
- **Runtime-event conformance:** the `CallStarted` envelope emitted at runtime validates
  against the **frozen** `contracts/json-schema/events/CallStarted.schema.json` (envelope +
  common defs) using the same `referencing` registry the spec's harness uses.
- **Spec harness unaffected:** `python3 conformance/run.py` remains green (504 checks).

## What's next

Extend the same shapes, not the architecture:
1. PostgreSQL binding of `Store` (SQLx) with the real `BEGIN…COMMIT` outbox and a
   `SELECT … FOR UPDATE SKIP LOCKED` relay — no caller changes.
2. The remaining `/v1/calls/{id}:hold|resume|transfer|hangup` actions (media commands are
   already modelled) and their events.
3. More entities/events from `contracts/json-schema/` as the surface widens.
4. Real JWT verification against Identity (Volume 9), replacing the dev token.
5. The SIP/RTP media engine behind the existing `MediaPlane` trait.
