# CommOS

**A specification-first blueprint for a modern communications operating system.**

CommOS is not (yet) a program. It is a **contract**. The legacy PBX world —
Asterisk, FreeSWITCH, and their descendants — encodes its behaviour in XML
dialplans, Lua scripts, and tribal knowledge. CommOS inverts that: the durable
asset is a precise, versioned, implementation-independent **specification suite**
plus a set of **machine-readable contracts** and an **executable conformance
harness**. Code is replaceable; the contract is the standard.

The guiding reframe: **voice is one workload.** A PBX is one application running
on a general communications substrate. The same substrate — Identity, Routing,
Presence, Media, Object Storage, Event Bus, Billing, Policy, AI Integration —
extends to messaging, video, intercom, contact centre, AI agents, and IoT
endpoints without a redesign.

## Why specification-first

For a system that is, in effect, a distributed real-time operating system,
implementation quality depends far more on **architecture and contracts** than on
feature lists. Freezing the contracts first lets independent teams — human or AI —
implement compatible components in parallel, and keeps alternative implementations
(a different media engine, a different provisioning engine) viable without breaking
compatibility. The intent is a vendor-neutral platform standard, in the spirit of
what OCI did for containers and Kubernetes did for orchestration.

## Repository layout

| Path | What it is |
|------|-----------|
| [`spec/`](spec/) | The specification suite — 20 volumes (0–19). The normative prose. |
| [`spec/CONVENTIONS.md`](spec/CONVENTIONS.md) | Normative language (RFC 2119), requirement IDs, contract versioning, conformance levels. Read this first. |
| [`spec/GLOSSARY.md`](spec/GLOSSARY.md) | Canonical terms. One word, one meaning. |
| [`contracts/`](contracts/) | Machine-readable contracts: JSON Schema for the domain model and events, OpenAPI for the API. **Normative.** |
| [`conformance/`](conformance/) | The executable conformance harness. Validates that the contracts are self-consistent and that any implementation conforms. |
| [`reference/`](reference/) | The **reference implementation** — the `commosd` single binary (Rust). First vertical slice of the frozen spine; builds for Raspberry Pi 4 (arm64) and amd64. |

## The specification suite

Read [`spec/README.md`](spec/README.md) for the full index and the **freeze-status
matrix**. In brief:

- **0 Philosophy** — the constitution: invariants and non-goals every other volume obeys.
- **1 PRD** — personas, epics, acceptance criteria.
- **2 Domain Model** — the keystone: every entity, its lifecycle and relationships.
- **3 Architecture** — subsystems and the control-plane / media-plane split.
- **4 API** — REST/WebSocket conventions and the endpoint catalogue (OpenAPI).
- **5 Events** — the canonical event model, envelope, and delivery guarantees.
- **6–18** — Database, Communications, Provisioning, Security, Billing, AI, Plugin
  SDK, UI/UX, Deployment, Observability, Testing, Performance, Engineering Standards.
- **19 ADRs** — why each significant decision was made, and what would reopen it.

## Specification-first development process

1. Freeze the **domain model** (Volume 2 + `contracts/json-schema/entities`).
2. Freeze the **event model** (Volume 5 + `contracts/json-schema/events`).
3. Freeze the **APIs** (Volume 4 + `contracts/openapi`).
4. Freeze the **UX flows** (Volume 13).
5. Build **executable conformance tests** from the specifications (`conformance/`).
6. **Implement** against those tests.

A contract is *frozen* only when it has a machine-readable form under `contracts/`
and the conformance harness passes against it. Prose without a contract is a draft.

## Running the conformance harness

```bash
python3 -m pip install jsonschema        # one dependency
python3 conformance/run.py               # validates contracts + spec consistency
```

The harness is the arbiter of "does this conform." See [`conformance/README.md`](conformance/README.md).

## Stand up a test PBX (real phones, ~5 minutes)

The installer gets a box to a working state and avoids the usual setup traps (chiefly a
loopback `media_ip`, which makes calls connect with no audio):

```bash
cd reference
sudo scripts/install.sh --build --systemd            # detects LAN IP, writes pbx.yaml, installs a service
# or, no root / no systemd:
scripts/install.sh --build --data-dir ./commos-data  # prints the exact command to start it
```

Then, from any machine on the LAN: open `http://<box-ip>:8080/onboarding` to add extensions,
point each phone's SIP account at `<box-ip>:5060` (username = its extension), and place a call.
Dialling your own number is an **echo test** (you hear yourself); dialling another phone's
extension is a **two-way call**. Live state is at `/dashboard`, metrics at `/metrics`.

This is a LAN test bed — SIP/RTP are unencrypted and REGISTER is not yet authenticated, so keep
UDP 5060 off the public internet. PSTN/carrier trunking, recording, and voicemail are not built
yet; internal calling and the echo test are.

## Building & releases

The reference binary is deliberately pure-Rust in its cross-compilation-hostile
dependencies (no OpenSSL, native-tls, or C codec libraries), so it builds for every
target with a stock cross toolchain:

```bash
cd reference
cargo build --release --bin commosd                                  # host
cargo build --release --target aarch64-unknown-linux-gnu --bin commosd   # Raspberry Pi 4/5
```

**CI** ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs on every push and PR:
build + test + `clippy -D warnings` on amd64, a compile-check for every published
architecture, and the full test suite for arm64 under QEMU — the two-architecture
conformance evidence the deployment contract requires (CMOS-14-DEP-060). Contract
conformance runs separately in [`conformance.yml`](.github/workflows/conformance.yml).

**Releases** ([`.github/workflows/release.yml`](.github/workflows/release.yml)) are cut by
pushing a `v*` tag. Every supported architecture is built, packaged, checksummed, signed
with a keyless [build-provenance attestation](https://docs.github.com/actions/security-guides/using-artifact-attestations),
and attached to a GitHub Release (CMOS-14-DEP-004: amd64 + arm64 parity, signed verifiable
checksums):

```bash
git tag v0.4.0 && git push origin v0.4.0
```

| Architecture | libc | Target triple | Typical hardware |
|---|---|---|---|
| amd64 | glibc  | `x86_64-unknown-linux-gnu`      | servers, desktops |
| amd64 | static | `x86_64-unknown-linux-musl`     | containers, portable |
| arm64 | glibc  | `aarch64-unknown-linux-gnu`     | Raspberry Pi 4/5, ARM servers |
| arm64 | static | `aarch64-unknown-linux-musl`    | containers, portable |
| armv7 | glibc  | `armv7-unknown-linux-gnueabihf` | Raspberry Pi 2/3, Zero 2 W |

The `musl` builds are fully static — no glibc dependency, run on any Linux of that
architecture out of the box, which is the single-self-contained-binary mandate
(CMOS-14-DEP-001) at its strongest.

### Object storage (local or S3-compatible)

Blobs (recordings, voicemail, exports, diagnostics) are stored behind a pluggable
`ObjectStore`. By default they live on the **local filesystem** under `{data_dir}/objects`.
The default binary is also built with the **`s3`** feature, so pointing `pbx.yaml` at any
**S3-compatible** service (AWS S3, MinIO, Cloudflare R2, Backblaze B2, Wasabi, Ceph) is just
configuration — credentials come from the environment, never the file (CMOS-14-DEP-083):

```yaml
# pbx.yaml
object_storage: "s3://my-bucket"
s3_endpoint: "https://s3.example.com"   # omit for AWS S3
s3_region: "us-east-1"
s3_path_style: true                      # safe default for S3-compatible servers
```
```sh
export AWS_ACCESS_KEY_ID=…  AWS_SECRET_ACCESS_KEY=…
```

For the leanest, purest-Rust binary (local storage only, no TLS stack), opt out with
`cargo build --no-default-features`. The S3 backend uses **rustls** (never OpenSSL/native-tls),
so an `s3`-enabled build still cross-compiles cleanly to every published architecture.

## Status

This is **v0.4** — **contract-complete**. The foundational spine (Philosophy, Domain,
Events, API, Architecture) is `FROZEN`; the remaining volumes are at `REVIEW`. Every
domain entity (36) and canonical event (74) has a JSON Schema and validated example,
the OpenAPI covers the full API surface (91 paths), the subsystem interfaces are
typed (8), and a second workload (messaging/video/presence/contact-centre) is modelled
alongside voice — proving the substrate is workload-general. The executable
conformance harness runs 500+ checks (green). See the freeze-status matrix in
[`spec/README.md`](spec/README.md).

## Licence

The specification is intended to be openly licensed as a vendor-neutral standard.
Licence selection is tracked in [ADR-0009](spec/019-adrs/README.md).
