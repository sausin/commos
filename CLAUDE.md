# CLAUDE.md — CommOS

Orientation for AI coding sessions. Keep it lean; **update it surgically at the end of a session**
when something structural changes (see the last section).

## What CommOS is

A single-binary, pure-Rust **SIP/PBX + HTTP API** communications platform (`commosd`). Runs on a
small box (Raspberry Pi 4 up), zero external dependencies by default (embedded SQLite + local
object store). Config-as-code via `pbx.yaml`; secrets are always *referenced*, never inline
(CMOS-14-DEP-083).

## Repo layout

- `spec/` — normative volumes (numbered `000`–`019`). The contract; code must conform.
- `contracts/` — frozen API/event/entity contracts.
- `conformance/` — conformance scenarios the implementation must pass.
- `reference/` — **the actual implementation** (a Cargo workspace):
  - `crates/commos-core` — entities, events, common types (the domain model).
  - `crates/commosd` — the daemon: `sip/` (SIP/RTP/media plane), `control/` (control plane),
    `api/` (HTTP handlers), `store/` (SQLite/Postgres/in-mem), `main.rs`, `config.rs`, `state.rs`.
  - `deploy/` — `pbx.example.yaml`, systemd unit, docker-compose.
  - `scripts/` — `install.sh` (installer), `smoke.sh` (call-path smoke test).

## Build / test / run

All commands run from `reference/`:

- Build: `cargo build --release --bin commosd`  (features: `tls` for SIPS, `s3` for S3 objects)
- Test:  `cargo test --bin commosd`  ·  Lint: `cargo clippy --bin commosd`
- Run:   `./target/release/commosd --config <pbx.yaml>`  (or `scripts/install.sh --build --systemd`)

## Path convention (IMPORTANT — history of path-resolution bugs)

All runtime state hangs off **`data_dir`** (default `.`; the installer sets it, e.g.
`/var/lib/commos`). Resolve paths against it with the `data_dir.trim_end_matches('/')` idiom —
**never** relative to the current working directory:

- `{data_dir}/commos.db` — SQLite (`Config::default_sqlite_path`)
- `{data_dir}/objects` — local object store (recordings, voicemail, exports)
- `{data_dir}/secrets/jwt.key` — auto-generated JWT secret
- `{data_dir}/sounds/en/*.ulaw` — audio prompts (`Config::sounds_dir`; voicemail greeting +
  `*97` menu). Downloaded from FreePBX by the installer; missing files fall back to a synth beep.

Config file itself is found via `default_config_path()` in `main.rs` (`$COMMOS_CONFIG`, then
`./pbx.yaml`, `/etc/commos/pbx.yaml`, `/var/lib/commos/pbx.yaml`).

## Key subsystems → files

- **SIP / media plane** — `sip/server.rs` (the B2BUA: INVITE/BYE, bridging, voicemail deposit +
  `*97`/`*98` retrieval, MWI), `sip/ivr.rs` (prompt playout + DTMF collect), `sip/g711.rs`
  (μ-law/A-law synth + transcode), `sip/rtp.rs`, `sip/srtp.rs`/`sdes.rs` (SRTP), `sip/digest.rs`.
  Note: the receive loop is single-threaded and `on_invite` blocks while ringing the callee.
- **Control plane** — `control/routing.rs` (Call state machine, driven by `MediaFact`s + CDRs),
  `control/voicemail.rs`, `control/onboarding.rs`, `control/provisioning.rs`, `control/trunking.rs`.
- **Provisioning** — `api/provision.rs` (per-vendor phone configs: Yealink/Grandstream/generic,
  incl. NTP + timezone).
- **State / wiring** — `state.rs` (`AppState`), `main.rs` (`run()` wires everything;
  `SipServer::new` and `AppState::new` are the big constructors).

## Conventions

- Secrets referenced, never inline (`SecretRef`, CMOS-14-DEP-083).
- Media stored as-is (no transcoding on the storage path); G.711 μ-law is the storage codec.
- Prompt-bearing media (IVR, voicemail greeting/retrieval) is plaintext G.711 (SRTP for it is
  future work); two-leg bridge/echo/trunk media honours SRTP when the caller offers it.
- Voicemail time is measured in **rings** (`no_answer_rings`, ~6 s each) for operators.

## Maintenance (do this at the end of each session)

Update this file **surgically**: amend only what structurally changed — a new subsystem, a moved
or new `{data_dir}` path, a new build/test command, a renamed key file. Keep it lean: do **not**
restate the specs, log session history, or enumerate every file. If nothing structural changed,
leave it alone.
