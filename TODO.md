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

## Deliberate non-goals (for reference — do NOT treat as gaps)

- No FreeSWITCH/Asterisk config front-end; no raw SIP/SDP/codec exposure by default (N-1, N-5).
- No bug-for-bug dialplan compatibility; the module/AGI ecosystem is out of scope (N-4).
- No bundled LLM/ASR/TTS — AI is an external event consumer (N-2).
- No mandatory Kubernetes/broker/cloud — single binary + SQLite/Postgres; HA is scale-out (N-3).
