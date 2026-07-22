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

The binary's reported version (`--version`, `/metrics`, dashboard) comes from `crates/commosd/build.rs`
via `env!("COMMOS_VERSION")` — `COMMOS_VERSION` env (CI passes the release tag) → `git describe` →
manifest fallback. Do **not** revert these sites to `CARGO_PKG_VERSION` (it is frozen at `0.1.0`).

## Path convention (IMPORTANT — history of path-resolution bugs)

All runtime state hangs off **`data_dir`** (default `.`; the installer sets it, e.g.
`/var/lib/commos`). Resolve paths against it with the `data_dir.trim_end_matches('/')` idiom —
**never** relative to the current working directory:

- `{data_dir}/commos.db` — SQLite (`Config::default_sqlite_path`)
- `{data_dir}/objects` — local object store (recordings, voicemail, exports)
- `{data_dir}/secrets/jwt.key` — auto-generated JWT secret
- `{data_dir}/sounds/en/*.ulaw` — audio prompts (`Config::sounds_dir`; voicemail greeting +
  `*97` menu). Downloaded from FreePBX by the installer; missing files fall back to a synth beep.
- `{data_dir}/moh/*.ulaw` — music-on-hold loop (`Config::moh_dir`; concatenated sorted). Absent →
  a synthesized tune (`sip/moh.rs`), so hold is never silent.
- `{data_dir}/display_name.txt` — optional (`Config::display_name_file`): the display name a
  called phone shows for CommOS-placed calls (1 line = static, N lines = random per call; absent
  → "commos"). Read per call.

Config file itself is found via `default_config_path()` in `main.rs` (`$COMMOS_CONFIG`, then
`./pbx.yaml`, `/etc/commos/pbx.yaml`, `/var/lib/commos/pbx.yaml`).

## Key subsystems → files

- **SIP / media plane** — `sip/server.rs` (the B2BUA: INVITE/BYE, bridging, voicemail deposit +
  `*97`/`*98` retrieval, MWI), `sip/ivr.rs` (prompt playout + DTMF collect), `sip/g711.rs`
  (μ-law/A-law synth + transcode), `sip/rtp.rs`, `sip/srtp.rs`/`sdes.rs` (SRTP), `sip/digest.rs`,
  `sip/reboot.rs` (remote reboot of a *registered* extension via a `check-sync;reboot=true` NOTIFY,
  answering a phone's digest challenge with its SIP credential — the reliable check-in/checkout
  path at `POST /v1/onboarding/reboot-extension`; the discovery sweep in
  `control/onboarding.rs::reboot_phones` remains for freshly-found IPs).
  Note: the UDP receive loop only parses + dispatches — it `tokio::spawn`s each datagram's
  `handle()` so `on_invite` blocking (up to `no_answer_timeout` while ringing the callee) no
  longer serializes other call setup.
  IMPORTANT (history of bugs): every outbound UAC request CommOS originates (bridge/trunk
  INVITE+ACK, mid-dialog BYE) **must** carry a reachable `Via` via `message::via_header(sent_by)`
  — `sent_by` = `media_ip` at the ephemeral port the request is sent from and its response awaited
  on. Omitting it makes `message::request` fall back to an unresolvable `commos.invalid` sent-by,
  so the callee's `180`/`200` are lost and the call wrongly diverts to voicemail. Bridged legs also
  pass the caller's identity (`CallerId` → `caller_from_header`) so the callee sees the real
  caller, not "commos". This depends on a correct `media_ip` (the installer picks the phone-LAN NIC
  on multi-homed hosts).
- **Control plane** — `control/routing.rs` (Call state machine, driven by `MediaFact`s + CDRs),
  `control/voicemail.rs`, `control/onboarding.rs`, `control/provisioning.rs`, `control/trunking.rs`.
- **Multi-destination routing** — `control/ringplan.rs` (pure `DialPlan` builder: ring stages +
  treatment + final action; the tested spine for ring groups / follow-me / queue-wait),
  `control/ringresolve.rs` (resolves live `RingGroup`/`Forwarding` config + registration state into
  a plan), `control/ringing.rs` (CRUD service). The SIP B2BUA executes a plan via
  `SipServer::execute_ring_plan`; `RING_ALL` rings all registered members **simultaneously**
  (`SipServer::fork_bridge`: parallel INVITEs raced on one task via `poll_fn`, first 2xx wins,
  losers `CANCEL`led — `try_bridge` takes an optional cancel `watch` receiver). MoH engine is
  `sip/moh.rs` (load/synth/stream); live hold-bridge injection is TODO.
- **Voicemail-to-email** — `control/smtp.rs` (hand-rolled pure-Rust SMTP submission client, like
  `webhook_delivery.rs`) + `control/voicemail_email.rs` (a `VoicemailReceived` bus subscriber that
  resolves the mailbox → `smtp.mailboxes` recipient and emails a WAV). Config: `smtp:` section.
- **Provisioning** — `api/provision.rs` (per-vendor phone configs: Yealink/Grandstream/generic,
  incl. NTP + timezone, voicemail Message-key code `*97`, Grandstream TR-069 off (`P1409=0`, kills
  the "CPE connection failed" warning), and optional web-UI lockdown via `phone_admin_password`
  SecretRef — Yealink `static.security.user_password`, Grandstream `P2`).
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
