# TODO — CommOS roadmap

Parked work to be picked up in separate threads. Keep this as a backlog, not a spec — each item
graduates to a proper spec/ADR + branch when started.

## Feature-parity roadmap (vs FreePBX / small-office PBX)

Positioning: CommOS is a well-architected core engine at roughly **30–40% of small-office PBX
parity**. The spine (SIP/RTP, provisioning, voicemail, IVR/CallFlow, trunking, ACD, CDR/billing,
REST API + events) is real and modern; the gaps are the classic PBX *feature surface*. The FreePBX
**module / AGI / dialplan ecosystem is a deliberate non-goal** (spec N-1/N-4) — not a gap to close.

Top gaps to reach "usable small-office PBX", ordered by impact. Size: S ≈ days, M ≈ weeks,
L ≈ multi-week.

| # | Gap | Size | Why |
|---|-----|------|-----|
| 1 | ~~**Ring groups**~~ ✅ | M | **Done (2026-07).** Entity + store + `/v1/ring-groups` API + SIP execution, including **true simultaneous ring-all fork** — parallel INVITEs raced on one task, first 2xx wins, losers get a SIP `CANCEL` (and a `BYE` on fork glare). Hunt strategies + follow-me exact. |
| 2 | **Time conditions / business hours / holidays** | M | Day/night-mode routing is table-stakes. Needs a schedule entity + CallFlow node. |
| 3 | **Conferences (N-way mixer)** | L | Only two-leg bridging exists; needs a real RTP mixer (pure-Rust, no codec libs makes it non-trivial). |
| 4 | **Harden attended/blind transfer + B2BUA** | L | Transfer is scaffolded but mid-dialog correctness is `TODO(B2BUA)`; used constantly, must be solid. |
| 5 | ~~**Music on hold**~~ ✅ | S/M | **Done (2026-07).** Engine (`sip/moh.rs`: file-load + synth fallback + looped streaming) + `moh_dir`/`music_on_hold` config; streamed to queue-waiting callers (#9) **and injected into the live two-leg bridge on a hold re-INVITE** (`rtp::Bridge` `HoldState`/`set_hold` — held leg hears MoH, relay suspends, resume restores it). Only *callee*-initiated hold is unwired (its re-INVITE arrives on the leg-B dialog which the in-dialog handler doesn't route yet); the media path already supports it (`HoldState::Held(Leg::B)`). |
| 6 | ~~**Voicemail-to-email**~~ ✅ | S | **Done (2026-07).** Pure-Rust SMTP client (`control/smtp.rs`) + `VoicemailReceived` dispatcher + `smtp:` config (mailbox→email map, μ-law→WAV attachment). |
| 7 | ~~**Call forwarding / Follow-me**~~ ✅ | M | **Done (2026-07).** `Forwarding` entity (ALWAYS/BUSY/NO_ANSWER/UNAVAILABLE + ordered follow-me targets) + `/v1/forwardings` API + SIP execution via the ring planner. |
| 8 | **More feature codes** (DND, `*72` forward, etc.) | M | Deliver as *intent*, not dialplans (honoring N-5). The `*97`/`*98` retrieval codes added in PR #8 are the pattern to build on. **Now cheap:** a `*72`/`*73` code just creates/deletes a `Forwarding` row; DND is a forward-to-voicemail rule. |
| 9 | ~~**Queue caller experience**~~ ✅ | M | **Done (2026-07).** A `queue:<uuid>` caller is answered immediately and hears a greeting + music-on-hold + periodic announcements while CommOS rings the queue's registered members (round-robin, `ivr_transfer` splice on answer) and overflows to `overflow_ref` after `max_wait_ms` (`sip/queuewait.rs` policy + `SipServer::queue_wait_driver`). Position announcements + callback remain as polish. |
| 10 | **WebRTC softphone endpoint** | L | Spec'd first-class (CMOS-07-SIP-051); unlocks browser calling + a user portal. |

> **Shared spine (landed 2026-07).** Items 1/5/7/9 are built on one pure, exhaustively-tested
> planner: `control/ringplan.rs` turns a dialled number into a `DialPlan` (ordered ring stages +
> per-stage treatment + final action), and `control/ringresolve.rs` resolves live config
> (ring groups, forwarding) + registration state into that plan. The SIP B2BUA executes it
> (`SipServer::execute_ring_plan`, with simultaneous ring-all forking in `SipServer::fork_bridge`).
> **All of items 1/5/6/7/9 have landed.** The only leftovers are polish — *callee*-initiated hold
> (the media path is ready; just needs the leg-B re-INVITE routed), queue position announcements,
> and callback. All the control-plane decision logic is unit-tested; the media-plane execution
> (forking, hold injection, queue treatment) reuses tested primitives but still merits real-phone
> validation.

### Parked — hospitality / multi-tenant hardening (added 2026-07, not yet started)
These graduate to their own specs/branches. Grouped because a hotel/serviced-office deployment
needs all three, and two of them extend surfaces that already exist.
- **Checkout privacy purge** (S/M) — On guest checkout, delete the call logs (CDRs) **and**
  voicemails belonging to an extension/number. Without it, the next guest inherits the previous
  guest's call history and messages — a real privacy exposure. Natural home: alongside the
  check-in/checkout path (`POST /v1/onboarding/reboot-extension`, `control/onboarding.rs`). Needs a
  scoped purge that removes CDRs + `Voicemail` rows + their stored audio objects for a mailbox, with
  an audit event.
- **Provisioning directory / phonebook alignment** (S) — Push the platform directory
  (`api/directory.rs` already knows the users/extensions) to the phones as a **remote phonebook**
  during provisioning. Today `api/provision.rs` writes SIP/NTP/lockdown config but never a contact
  list, so handsets come up with empty directories. Yealink (`remote_phonebook`) and Grandstream
  (XML phonebook download) both take a URL — add a phonebook XML endpoint + per-vendor template.
- **Class-of-service / call gating & whitelisting** (M) — Per-extension / per-group call-permission
  matrix: restrict which destinations an endpoint may dial — no direct inter-guest calls, no
  trunk/external calls from a room phone, block specific internal targets (e.g. kitchen staff).
  Extends `control/policy.rs` from *tenant-wide* fraud guardrails (international, concurrency,
  emergency) to a *per-endpoint* class-of-service checked on the INVITE path. Hospitality-driven but
  broadly useful (kiosk phones, lobby phones, department restrictions).

### Honorable mentions
- **SIP-layer security / rate-limiting** (fail2ban-style, SIP flood protection) — Missing, and a real
  risk once 5060 faces a network (README already warns about this). Auth-level fraud guardrails exist
  (`control/policy.rs`), but network-layer protection does not.
- **CDR reporting / analytics UI** — CDR entity + list/get API exist; no aggregation/reporting view.
- **Full backup & restore** — config-as-code export/import exists; no DB + recordings/objects bundle.
- **Outbound SIP-TLS on UAC/trunk legs** — inbound SIPS is feature-gated; outbound TLS is not done.
- **SRTP for prompt-bearing media** — IVR + voicemail greeting/deposit/retrieval are plaintext G.711
  (introduced in PR #8); bridge/echo/trunk legs already honor SRTP.

## Performance & scale (Raspberry Pi 4 / 4 GB target)

Analysis, not yet actioned. Honest read: **the ceiling today is set by the software
architecture, not the Pi.** The media plane could plausibly relay a few hundred concurrent G.711
calls (no transcoding, multi-thread tokio, zero-copy plaintext relay), but as-is you can reliably
establish only ~10–30 because call *setup* is serialized. Estimates are ±2× — validate with a
SIPp load test whose scenario **rings a few seconds before answering** (that is what exposes the
blocking loop); watch per-core CPU, fd count, and setup latency.

### ✅ BIGGEST WIN — de-block the SIP receive loop (DONE)

**Landed.** The UDP `run()` loop in `sip/server.rs` now only parses + dispatches: it copies each
datagram and hands `handle()` to `tokio::spawn` instead of awaiting it inline. `on_invite` still
blocks for up to `no_answer_timeout` (~30 s) while ringing the callee, but that block now lives on
a detached task, so a single ringing phone no longer freezes every other INVITE / REGISTER / BYE.
Shared state was already `Arc`/`Mutex`, so the change was mechanical; setup now runs concurrently
across all cores. (The TLS ingress already spawned per-connection, so it was unaffected.)

Original analysis, for context: `run()` awaited `handle()` inline, and `on_invite` → `try_bridge`
→ `send_invite_await_final` blocks up to ~30 s. Consequences were serialized setup on one core, a
single ringing phone freezing the SIP plane for up to 30 s, while established calls (separate
spawned RTP relays) kept flowing. This was **~80% of the achievable win** — everything below is
second-order and was deferred until this landed.

Follow-up worth noting: spawning means a retransmitted INVITE can now be processed concurrently
with its original (before, retransmits queued behind the block). Real SIP transaction dedup by
branch/Call-ID is not implemented — acceptable for now, but the natural next hardening step.

### Concrete ceilings & CPU

- **File descriptors.** ~2 UDP fds per established call (the two relay sockets). Default `ulimit`
  1024 → a hard wall near **~500 calls**. ✅ `LimitNOFILE=65536` now set in both the installer's
  generated unit and `deploy/commosd.service`. Cheap insurance.
- **ARM hardware crypto for SRTP.** The A72 has ARMv8 AES/SHA, but the `aes`/`sha1` crates only use
  it if the build enables it. ✅ `rustflags = ["-C", "target-feature=+aes,+sha2"]` now set for
  `aarch64-unknown-linux-gnu` in `.cargo/config.toml`, so SRTP uses hardware AES/SHA instead of the
  software fallback.
- **Per-packet allocation in the SRTP relay.** `unprotect` → `Cow::Owned` and `protect` → fresh
  `Vec` (`sip/rtp.rs`) = 2 heap allocs per packet per direction when encrypted. Use a reusable
  per-relay scratch buffer. (Plaintext relay is already alloc-free.)
- **Syscall overhead.** One `recv_from`/`send_to` per 20 ms packet per direction. `recvmmsg` /
  `sendmmsg` (or GRO/GSO) batching amortizes it — only worth it targeting the high hundreds.
- **One tokio task per call.** Fine to a few hundred; beyond that a single reactor over many RTP
  sockets cuts scheduler churn. Defer until profiling demands it.

### Memory

- **Recording capture holds the whole call in RAM, capped 16 MB/call (~35 min)** (`sip/rtp.rs`
  `MAX_CAPTURE_BYTES`). 100 concurrent *recorded* long calls ≈ 1.6 GB. If heavy recording is
  expected, stream the capture to the object store in chunks instead of one growing `Vec`.
  Voicemail is already capped at 120 s.
- Per-call fixed overhead (buffers + task + dialog entry) is tens of KB — not the binding limit.

### Algorithmic / suboptimal choices

- **`resolve_extension` queries SQLite on every INVITE** (`control/routing.rs`, paged store scan) —
  DB I/O on the setup hot path. Add an in-memory extension→route cache (like registrations already
  are), invalidated on change. Best effort-to-impact ratio after de-blocking the loop.
- **`find_registered`** is a linear scan of all registrations per call; **`mailbox_summary` /
  `list_for_mailbox`** scan *all* voicemails × a `get_call` each (`control/voicemail.rs`). Both
  O(N), fine at small scale — index by user-part / per-mailbox when volume grows.
- **Nonce map `retain()` sweep** on every challenge is O(nonces) — trivial now; lazy/time-wheel
  expiry if auth volume grows.

### Already-good choices (don't over-correct)

G.711 stored/relayed as-is (no transcode), in-memory registrations off the DB, bounded capture,
zero-copy plaintext relay, multi-thread runtime, `opt-level="z"` + `strip` (small footprint).

## Deliberate non-goals (for reference — do NOT treat as gaps)

- No FreeSWITCH/Asterisk config front-end; no raw SIP/SDP/codec exposure by default (N-1, N-5).
- No bug-for-bug dialplan compatibility; the module/AGI ecosystem is out of scope (N-4).
- No bundled LLM/ASR/TTS — AI is an external event consumer (N-2).
- No mandatory Kubernetes/broker/cloud — single binary + SQLite/Postgres; HA is scale-out (N-3).
