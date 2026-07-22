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
| 1 | **Ring groups** | M | Most-expected feature after extensions ("ring the whole team"). No `ring_group` entity today. |
| 2 | **Time conditions / business hours / holidays** | M | Day/night-mode routing is table-stakes. Needs a schedule entity + CallFlow node. |
| 3 | **Conferences (N-way mixer)** | L | Only two-leg bridging exists; needs a real RTP mixer (pure-Rust, no codec libs makes it non-trivial). |
| 4 | **Harden attended/blind transfer + B2BUA** | L | Transfer is scaffolded but mid-dialog correctness is `TODO(B2BUA)`; used constantly, must be solid. |
| 5 | **Music on hold** | S/M | Hold works but plays silence; needs an MoH source + per-hold streaming. Low effort, high polish. |
| 6 | **Voicemail-to-email** | S | VM + MWI already work; add an SMTP sender + mailbox email config. Small lift, big value. |
| 7 | **Call forwarding / Follow-me** | M | "Send my calls to my mobile" — mobility is a stated CommOS pillar yet absent. |
| 8 | **More feature codes** (DND, `*72` forward, etc.) | M | Deliver as *intent*, not dialplans (honoring N-5). The `*97`/`*98` retrieval codes added in PR #8 are the pattern to build on. |
| 9 | **Queue caller experience** | M | Position/wait announcements, queue MoH, callback. ACD assigns agents but gives the caller no treatment. |
| 10 | **WebRTC softphone endpoint** | L | Spec'd first-class (CMOS-07-SIP-051); unlocks browser calling + a user portal. |

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

### 🔴 BIGGEST WIN — de-block the SIP receive loop

The SIP receive loop is **single-threaded and blocks on call setup**. `run()` awaits `handle()`
inline (`sip/server.rs`, the `recv_from` loop), and `on_invite` → `try_bridge` →
`send_invite_await_final` blocks for **up to `no_answer_timeout` (~30 s)** waiting for the callee
to answer. Consequences:

- Call setup is serialized across the whole system, on one core.
- **A single ringing phone freezes the entire SIP plane for up to 30 s** — no other INVITE /
  REGISTER / BYE is processed. (Raising the ring timeout for UX made this worse.)
- Established calls keep flowing (their RTP relays are separate spawned tasks); only *setup*
  starves.

**Fix:** the receive loop should only parse + dispatch, then hand each transaction to
`tokio::spawn` (shared state is already `Arc`/`Mutex`, so this is mostly mechanical). This moves
setup from "~1 at a time" to hundreds and lets all 4 cores work. **~80% of the achievable win —
do this before any other perf work.** Everything below is second-order until it is done.

### Concrete ceilings & CPU

- **File descriptors.** ~2 UDP fds per established call (the two relay sockets). Default `ulimit`
  1024 → a hard wall near **~500 calls**. Set `LimitNOFILE=65536` in the systemd unit (installer +
  `deploy/commosd.service`). Cheap insurance.
- **ARM hardware crypto for SRTP.** The A72 has ARMv8 AES/SHA, but the `aes`/`sha1` crates only use
  it if the build enables it. Add `-C target-feature=+aes,+sha2` for `aarch64` in
  `.cargo/config.toml`. Without it, SRTP runs software AES — a big silent tax. One-line, high impact.
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
