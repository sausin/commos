<div align="center">

# CommOS

**A modern Communications Operating System for the AI era.**

API-first · Identity-first · Event-driven · Rust-powered · One binary.
From 5 users to 500,000.

*Not another PBX — a platform for building modern business communications.*

</div>

---

CommOS starts from a single reframe: **voice is just another workload.** A PBX is one
application running on a general communications substrate — the same Identity, Routing,
Media, Events, Billing, and Storage spine also carries messaging, video, presence, IVR,
contact-centre, and AI agents, with no redesign.

Instead of exposing SIP internals and XML dialplans, CommOS exposes **business concepts** —
people, extensions, numbers, call flows, policies, events — and decides for itself how to
translate that intent into signalling. It ships as **one self-contained binary** that runs on
a Raspberry Pi or a fleet of servers, is driven entirely by a **REST API**, and emits a
**structured event for every observable thing that happens**.

## Quickstart — a working phone system in ~5 minutes

```bash
cd reference
sudo scripts/install.sh --build --systemd     # detects your LAN IP, writes pbx.yaml, installs a service
#   no root / no systemd?
scripts/install.sh --build --data-dir ./data  # prints the exact command to run it
```

The binary boots on an **embedded SQLite** database — durable, with **no server to install**.
Then:

1. Open **`http://<box-ip>:8080/onboarding`** — answer two questions (what kind of place, how
   many phones) and CommOS proposes an extension plan, a network layout, discovered phones, and
   the exact DNS/DHCP lines so phones provision themselves. If the box has several network
   interfaces it asks which one carries the phones so the right subnet is picked; you line up
   each handset (by MAC — discovered ones are pre-filled, or type in a MAC for a phone not yet
   powered on) with the extension it should own **and the name to show on its LCD**; and SSL
   stays **off by default** — LAN phones reject a self-signed cert, so provisioning runs over
   plain HTTP unless you have a CA-signed certificate (media is SRTP-encrypted either way). One
   click applies it, tells you exactly **where the config was saved**, and offers a **Reboot
   phones now** button that resyncs the handsets (SIP `check-sync`) so they come up registered
   without a manual power-cycle.
2. Point each phone's SIP account at **`<box-ip>:5060`** (username = its extension) and place a
   call. Dial another extension for a two-way call; dial your own number for an echo test.
3. Watch it live at **`/dashboard`**, scrape **`/metrics`**, or drive everything over the API:

```bash
TENANT=01920000-0000-7000-8000-000000000001
AUTH="Authorization: Bearer tenant:$TENANT"       # dev token; HS256 JWT verification is opt-in

# Place a call
curl -s -X POST localhost:8080/v1/calls -H "$AUTH" -H 'content-type: application/json' \
     -d '{"direction":"OUTBOUND","from_ref":"sip:100","to_ref":"sip:200"}'

# Publish a versioned IVR call flow, list voicemails, watch the event stream
curl -s localhost:8080/v1/call-flows -H "$AUTH"
curl -s localhost:8080/v1/voicemails -H "$AUTH"
curl -s localhost:8080/_introspect/events
```

> A LAN test bed today: SIP/RTP are unencrypted and SIP auth is optional, so keep UDP 5060 off
> the public internet until TLS/SRTP and full auth land.

## Why

Business telephony hasn't fundamentally changed in two decades. Today's platforms still expose
legacy machinery — XML dialplans, SIP contexts, manual provisioning, device-centric billing,
vendor lock-in. CommOS starts from first principles instead:

- **Voice is one workload**, not the whole system.
- **People and identities** are first-class — not extensions and MAC addresses.
- **Every action is an event** — observable, auditable, and consumable by anything.
- **Intent over configuration** — you declare *what*; the system decides *how*.

## Design principles

```
✓ Rust-first          ✓ API-first           ✓ Event-driven
✓ Identity-centric    ✓ Zero-touch setup    ✓ Multi-tenant
✓ Cloud-neutral       ✓ S3-compatible       ✓ Observable
✓ One binary          ✓ Horizontally scalable   ✓ Contract-defined
```

## What works today

CommOS is a maturing reference implementation. This is real, running functionality — not a wish
list:

- **Telephony** — SIP/UDP registration; inbound and API-originated calls; the full Call
  lifecycle (`INITIATED → RINGING → ANSWERED → …`); hold / resume / transfer / hangup; a two-leg
  RTP bridge between registered phones (symmetric-RTP latching); SDP **codec negotiation**
  (transparent pass-through of any codec for bridged/trunked calls; PCMU/PCMA for prompts).
- **PSTN / SIP trunking** — outbound calls to a carrier gateway (with digest auth) and inbound
  DID routing to any internal destination.
- **Voicemail** — record-on-no-answer with a spoken "leave a message after the tone" greeting,
  a configurable ring count before divert, message-waiting indication (MWI) pushed over SIP
  `NOTIFY`, dial-in retrieval (`*97`/`*98`, with delete/save/next), and an HTTP retrieval API.
- **IVR & Call Flows** — a versioned `CallFlow` entity with **publish / rollback** over
  immutable, append-only revision history, and a **media runtime** that plays prompts and
  collects **DTMF** (RFC 4733 telephone-events *and* SIP INFO) to route a caller — e.g. to
  voicemail.
- **Recording** — call audio captured as-is and stored as an object, with a retrieval API.
- **Zero-touch provisioning** — an onboarding wizard that discovers phones from the ARP table,
  proposes an extension/network plan, and generates DNS + DHCP-option-66 config so phones
  provision themselves from `GET /provision/{mac}.cfg`.
- **Billing** — a CDR and a `BillingGenerated` event produced atomically when a call ends, priced
  by a destination-aware rating engine (E.164 longest-prefix, per-minute rounding).
- **Contact-centre** — call queues with basic ACD (assign a call to an available agent) and agent
  state.
- **Directory** — people, extensions, phones, and routes, with lifecycle and config-as-code
  export/import (`GET|POST /v1/config`).
- **Platform** — a REST API (bearer auth, tenant isolation, Problem Details, idempotency,
  cursor pagination), a **transactional outbox → event bus** (no state change without its event),
  outbound **webhooks**, pluggable **object storage** (local or any S3-compatible service),
  Prometheus **metrics**, and readiness-gated graceful drain.
- **Multi-workload substrate** — messaging (channels/threads/messages), presence, and video-room
  entities ride the same store, outbox, and API — proving voice is one workload of many.

Everything is **multi-tenant** and runs from a **single binary** on either embedded SQLite
(default, zero dependencies) or PostgreSQL (multi-node / HA).

## Capabilities

Shipped ✓ · Partial ◐ · Planned ○

| Identity                | Communications            | Platform                     |
| ----------------------- | ------------------------- | ---------------------------- |
| Users ✓                 | SIP / RTP ✓               | REST API ✓                   |
| Multi-tenancy ✓         | Calls & bridging ✓        | Event bus + outbox ✓         |
| Bearer / HS256 JWT ✓    | Voicemail + MWI ✓         | Webhooks ✓                   |
| Directory & lifecycle ✓ | IVR / Call Flows ✓        | Object storage (local/S3) ✓  |
| Attribution chain ✓     | Recording ✓               | Billing / CDR + rating ✓     |
| Capabilities / RBAC ◐   | Queues / ACD ✓            | Config-as-code ✓             |
| OIDC / SSO ○            | Messaging · Presence ◐    | Metrics / observability ✓    |
| WebAuthn / MFA ○        | Video / WebRTC ○          | Event streaming ◐            |
| Device identity ○       | Conferences ○             | Automation ○                 |
| PIN / RFID / Bluetooth ○| PSTN / SIP trunking ✓     | WASM plugins ○               |

## Architecture

```
              REST API   ·   Event Stream   ·   Webhooks
            ┌───────────────────────────────────────────┐
            │                API Gateway                 │  bearer auth · tenant scope · Problem Details
            └───────────────────────────────────────────┘
  ── Control plane ─────────────────────────────────────────────────
     Identity · Provisioning · Routing · Call Flows / IVR
     Policy · Billing / Rating · Queues (ACD) · Webhooks
  ── Media plane ─── typed boundary, never shared memory ────────────
     SIP (UDP) · RTP · DTMF · Prompt playout · Recording · Voicemail
  ── Transactional outbox  ─────────────────────────────▶  Event Bus
  ── Storage ────────────────────────────────────────────────────────
     SQLite (default)  /  PostgreSQL        Object Storage (local / S3)
```

Two design invariants make this hold together:

- **Control decides, media acts.** The control and media planes talk only over a **typed
  interface, never shared memory** — even compiled into one binary — so the media plane can later
  split onto its own node with no control-plane change.
- **No state change without its event.** Every mutation and the event it produces are written in
  the **same transaction** to an outbox, then relayed at-least-once. That is what makes the event
  stream a faithful, replayable record of the platform.

## Intent, not dialplans

CommOS does not ask administrators to manage SIP contexts or XML. They manage **people, phones,
numbers, and call flows**; the system translates that into signalling. The whole directory is
**config-as-code** — export it as a Git-reviewable `pbx.yaml`, review the diff, re-import it:

```bash
curl -s localhost:8080/v1/config -H "$AUTH" > pbx.yaml     # export the live directory
git diff pbx.yaml                                          # review the change
curl -s -X POST localhost:8080/v1/config -H "$AUTH" \
     -H 'content-type: text/yaml' --data-binary @pbx.yaml         # apply it
```

Runtime configuration is a separate, declarative `pbx.yaml`. Secrets are **references, never
inlined** (an inline secret is rejected at boot):

```yaml
# pbx.yaml
media_ip: "192.168.1.10"                          # the address advertised to phones for RTP
sip_listen: "0.0.0.0:5060"                        # SIP/UDP ingress (null disables it)
sips_listen: "0.0.0.0:5061"                       # SIP-over-TLS ingress (needs a --features tls build)
sip_tls_cert: "/etc/commos/tls/sip-fullchain.pem" # PEM cert chain for SIPS
sip_tls_key: { ref_uri: "file:///etc/commos/tls/sip-key.pem" }  # key by reference, never inline
record_calls: true                                # capture call audio as objects
voicemail_enabled: true                           # record-on-no-answer + MWI
srtp: true                                        # encrypt RTP (SRTP/SDES) on endpoint + bridge/trunk legs when offered
trunk_srtp: false                                 # also offer SRTP to a carrier trunk (off: carrier leg stays plaintext)
object_storage: "s3://my-bucket"                  # local filesystem by default (any S3-compatible service)
database_url: { ref_uri: "env://DATABASE_URL" }   # embedded SQLite if unset
```

## Provisioning, done right

```
Plug in a phone.  CommOS detects it.  Approve.  Done.
```

No XML. No per-vendor templates to hand-edit. No MAC-address spreadsheets. The onboarding wizard
reads the network, flags likely IP phones by MAC vendor, and generates the DNS + DHCP lines to
paste; each phone then fetches its own config and registers itself.

## Billing that attributes to people

Extensions don't make calls — **people do.** Every billable action is attributed along the chain

```
Device → Identity → Department → Cost Centre → Organisation
```

so a CDR carries *who*, not just *which port* — enabling internal recharge, department reporting,
and real accountability. Off-net destinations are normalised to E.164 and priced by a
longest-prefix rating table; the record is produced atomically with the call's `CallEnded` event.

## AI wasn't bolted on

CommOS doesn't ship a model — it ships **structure**. Every call becomes typed data, every
recording an addressable object, every action an event on the bus. That makes CommOS a clean
integration surface for any AI platform — OpenAI, Claude, Gemini, Ollama, vLLM, LangGraph, n8n,
anything — consuming events (or webhooks) and acting through the same API a human would. AI is a
first-class **consumer**, not a dependency baked into the core.

## Scale

The architecture is the same at every size. The control plane is **stateless** — all state lives
in the store — so you scale out by adding nodes behind a load balancer and pointing them at one
PostgreSQL. Start on a single Raspberry Pi with embedded SQLite; grow to a multi-node cluster
without changing a line of application code.

```
5  →  50  →  500  →  5,000  →  50,000  →  500,000 users
```

## Built on frozen contracts

CommOS's durable asset is not any one codebase — it's a precise, versioned, implementation-
independent **contract**: the domain model and events as JSON Schema, the API as OpenAPI, and an
**executable conformance harness** that proves an implementation conforms. Code is replaceable;
the contract is the standard — in the spirit of what OCI did for containers.

```bash
python3 -m pip install jsonschema
python3 conformance/run.py          # validates the contracts + spec consistency (500+ checks)
```

| Path | What it is |
|------|-----------|
| [`reference/`](reference/) | The **`commosd`** single binary (Rust) — the reference implementation. |
| [`contracts/`](contracts/) | Machine-readable contracts: JSON Schema (entities + events), OpenAPI (the API). |
| [`conformance/`](conformance/) | The executable conformance harness — the arbiter of "does this conform". |
| [`spec/`](spec/) | The specification suite (20 volumes): the normative prose behind the contracts. |

## Build & run

Pure-Rust in its cross-compilation-hostile dependencies (no OpenSSL, native-tls, or C codec
libraries), so the binary builds for every target with a stock toolchain:

```bash
cd reference
cargo build --release --bin commosd                                    # host
cargo build --release --target aarch64-unknown-linux-gnu --bin commosd # Raspberry Pi 4/5
./target/release/commosd                                               # boots on :8080 (SQLite)
```

Releases are cut by pushing a `v*` tag; every architecture is built, checksummed, and signed with
a keyless build-provenance attestation, then attached to a GitHub Release:

| Architecture | libc | Target triple | Typical hardware |
|---|---|---|---|
| amd64 | glibc / musl | `x86_64-unknown-linux-{gnu,musl}` | servers, desktops, containers |
| arm64 | glibc / musl | `aarch64-unknown-linux-{gnu,musl}` | Raspberry Pi 4/5, ARM servers, containers |
| armv7 | glibc | `armv7-unknown-linux-gnueabihf` | Raspberry Pi 2/3, Zero 2 W |

The `musl` builds are fully static — no glibc dependency, run on any Linux of that architecture
out of the box. See [`reference/README.md`](reference/README.md) for the full operator guide
(PostgreSQL, S3 storage, systemd, cross-builds).

## Status

An actively developed reference implementation on a frozen contract spine. The foundational
volumes (Philosophy, Domain Model, Events, API, Architecture) are `FROZEN`; the platform surface
above is real and growing. Production hardening — TLS/SRTP, full OIDC/RBAC, PSTN trunking,
conferencing, WebRTC — is on the roadmap, tracked against the same contracts.

```
Phase 1 — Core Communications   ✓ Identity  ✓ SIP/RTP  ✓ Provisioning  ✓ Recording  ✓ Voicemail  ✓ IVR
Phase 2 — Platform              ✓ Billing   ✓ Event Bus  ✓ Webhooks  ○ Plugins  ○ AI automation
Phase 3 — Communications OS     ◐ Messaging  ◐ Video  ◐ Contact Centre  ○ Mobile clients  ○ Federation
```

## Acknowledgements

The voicemail greeting and `*97`/`*98` retrieval menu play audio prompts from the
**[FreePBX](https://www.freepbx.org/) / Sangoma** publicly-downloadable sound library
(see the [FreePBX GitHub org](https://github.com/FreePBX)). CommOS does **not** bundle or
redistribute these files — the installer downloads them onto your system (into
`{data_dir}/sounds`), and they remain the property of FreePBX / Sangoma. With no pack installed,
voicemail falls back to a synthesized beep. Thank you to the FreePBX community for making these
prompts freely available.

## Licence

Source-available under the **[O'Saasy License](https://osaasy.dev/)** (see [`LICENSE`](LICENSE)):
self-host, use, modify, and redistribute freely — but don't repackage it as a competing hosted /
managed / SaaS product. See [ADR-0009](spec/019-adrs/README.md) for the rationale.

---

<div align="center">

*We believe communication infrastructure should be simple to operate, secure by default,
programmable through APIs, observable by design, and intelligent through integration — not
complexity. CommOS exists to build that future.*

</div>
