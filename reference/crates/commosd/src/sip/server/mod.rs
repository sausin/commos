//! The UDP SIP signalling ingress (Volume 7) — the front door a real softphone talks to.
//!
//! Parses each datagram with [`super::message`] and dispatches by method:
//! - **REGISTER** binds the AoR→contact in the [`RegistrationRegistry`] (a phone appears in
//!   the platform).
//! - **INVITE** creates an inbound [`Call`](commos_core::entities::call::Call) in the control
//!   plane, reports ring+answer as media facts, sets up an RTP echo path, and answers
//!   `200 OK` with an SDP answer — a caller can place a call and hear themselves.
//! - **BYE/CANCEL** hangs the Call up (which produces the CDR), aborts its RTP, and `200`s.
//! - **OPTIONS** `200`s; **ACK** is silent; anything else `501`s.
//!
//! Robustness is a hard requirement: a malformed datagram is logged at debug and dropped —
//! it must never break the receive loop.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tokio::time::Duration;

use commos_core::common::{Timestamp, Uuid};
use commos_core::entities::gateway::{Gateway, GatewayHealth, GatewayKind};
use commos_core::entities::ivr::Ivr;

use crate::config::OnDecline;
use crate::control::dialplan;
use crate::control::ivr::IvrService;
use crate::control::objects::ObjectService;
use crate::control::recordings::RecordingService;
use crate::control::registrations::{Registration, RegistrationRegistry};
use crate::control::routing::Routing;
use crate::control::voicemail::VoicemailService;
use crate::media::MediaFact;
use crate::store::Store;

/// Current unix time in seconds (for nonce expiry).
fn now_unix() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

use super::message::{self, SipMessage};
use super::transport::Responder;
use super::{codec, dtmf, g711, ivr, rtp, sdes, srtp};

/// Largest UDP SIP datagram we accept (the UDP ceiling; ample for INVITE+SDP).
const MAX_DATAGRAM: usize = 65_535;

/// Cap an IVR-deposited voicemail recording (~2 min of G.711) so an abandoned line can't
/// record forever; a graceful hangup (BYE) stops it sooner.
const IVR_VOICEMAIL_MAX: Duration = Duration::from_secs(120);

/// Wall-clock length of one "ring" of the standard ringback cadence (~2 s on + ~4 s off). The
/// configured `no_answer_rings` is multiplied by this to get the no-answer timeout, so the
/// operator can reason in rings while the wait is measured in time.
const SECONDS_PER_RING: u64 = 6;

/// How long the `*97`/`*98` retrieval menu waits for the caller's per-message action key
/// (7 delete / 9 save / # next) after playing a message before advancing.
const VM_MENU_TIMEOUT: Duration = Duration::from_secs(6);

/// RFC 3261 §17.1.1 initial retransmit interval (T1). Requests are re-sent at T1, 2·T1, 4·T1 …
/// (capped at [`T2`]) until a response arrives, so a lost request on UDP is recovered.
const T1: Duration = Duration::from_millis(500);
/// RFC 3261 retransmit interval ceiling (T2).
const T2: Duration = Duration::from_secs(4);
/// Overall budget for a reliable non-INVITE transaction (BYE) — a few retransmits, then give up.
const NON_INVITE_TIMEOUT: Duration = Duration::from_secs(5);

/// The media path backing a dialog: either the single-socket echo (PSTN-style / non-registered
/// destinations) or a two-leg [`Bridge`](rtp::Bridge) between caller and a registered callee.
enum Media {
    /// Echo test: one UDP socket reflecting RTP back to the caller.
    Echo(JoinHandle<()>),
    /// A live two-leg RTP relay between the caller and the callee.
    Bridge(rtp::Bridge),
    /// An IVR menu session (prompt playback + DTMF collection). Torn down *gracefully* — a
    /// hangup signals `stop` so an in-progress voicemail deposit is saved before the task
    /// exits — rather than hard-aborted; the detached task then finishes on its own.
    Ivr {
        task: JoinHandle<()>,
        stop: tokio::sync::watch::Sender<bool>,
    },
}

impl Media {
    /// Tear the media plane down. Echo/Bridge abort immediately; an IVR session is asked to
    /// stop gracefully (so it can persist a voicemail deposit) and detached.
    fn abort(self) {
        match self {
            Media::Echo(task) => task.abort(),
            Media::Bridge(bridge) => bridge.abort(),
            Media::Ivr { task, stop } => {
                let _ = stop.send(true);
                drop(task); // detach: the task drains its stop signal and exits itself
            }
        }
    }
}

/// One leg of a bridged (B2BUA) call — the dialog identifiers we need to send a mid-dialog BYE
/// toward that leg's endpoint. Used for the **callee** leg (BYE it when the caller hangs up) and,
/// symmetrically, for the **caller** leg (BYE it when the callee hangs up), so hanging up on
/// either phone tears the other one down.
///
/// TODO(B2BUA): this is best-effort. We reconstruct a BYE from the identifiers captured when
/// the leg was set up, but full RFC 3261 mid-dialog correctness (route sets, contact
/// refresh, robust CSeq accounting) is not implemented.
struct CalleeLeg {
    /// Where to send requests toward the callee (its Contact's host:port).
    addr: SocketAddr,
    /// Request-URI to use for the mid-dialog BYE (the callee's Contact).
    request_uri: String,
    /// Our `From` header value on the outbound leg (with our tag).
    from: String,
    /// The callee's `To` header value from its 200 OK (with the callee's tag).
    to: String,
    /// The outbound leg's `Call-ID`.
    call_id: String,
    /// The CSeq number used for the outbound INVITE; the BYE uses `cseq + 1`.
    cseq: u32,
}

/// The calling party's identity, carried from the inbound INVITE onto the outbound (callee) leg so
/// the callee's phone shows who is really calling. `number` is the caller's user-part and `display`
/// its display name (either may be absent for an anonymous/malformed caller).
#[derive(Clone, Copy, Default)]
struct CallerId<'a> {
    number: Option<&'a str>,
    display: Option<&'a str>,
}

/// The outcome of an outbound bridge attempt ([`SipServer::try_bridge`]). Distinguishes an
/// *active* callee rejection (`486 Busy Here` / `600` / `603 Decline` — the Decline button) from
/// a plain no-answer/failure, so the direct-call path can treat a decline differently (announce /
/// relay busy / voicemail) instead of silently folding it into "did not answer".
// A short-lived move value returned straight up the call stack (never stored in a collection), so
// the size gap between `Answered` and the unit variants doesn't matter — boxing would just add an
// allocation on the answered hot path.
#[allow(clippy::large_enum_variant)]
enum BridgeOutcome {
    /// The callee answered (2xx): the live bridge + callee leg + negotiated codec/DTMF/SRTP.
    Answered(
        rtp::Bridge,
        CalleeLeg,
        codec::Codec,
        u8,
        Option<sdes::CryptoAttr>,
    ),
    /// The callee actively declined with the carried final status (`486`/`600`/`603`).
    Declined(u16),
    /// No answer within the ring timeout, or any setup failure (unresolvable/bind/cancel).
    NoAnswer,
}

/// Preloaded audio prompts for the `*97`/`*98` retrieval session, already transcoded to the
/// negotiated codec. An empty buffer means the file is not installed — playback simply skips it,
/// so retrieval still works (via DTMF) with no sound pack. Preloaded in the request context (which
/// has `&self`) and moved into the spawned driver.
#[derive(Default)]
struct RetrievalPrompts {
    /// "You have"
    youhave: Vec<u8>,
    /// "messages" (plural)
    messages: Vec<u8>,
    /// "message" (singular)
    message: Vec<u8>,
    /// "No more messages."
    no_more: Vec<u8>,
    /// "Message deleted."
    deleted: Vec<u8>,
    /// "Please enter the mailbox number" (for *98).
    enter_mailbox: Vec<u8>,
    /// Spoken digits 0–9 (index = digit) for the count announcement.
    digits: Vec<Vec<u8>>,
}

/// The mailbox a voicemail dialog is recording for. Set on the no-answer / offline-callee
/// path; on hangup the captured audio is stored as a [`Voicemail`] and a message-waiting
/// indication is pushed to the phone.
struct VoicemailBox {
    /// Mailbox address-of-record (e.g. `sip:200@host`); its user-part keys the MWI summary.
    aor: String,
    /// Where to push the MWI NOTIFY as soon as the voicemail is stored — the phone's contact
    /// `(address, request-URI)` — when the mailbox is currently registered. `None` for an
    /// offline mailbox, whose MWI is delivered on its next REGISTER instead.
    notify: Option<(SocketAddr, String)>,
}

/// Per-INVITE state, keyed by the SIP `Call-ID`, so BYE/CANCEL can find the Call and its
/// media. For a bridged call, `callee` carries the second leg so a BYE tears down both sides.
struct Dialog {
    call_id: Uuid,
    media: Media,
    /// Present only for bridged (internal) calls; `None` for the echo path.
    callee: Option<CalleeLeg>,
    /// The caller leg's dialog identifiers, for a bridged/trunked call — so a BYE from the
    /// *callee* can be propagated to the caller and hang its phone up too. `None` for
    /// echo/voicemail/IVR dialogs (there is no second party to originate a BYE).
    caller: Option<CalleeLeg>,
    /// Shared RTP capture buffer when recording is on; `None` when the call is not recorded.
    /// On hangup the buffer's bytes are persisted as a [`Recording`] — or, when `voicemail`
    /// is set, as a [`Voicemail`].
    capture: Option<rtp::Capture>,
    /// Set when this dialog is a voicemail (the callee did not answer or is offline); drives
    /// voicemail storage + MWI on hangup instead of ordinary call recording.
    voicemail: Option<VoicemailBox>,
    /// For an active IVR dialog, the channel that injects SIP INFO DTMF digits into the running
    /// menu session; `None` for echo/bridge/voicemail dialogs.
    info_tx: Option<tokio::sync::mpsc::UnboundedSender<char>>,
    /// The SDP body CommOS answered this dialog's INVITE with, so a retransmitted INVITE (our 200
    /// was lost) or a re-INVITE (media refresh / hold) is re-answered idempotently — replaying the
    /// same media — instead of creating a duplicate Call.
    answer_sdp: String,
}

/// The UDP SIP server. [`Self::run`] takes ownership and drives the receive loop.
/// Per-nonce replay-protection state: expiry plus the highest digest nonce-count (`nc`) we have
/// already accepted for it. A captured, validly-signed request cannot be replayed because its
/// `nc` is no longer strictly greater than what we have seen (or, for clients that send no
/// `nc`, the nonce is consumed single-use on first success).
#[derive(Clone, Copy)]
struct NonceState {
    exp: i64,
    highest_nc: u32,
}

pub struct SipServer {
    registrations: RegistrationRegistry,
    routing: Routing,
    /// IP advertised in SDP `c=`/`o=` lines. Set to the server's reachable address for real
    /// phones; `127.0.0.1` suffices for a loopback echo test.
    media_ip: IpAddr,
    /// The tenant every request on this ingress is attributed to (single-tenant
    /// simplification; SIP-domain→tenant mapping is Volume 9).
    default_tenant: Uuid,
    /// Active dialogs by SIP `Call-ID` (the caller-leg Call-ID is the primary key).
    dialogs: Arc<Mutex<HashMap<String, Dialog>>>,
    /// Maps a bridged call's **callee-leg** Call-ID → its **caller-leg** (primary) Call-ID,
    /// so a BYE arriving on the callee leg can find and tear down the same dialog.
    bye_aliases: Arc<Mutex<HashMap<String, String>>>,
    /// The durable store, for SIP digest credential lookup.
    store: Arc<dyn Store>,
    /// Require SIP digest auth on REGISTER/INVITE (Volume 9). Auth is *additionally* required
    /// for any request from an untrusted (public) source address regardless of this flag, so an
    /// internet-reachable SIP port is never open to unauthenticated REGISTER/INVITE.
    require_auth: bool,
    /// Digest realm advertised in the auth challenge.
    realm: String,
    /// Nonces we have issued → their replay-protection state. In-memory; a restart re-challenges.
    nonces: Arc<Mutex<HashMap<String, NonceState>>>,
    /// Record calls (Volume 7): capture the caller's RTP audio and persist it on hangup.
    record_calls: bool,
    /// Recording service used to store captured audio when `record_calls` is on.
    recordings: RecordingService,
    /// Take a voicemail when an internal callee does not answer / is offline (Volume 7).
    voicemail_enabled: bool,
    /// How to treat a caller when the called extension actively declines (SIP `486`/`603`), as
    /// distinct from a plain no-answer. See [`OnDecline`] / `Config::on_decline`.
    on_decline: OnDecline,
    /// How long a called extension rings before an unanswered call diverts to voicemail/echo.
    /// Derived from the configured `no_answer_rings` (× [`SECONDS_PER_RING`]).
    no_answer_timeout: Duration,
    /// Voicemail service used to store captured audio and drive the MWI summary.
    voicemails: VoicemailService,
    /// IVR service — resolve an `ivr:<id>` routing target to its menu definition.
    ivrs: IvrService,
    /// Object service — fetch an IVR's prompt audio Object for playback.
    objects: ObjectService,
    /// Home country code (digits) used to classify a dialled number as external (E.164) for
    /// outbound trunk routing and to normalise inbound DID numbers.
    default_cc: String,
    /// Encrypt the endpoint media path with SRTP when a caller offers `RTP/SAVP` + SDES
    /// ([`srtp`]/[`sdes`], RFC 3711/4568). Plain-RTP callers are unaffected.
    srtp_enabled: bool,
    /// Also offer SRTP toward an outbound carrier trunk (default off — carrier SRTP support is
    /// inconsistent and an `RTP/SAVP` offer a carrier can't answer would fail the call).
    trunk_srtp: bool,
    /// Absolute directory of audio prompt files (`<sounds_dir>/en/<name>.ulaw`), resolved once at
    /// boot from config (`{data_dir}/sounds` by default). Used for the voicemail greeting and the
    /// `*97`/`*98` retrieval menu; a missing file falls back to a synthesized tone.
    sounds_dir: String,
    /// Path to the operator's phone display-name file (`{data_dir}/display_name.txt` by default):
    /// the text a called phone shows as the calling party instead of the bare "commos". Re-read
    /// per call so edits apply live; absent/empty → the default "commos".
    display_name_file: String,
    /// Music-on-hold source (loaded once at boot from `{data_dir}/moh`, or synthesised) and
    /// whether hold music is enabled. Streamed to a queue-waiting caller (see the queue-wait
    /// driver); the splice into the live two-leg hold bridge is a documented follow-up.
    moh: Arc<super::moh::MohSource>,
    music_on_hold: bool,
    /// Per-call rotation counter for hunt-group / round-robin member ordering (the pure ring
    /// planner takes this as its rotation input, spreading load across successive calls).
    ring_rotation: Arc<std::sync::atomic::AtomicUsize>,
}

// The `impl SipServer` is split across these topic modules (each an `impl SipServer` block);
// this file keeps the struct, its helper types, the constructor, and the receive loop/dispatch
// (`run`/`handle`/`reply`). Methods are `pub(super)` so they remain visible across the whole
// `server` module tree. See each submodule's header for what it owns.
mod auth; // digest auth: nonce issue/replay-guard + challenge
mod bridge; // media plane: echo, two-leg bridge, ring-group fork, trunk, recording
mod decline; // callee-decline announcement + leave-a-message path
mod handlers; // non-INVITE methods: REGISTER, INFO, BYE/CANCEL
mod invite; // the inbound INVITE handler (core routing decision)
mod ivr_menu; // IVR menu playout + driver
mod queue; // call-queue answer path + queue-wait driver
mod routing; // destination resolution + outbound gateway selection
mod sdp; // SDP/SRTP negotiation + SIP response construction
mod voicemail; // voicemail deposit/retrieval, storage, MWI

impl SipServer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registrations: RegistrationRegistry,
        routing: Routing,
        media_ip: IpAddr,
        default_tenant: Uuid,
        store: Arc<dyn Store>,
        require_auth: bool,
        realm: impl Into<String>,
        record_calls: bool,
        recordings: RecordingService,
        voicemail_enabled: bool,
        on_decline: OnDecline,
        no_answer_rings: u32,
        voicemails: VoicemailService,
        ivrs: IvrService,
        objects: ObjectService,
        default_cc: impl Into<String>,
        srtp_enabled: bool,
        trunk_srtp: bool,
        sounds_dir: impl Into<String>,
        display_name_file: impl Into<String>,
        moh: Arc<super::moh::MohSource>,
        music_on_hold: bool,
    ) -> Self {
        SipServer {
            registrations,
            routing,
            media_ip,
            default_tenant,
            dialogs: Arc::new(Mutex::new(HashMap::new())),
            bye_aliases: Arc::new(Mutex::new(HashMap::new())),
            store,
            require_auth,
            realm: realm.into(),
            nonces: Arc::new(Mutex::new(HashMap::new())),
            record_calls,
            recordings,
            voicemail_enabled,
            on_decline,
            no_answer_timeout: Duration::from_secs(
                no_answer_rings.max(1) as u64 * SECONDS_PER_RING,
            ),
            voicemails,
            ivrs,
            objects,
            default_cc: default_cc.into(),
            srtp_enabled,
            trunk_srtp,
            sounds_dir: sounds_dir.into(),
            display_name_file: display_name_file.into(),
            moh,
            music_on_hold,
            ring_rotation: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Bind `bind` and serve SIP over UDP forever. Returns only on a fatal socket error. Shared
    /// behind an [`Arc`] so the same server also drives the TLS ingress ([`super::tls`]).
    pub async fn run(self: Arc<Self>, bind: SocketAddr) -> std::io::Result<()> {
        let socket = Arc::new(UdpSocket::bind(bind).await?);
        let local = socket.local_addr().unwrap_or(bind);
        tracing::info!(addr = %local, "SIP signalling ingress listening (UDP)");

        let mut buf = vec![0u8; MAX_DATAGRAM];
        loop {
            let (len, src) = match socket.recv_from(&mut buf).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::debug!(error = %e, "SIP recv_from error; continuing");
                    continue;
                }
            };
            // Parse + dispatch on a detached task, never inline. `on_invite` blocks for up to
            // `no_answer_timeout` (~30 s) while ringing the callee, so awaiting `handle` here
            // would serialize *all* call setup on this one core — a single ringing phone would
            // freeze every other INVITE/REGISTER/BYE. Shared state is `Arc`/`Mutex`, so handing
            // each transaction to `tokio::spawn` lets setups run concurrently across all cores.
            // The datagram is copied because the receive buffer is reused on the next iteration.
            let responder = Responder::Udp {
                socket: socket.clone(),
                dst: src,
            };
            let datagram = buf[..len].to_vec();
            let server = self.clone();
            tokio::spawn(async move {
                if let Err(e) = server.handle(&responder, &datagram).await {
                    tracing::debug!(error = %e, %src, "dropping SIP datagram");
                }
            });
        }
    }

    /// Parse and dispatch one received SIP message, replying via `resp`. Transport-agnostic: the
    /// same path serves a UDP datagram and a message framed off a TLS stream ([`super::tls`]).
    pub(crate) async fn handle(&self, resp: &Responder, datagram: &[u8]) -> std::io::Result<()> {
        let src = resp.peer();
        let msg = match message::parse(datagram) {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(error = %e, %src, "unparseable SIP message");
                return Ok(());
            }
        };

        let method = match msg.method() {
            Some(m) => m.to_string(),
            None => {
                tracing::debug!(%src, status = ?msg.status(), "ignoring SIP response");
                return Ok(());
            }
        };

        match method.as_str() {
            "REGISTER" => self.on_register(resp, &msg).await,
            "OPTIONS" => {
                tracing::info!(method = %method, %src, "SIP OPTIONS");
                self.reply(resp, &msg, 200, "OK").await
            }
            "INVITE" => self.on_invite(resp, &msg).await,
            "ACK" => {
                tracing::info!(method = %method, %src, "SIP ACK");
                Ok(())
            }
            "BYE" | "CANCEL" => self.on_bye(resp, &msg).await,
            "INFO" => self.on_info(resp, &msg).await,
            other => {
                tracing::info!(method = %other, %src, "SIP method not implemented");
                self.reply(resp, &msg, 501, "Not Implemented").await
            }
        }
    }

    async fn reply(
        &self,
        resp: &Responder,
        msg: &SipMessage,
        status: u16,
        reason: &str,
    ) -> std::io::Result<()> {
        resp.send(message::response(msg, status, reason).as_bytes())
            .await
    }
}

/// The user-part of a SIP URI: `sip:200@example.com` → `200`. Tolerates a leading `<` and
/// the `sip:`/`sips:`/`tel:` schemes. Returns `None` for a domain-only URI (no `@`).
/// Parse a digest `nc` (nonce-count) value — up to 8 hex digits per RFC 2617 — into a number
/// for the replay guard. Returns `None` for a missing/malformed value (treated as "no nc").
fn parse_nc(s: &str) -> Option<u32> {
    let t = s.trim();
    if t.is_empty() || t.len() > 8 || !t.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(t, 16).ok()
}

fn user_part(uri: &str) -> Option<&str> {
    let s = uri
        .trim()
        .trim_start_matches('<')
        .trim_start_matches("sips:")
        .trim_start_matches("sip:")
        .trim_start_matches("tel:");
    let user = s.split_once('@')?.0.trim();
    if user.is_empty() {
        None
    } else {
        Some(user)
    }
}

/// Pick which display-name line to use for a call when the file has several, varied per call so
/// the messages rotate. Derived from the call id's random bits (UUIDv7), so it is stable for a
/// given call but differs between calls without needing an RNG.
fn display_line_index(call_id: Uuid, n: usize) -> usize {
    let sum: u32 = call_id.to_string().bytes().map(u32::from).sum();
    (sum as usize) % n.max(1)
}

/// Sanitise operator-provided text into a SIP display-name **quoted-string** payload (without the
/// surrounding quotes): drop control characters (so it can't inject headers), escape `\` and `"`
/// per RFC 3261, and cap the length so a stray huge line can't bloat every INVITE.
fn sip_display_name(raw: &str) -> String {
    let mut out = String::new();
    for c in raw.chars() {
        if c.is_control() {
            continue;
        }
        if c == '\\' || c == '"' {
            out.push('\\');
        }
        out.push(c);
        if out.len() >= 64 {
            break;
        }
    }
    out.trim().to_string()
}

/// Build a CommOS outbound-leg `From` header value, prefixing the configurable display name (the
/// text a called phone shows as the calling party) when one is set. `tag` is the leg's from-tag.
/// With no display name it is the bare `<sip:commos@host>;tag=…` as before.
fn commos_from_header(media_ip: IpAddr, display: Option<&str>, tag: &str) -> String {
    match display {
        Some(d) if !d.is_empty() => format!("\"{d}\" <sip:commos@{media_ip}>;tag={tag}"),
        _ => format!("<sip:commos@{media_ip}>;tag={tag}"),
    }
}

/// Build the outbound-leg `From` that presents the **caller's** identity to a bridged callee, so
/// the callee's phone shows who is really calling (its number, and its display name when the caller
/// supplied one) rather than the bare "commos" service identity. `number` is the caller's
/// user-part; the URI host is CommOS (`media_ip`), since the B2BUA is the caller's contact. `tag`
/// is the outbound leg's from-tag. Falls back to the plain service `From` when the caller has no
/// usable number (so an anonymous/malformed caller still gets a well-formed header).
fn caller_from_header(
    media_ip: IpAddr,
    number: Option<&str>,
    display: Option<&str>,
    tag: &str,
) -> String {
    let Some(number) = number.filter(|n| !n.is_empty()) else {
        return commos_from_header(media_ip, display, tag);
    };
    match display {
        Some(d) if !d.is_empty() => format!("\"{d}\" <sip:{number}@{media_ip}>;tag={tag}"),
        _ => format!("<sip:{number}@{media_ip}>;tag={tag}"),
    }
}

/// Extract the display-name (the quoted or bare text before the `<uri>`) from a `From`/`To` header
/// value, sanitized for re-emission. Returns `None` when there is no display name (a bare
/// `<sip:…>` or `sip:…` value). Used to carry the caller's name through to the bridged callee.
fn header_display_name(value: &str) -> Option<String> {
    let v = value.trim();
    let raw = if let Some(rest) = v.strip_prefix('"') {
        // Quoted form: "Alice" <sip:…>. Take up to the closing quote.
        rest.split('"').next().unwrap_or("")
    } else if let Some(idx) = v.find('<') {
        // Unquoted display name before the angle-bracketed URI (e.g. `Alice <sip:…>`).
        &v[..idx]
    } else {
        // Bare URI (`sip:100@host` or `<sip:100@host>`): no display name.
        ""
    };
    let name = sip_display_name(raw);
    (!name.is_empty()).then_some(name)
}

/// A default codec (PCMU/8000) for when an offer carries no usable audio codec.
fn default_codec() -> codec::Codec {
    codec::Codec {
        pt: 0,
        name: "PCMU".to_string(),
        clock: 8000,
    }
}

/// Answer a peer's SDES key: generate CommOS's own fresh key and the [`srtp::SrtpSession`] for
/// that leg — `inbound` decrypts what the peer sends (its key), `outbound` encrypts what CommOS
/// sends (our key). Returns the `a=crypto` attribute to advertise back (echoing the peer's tag).
fn srtp_answer(theirs: &sdes::CryptoAttr) -> (sdes::CryptoAttr, srtp::SrtpSession) {
    let (their_key, their_salt) = srtp::split_key_salt(&theirs.key_salt);
    let ours = srtp::random_key_salt();
    let (our_key, our_salt) = srtp::split_key_salt(&ours);
    let session = srtp::SrtpSession {
        inbound: srtp::SrtpContext::new(&their_key, &their_salt),
        outbound: srtp::SrtpContext::new(&our_key, &our_salt),
    };
    (
        sdes::CryptoAttr {
            tag: theirs.tag,
            key_salt: ours,
        },
        session,
    )
}

/// Pair a peer's SDES key (from its SDP answer) with the key CommOS **offered** it, into the
/// [`srtp::SrtpSession`] for that leg: `inbound` decrypts the peer's key, `outbound` encrypts with
/// the offered key. Used for the callee/carrier leg, where CommOS is the offerer.
fn srtp_offered(
    offered: &[u8; srtp::KEY_SALT_LEN],
    theirs: &sdes::CryptoAttr,
) -> srtp::SrtpSession {
    let (their_key, their_salt) = srtp::split_key_salt(&theirs.key_salt);
    let (our_key, our_salt) = srtp::split_key_salt(offered);
    srtp::SrtpSession {
        inbound: srtp::SrtpContext::new(&their_key, &their_salt),
        outbound: srtp::SrtpContext::new(&our_key, &our_salt),
    }
}

/// The SDP body advertising `port` at `media_ip` for a single negotiated `audio` codec plus RFC
/// 4733 `telephone-event/8000` at `te_pt` (for in-band DTMF). Used for the answer CommOS sends
/// the caller and for the single-codec offer on the IVR-transfer leg. When `crypto` is `Some`, the
/// media is offered over the secure `RTP/SAVP` profile with an SDES `a=crypto` key (SRTP).
fn media_sdp(
    media_ip: IpAddr,
    port: u16,
    audio: &codec::Codec,
    te_pt: u8,
    crypto: Option<&sdes::CryptoAttr>,
) -> String {
    let (proto, crypto_line) = match crypto {
        Some(c) => ("RTP/SAVP", format!("{}\r\n", c.to_line())),
        None => ("RTP/AVP", String::new()),
    };
    format!(
        "v=0\r\n\
         o=commos 0 0 IN IP4 {ip}\r\n\
         s=CommOS\r\n\
         c=IN IP4 {ip}\r\n\
         t=0 0\r\n\
         m=audio {port} {proto} {apt} {te}\r\n\
         a=rtpmap:{apt} {rtpmap}\r\n\
         a=rtpmap:{te} telephone-event/8000\r\n\
         a=fmtp:{te} 0-16\r\n\
         {crypto_line}\
         a=sendrecv\r\n",
        ip = media_ip,
        port = port,
        apt = audio.pt,
        rtpmap = audio.rtpmap(),
        te = te_pt,
    )
}

/// The SDP body **re-offering** the caller's full codec list at `port` (plus a telephone-event
/// line) to the far end of a bridge/trunk — so caller and callee converge on a shared codec that
/// CommOS relays untouched (transparent pass-through, no transcoding). Falls back to a PCMU offer
/// when the caller advertised no audio codecs. When `crypto` is `Some`, the far leg is offered the
/// secure `RTP/SAVP` profile with an SDES key, extending SRTP to the callee/carrier leg.
fn reoffer_sdp(
    media_ip: IpAddr,
    port: u16,
    offer: &codec::AudioMedia,
    crypto: Option<&sdes::CryptoAttr>,
) -> String {
    let te = offer
        .telephone_event_pt()
        .unwrap_or(dtmf::TELEPHONE_EVENT_PT);
    let (pts, rtpmaps) = offer.reoffer_lines();
    let (pts, rtpmaps) = if pts.trim().is_empty() {
        (
            format!("0 {te}"),
            format!("a=rtpmap:0 PCMU/8000\r\na=rtpmap:{te} telephone-event/8000\r\n"),
        )
    } else {
        (pts, rtpmaps)
    };
    let (proto, crypto_line) = match crypto {
        Some(c) => ("RTP/SAVP", format!("{}\r\n", c.to_line())),
        None => ("RTP/AVP", String::new()),
    };
    format!(
        "v=0\r\n\
         o=commos 0 0 IN IP4 {ip}\r\n\
         s=CommOS\r\n\
         c=IN IP4 {ip}\r\n\
         t=0 0\r\n\
         m=audio {port} {proto} {pts}\r\n\
         {rtpmaps}\
         {crypto_line}\
         a=sendrecv\r\n",
        ip = media_ip,
        port = port,
    )
}

/// Parse an `ivr:<uuidv7>` destination reference to the IVR id, else `None`.
fn ivr_id_of(dest: &str) -> Option<Uuid> {
    Uuid::parse(dest.trim().strip_prefix("ivr:")?.trim()).ok()
}

/// The dial-target user-part of an IVR `destination_ref` — leniently, since an option value may
/// be `sip:200@host`, a bare `200`, or `ext:200`. Returns `None` for non-endpoint targets like
/// `queue:sales` (which carry a `:` and match no registration user-part).
fn dial_target(dest: &str) -> Option<&str> {
    let s = dest
        .trim()
        .trim_start_matches("ext:")
        .trim_start_matches('<')
        .trim_start_matches("sips:")
        .trim_start_matches("sip:")
        .trim_start_matches("tel:");
    let user = match s.split_once('@') {
        Some((u, _)) => u,
        None => s,
    };
    let user = user.split(['>', ';']).next().unwrap_or(user).trim();
    (!user.is_empty()).then_some(user)
}

/// Find a currently-registered endpoint whose AoR user-part matches an IVR `destination_ref`.
fn find_registered(regs: &RegistrationRegistry, tenant: Uuid, dest: &str) -> Option<Registration> {
    let want = dial_target(dest)?;
    regs.list(tenant)
        .into_iter()
        .find(|r| user_part(&r.aor).is_some_and(|u| u.eq_ignore_ascii_case(want)))
}

/// Compose the "you have N message(s)" announcement from preloaded prompt pieces: "You have" +
/// the spoken digit + "message"/"messages". Any missing piece is simply skipped; if nothing is
/// installed the result is empty and the caller hears no count (the menu still works via DTMF).
fn build_count_prompt(prompts: &RetrievalPrompts, count: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&prompts.youhave);
    if let Some(digit) = prompts.digits.get(count) {
        buf.extend_from_slice(digit);
    }
    buf.extend_from_slice(if count == 1 {
        &prompts.message
    } else {
        &prompts.messages
    });
    buf
}

/// Play `prompt` and collect a string of DTMF digits terminated by `#` (or a timeout), for the
/// `*98` "enter mailbox number" step. Returns the digits entered (without the `#`), or `None` if
/// nothing was entered. Latches the caller's RTP `peer` (persisted across the collection).
async fn collect_digits(
    sock: &UdpSocket,
    prompt: &[u8],
    audio_pt: u8,
    te_pt: u8,
    info_rx: &mut tokio::sync::mpsc::UnboundedReceiver<char>,
    peer: &mut Option<SocketAddr>,
) -> Option<String> {
    let mut entered = String::new();
    // First digit: play the prompt while collecting. Subsequent digits: short inter-digit window.
    let mut this_prompt: &[u8] = prompt;
    loop {
        let window = if entered.is_empty() {
            Duration::from_millis((prompt.len() as u64 / 8) + 5000)
        } else {
            Duration::from_secs(4)
        };
        match ivr::play_and_collect(sock, this_prompt, audio_pt, te_pt, window, info_rx, peer).await
        {
            Some('#') => break,
            Some(d) if d.is_ascii_digit() => {
                entered.push(d);
                this_prompt = &[]; // only play the prompt once
                if entered.len() >= 12 {
                    break; // guard against runaway input
                }
            }
            // A non-digit, non-# key is ignored; a timeout ends collection.
            Some(_) => {
                this_prompt = &[];
            }
            None => break,
        }
    }
    (!entered.is_empty()).then_some(entered)
}

/// Bridge an in-progress IVR caller to a registered `callee`, mid-call, with no re-INVITE to the
/// caller: the IVR's own socket `sock_a` (caller already latched at `peer_a`) becomes leg A, and
/// a fresh leg-B socket is offered to the callee via an outbound INVITE. Once the callee answers,
/// RTP is relayed A↔B until `stop` (the caller hangs up), when a BYE tears the callee leg down.
///
/// Returns `true` if the call was bridged (and has now ended), `false` if the callee could not be
/// reached (the caller should then be held / hung up). Blind, media-plane transfer — the caller's
/// phone is untouched at the signalling layer, so it works with any endpoint.
///
/// TODO(B2BUA): the outbound leg is best-effort (as [`SipServer::try_bridge`]); full mid-dialog
/// correctness (transactions, retransmission, ringback tone during setup) is future work.
#[allow(clippy::too_many_arguments)]
async fn ivr_transfer(
    sock_a: &UdpSocket,
    peer_a: SocketAddr,
    callee: &Registration,
    media_ip: IpAddr,
    call_id: Uuid,
    g711: g711::G711,
    te_pt: u8,
    no_answer_timeout: Duration,
    display: Option<String>,
    stop_rx: &mut tokio::sync::watch::Receiver<bool>,
) -> bool {
    let addr = match resolve_contact_addr(&callee.contact).await {
        Some(a) => a,
        None => {
            tracing::warn!(contact = %callee.contact, "IVR transfer: callee contact unresolvable");
            return false;
        }
    };
    // Leg B (callee RTP) and a throwaway signalling socket for the outbound dialog.
    let (sock_b, sig) = match (
        UdpSocket::bind("0.0.0.0:0").await,
        UdpSocket::bind("0.0.0.0:0").await,
    ) {
        (Ok(b), Ok(s)) => (b, s),
        _ => {
            tracing::warn!("IVR transfer: could not bind media/signalling sockets");
            return false;
        }
    };
    let leg_b_port = match sock_b.local_addr() {
        Ok(a) => a.port(),
        Err(_) => return false,
    };

    // Outbound-leg dialog identifiers, derived from the CommOS Call id (mirrors try_bridge).
    let leg_call_id = format!("{}@commos-ivr", call_id.to_string().replace('-', ""));
    let from_tag: String = call_id
        .to_string()
        .chars()
        .filter(|c| *c != '-')
        .take(16)
        .collect();
    let from_hdr = commos_from_header(media_ip, display.as_deref(), &from_tag);
    // Offer the callee the IVR caller's negotiated codec (the caller is already on it).
    let audio = codec::Codec {
        pt: g711.payload_type(),
        name: g711.sdp_name().to_string(),
        clock: 8000,
    };
    let sdp = media_sdp(media_ip, leg_b_port, &audio, te_pt, None);
    let invite = message::request(
        "INVITE",
        &callee.contact,
        &[
            ("From", from_hdr.clone()),
            ("To", format!("<{}>", callee.aor)),
            ("Call-ID", leg_call_id.clone()),
            ("CSeq", "1 INVITE".to_string()),
            ("Contact", format!("<sip:commos@{media_ip}>")),
        ],
        Some(("application/sdp", &sdp)),
    );
    if sig.send_to(invite.as_bytes(), addr).await.is_err() {
        return false;
    }

    // Await a 2xx (ignoring provisional 1xx) up to the answer timeout, **playing ring-back to
    // the caller** on leg A meanwhile so they hear audible ring instead of silence while the
    // callee's phone rings. Ring-back loops the standard 440+480 Hz / 2 s-on-4 s-off cadence.
    let mut buf = vec![0u8; MAX_DATAGRAM];
    let ringback = g711::ringback(g711);
    let mut rb_pos = 0usize;
    let mut rb_seq: u16 = 0;
    let mut rb_ts: u32 = 0;
    let mut rb_first = true;
    let mut ticker = tokio::time::interval(Duration::from_millis(20));
    let ring_deadline = tokio::time::sleep(no_answer_timeout);
    tokio::pin!(ring_deadline);
    let resp = loop {
        tokio::select! {
            _ = &mut ring_deadline => {
                tracing::info!(callee = %callee.aor, "IVR transfer: callee did not answer");
                return false;
            }
            r = sig.recv_from(&mut buf) => {
                let Ok((n, _from)) = r else { continue };
                let msg = match message::parse(&buf[..n]) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                match msg.status() {
                    Some(s) if (100..200).contains(&s) => continue,     // provisional ring
                    Some(s) if (200..300).contains(&s) => break msg,    // answered
                    Some(_) => {                                        // callee rejected/failed
                        tracing::info!(callee = %callee.aor, "IVR transfer: callee rejected");
                        return false;
                    }
                    None => continue,
                }
            }
            _ = ticker.tick() => {
                // Send the next 20 ms ring-back frame to the caller, wrapping the cadence buffer.
                let mut frame = [0u8; 160];
                for b in frame.iter_mut() {
                    *b = ringback[rb_pos % ringback.len()];
                    rb_pos += 1;
                }
                let pkt = ivr::rtp_frame(g711.payload_type(), rb_seq, rb_ts, &frame, rb_first);
                let _ = sock_a.send_to(&pkt, peer_a).await;
                rb_seq = rb_seq.wrapping_add(1);
                rb_ts = rb_ts.wrapping_add(160);
                rb_first = false;
            }
        }
    };
    let callee_to = resp
        .header("To")
        .map(str::to_string)
        .unwrap_or_else(|| format!("<{}>", callee.aor));
    let callee_target = resp
        .header("Contact")
        .and_then(extract_uri)
        .unwrap_or_else(|| callee.contact.clone());
    let ack = message::request(
        "ACK",
        &callee_target,
        &[
            ("From", from_hdr.clone()),
            ("To", callee_to.clone()),
            ("Call-ID", leg_call_id.clone()),
            ("CSeq", "1 ACK".to_string()),
        ],
        None,
    );
    let _ = sig.send_to(ack.as_bytes(), addr).await;
    tracing::info!(%call_id, callee = %callee.aor, leg_b_port, "IVR transfer: bridged to registered callee");

    // Relay RTP: caller (sock_a/peer_a) ↔ callee (sock_b/peer_b, latched on first packet).
    let mut peer_b: Option<SocketAddr> = None;
    let mut buf_a = [0u8; 2048];
    let mut buf_b = [0u8; 2048];
    loop {
        tokio::select! {
            _ = stop_rx.changed() => break,
            r = sock_a.recv_from(&mut buf_a) => match r {
                Ok((n, _)) => { if let Some(pb) = peer_b { let _ = sock_b.send_to(&buf_a[..n], pb).await; } }
                Err(_) => break,
            },
            r = sock_b.recv_from(&mut buf_b) => match r {
                Ok((n, from)) => { peer_b.get_or_insert(from); let _ = sock_a.send_to(&buf_b[..n], peer_a).await; }
                Err(_) => break,
            },
        }
    }

    // Caller hung up → BYE the callee leg (best-effort, fire-and-forget).
    let bye = message::request(
        "BYE",
        &callee_target,
        &[
            ("From", from_hdr),
            ("To", callee_to),
            ("Call-ID", leg_call_id),
            ("CSeq", "2 BYE".to_string()),
        ],
        None,
    );
    let _ = sig.send_to(bye.as_bytes(), addr).await;
    tracing::info!(%call_id, "IVR transfer: relay ended, BYE sent to callee");
    true
}

/// Classify an SDP body's media direction for hold detection: `Some(true)` = the offerer put
/// the call on hold (`a=sendonly` / `a=inactive`), `Some(false)` = active/resume
/// (`a=sendrecv` / `a=recvonly`), `None` = no direction attribute at all (a plain retransmit,
/// so the hold state is left unchanged). The check is idempotent — a retransmitted hold or
/// resume re-INVITE re-asserts the same state.
fn hold_direction(sdp: &str) -> Option<bool> {
    if sdp.contains("a=sendonly") || sdp.contains("a=inactive") {
        Some(true)
    } else if sdp.contains("a=sendrecv") || sdp.contains("a=recvonly") {
        Some(false)
    } else {
        None
    }
}

/// Push a message-waiting indication to a phone as an unsolicited SIP `NOTIFY` with an
/// `application/simple-message-summary` body (RFC 3842). `addr`/`request_uri` are the phone's
/// contact; `aor` is its mailbox. Fire-and-forget over a throwaway socket — a phone that does
/// not implement MWI simply ignores it. (A full implementation would honour a prior
/// SUBSCRIBE; the reference sends the summary unsolicited, which common desk phones accept.)
async fn send_mwi_notify(
    addr: SocketAddr,
    request_uri: &str,
    aor: &str,
    media_ip: IpAddr,
    new: u32,
    old: u32,
) {
    let waiting = if new > 0 { "yes" } else { "no" };
    // RFC 3842 §5: `Voice-Message: <new>/<old> (<new-urgent>/<old-urgent>)`.
    let body = format!(
        "Messages-Waiting: {waiting}\r\n\
         Message-Account: {aor}\r\n\
         Voice-Message: {new}/{old} (0/0)\r\n"
    );
    let ua = format!("<sip:commos@{media_ip}>");
    let notify = message::request(
        "NOTIFY",
        request_uri,
        &[
            ("From", ua.clone()),
            ("To", format!("<{aor}>")),
            ("Event", "message-summary".to_string()),
            ("Subscription-State", "active".to_string()),
            ("Contact", ua),
        ],
        Some(("application/simple-message-summary", &body)),
    );
    match UdpSocket::bind("0.0.0.0:0").await {
        Ok(sock) => {
            if let Err(e) = sock.send_to(notify.as_bytes(), addr).await {
                tracing::debug!(error = %e, %addr, "sending MWI NOTIFY failed");
            } else {
                tracing::info!(%addr, mailbox = %aor, new, old, "sent MWI NOTIFY");
            }
        }
        Err(e) => tracing::debug!(error = %e, "could not bind socket for MWI NOTIFY"),
    }
}

/// Send an INVITE on `sock` to `dst` and return its **final** response, retransmitting per RFC
/// 3261 §17.1.1 (a client INVITE transaction over UDP): re-send at T1, 2·T1 … until the first
/// response arrives, then stop retransmitting and wait for the final one (skipping provisional
/// 1xx) up to `overall`. Returns `None` if nothing final arrives in time — so a lost INVITE is
/// retried rather than silently failing the call setup.
async fn send_invite_await_final(
    sock: &UdpSocket,
    invite: &[u8],
    dst: SocketAddr,
    overall: Duration,
) -> Option<SipMessage> {
    if sock.send_to(invite, dst).await.is_err() {
        return None;
    }
    let deadline = tokio::time::sleep(overall);
    tokio::pin!(deadline);
    let mut buf = vec![0u8; MAX_DATAGRAM];
    let mut interval = T1;
    let mut retransmit = true; // stops once any response (even 1xx) is seen
    loop {
        let retx = tokio::time::sleep(interval);
        tokio::select! {
            _ = &mut deadline => return None,
            _ = retx, if retransmit => {
                let _ = sock.send_to(invite, dst).await; // retransmit until first response
                interval = (interval * 2).min(T2);
            }
            r = sock.recv_from(&mut buf) => {
                let Ok((n, _)) = r else { return None };
                let Ok(m) = message::parse(&buf[..n]) else { continue };
                match m.status() {
                    Some(s) if (100..200).contains(&s) => retransmit = false, // provisional: keep waiting
                    Some(_) => return Some(m),                                 // final response
                    None => continue,                                          // stray request
                }
            }
        }
    }
}

/// Like [`send_invite_await_final`], but also watches a `cancel` signal: when it fires (or the
/// sender is dropped) while the callee is still ringing, this sends the pre-built `cancel_req`
/// (a `CANCEL` for the INVITE transaction) to `dst` and returns `None`. This is what stops the
/// losing legs of a simultaneous ring-all fork once another member answers. A cancel that
/// arrives after a 2xx is a no-op here (the 2xx is already returned); the forking caller BYEs
/// that late-answering leg instead.
async fn send_invite_await_final_cancellable(
    sock: &UdpSocket,
    invite: &[u8],
    dst: SocketAddr,
    overall: Duration,
    mut cancel: tokio::sync::watch::Receiver<bool>,
    cancel_req: Vec<u8>,
) -> Option<SipMessage> {
    if sock.send_to(invite, dst).await.is_err() {
        return None;
    }
    let deadline = tokio::time::sleep(overall);
    tokio::pin!(deadline);
    let mut buf = vec![0u8; MAX_DATAGRAM];
    let mut interval = T1;
    let mut retransmit = true;
    loop {
        let retx = tokio::time::sleep(interval);
        tokio::select! {
            _ = &mut deadline => return None,
            _ = retx, if retransmit => {
                let _ = sock.send_to(invite, dst).await;
                interval = (interval * 2).min(T2);
            }
            changed = cancel.changed() => {
                // A change to `true` (or a dropped sender) means a sibling won the race: CANCEL
                // this still-ringing INVITE and give up.
                if changed.is_err() || *cancel.borrow() {
                    let _ = sock.send_to(&cancel_req, dst).await;
                    return None;
                }
            }
            r = sock.recv_from(&mut buf) => {
                let Ok((n, _)) = r else { return None };
                let Ok(m) = message::parse(&buf[..n]) else { continue };
                match m.status() {
                    Some(s) if (100..200).contains(&s) => retransmit = false,
                    Some(_) => return Some(m),
                    None => continue,
                }
            }
        }
    }
}

/// Send a mid-dialog `BYE` toward one leg's endpoint, reconstructed from the dialog identifiers
/// captured when the leg was set up. Binds its own socket so the BYE's `Via` advertises the exact
/// port the 200-to-BYE is awaited on (an unreachable Via just means the transaction is retried,
/// not that teardown fails). Free function so a detached driver (with no `&self`) can BYE the
/// caller it answered — the caller-side mirror of the callee BYE on hangup.
async fn bye_leg(media_ip: IpAddr, leg: &CalleeLeg) {
    let sock = match UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(error = %e, addr = %leg.addr, "could not bind socket for BYE");
            return;
        }
    };
    let sent_by = SocketAddr::new(media_ip, sock.local_addr().map(|a| a.port()).unwrap_or(0));
    let bye = message::request(
        "BYE",
        &leg.request_uri,
        &[
            ("Via", message::via_header(sent_by)),
            ("From", leg.from.clone()),
            ("To", leg.to.clone()),
            ("Call-ID", leg.call_id.clone()),
            ("CSeq", format!("{} BYE", leg.cseq + 1)),
        ],
        None,
    );
    if send_request_reliable_on(&sock, bye.as_bytes(), leg.addr).await {
        tracing::info!(addr = %leg.addr, "leg BYE confirmed");
    } else {
        tracing::debug!(addr = %leg.addr, "leg BYE unconfirmed (no final response)");
    }
}

/// Map a decline final status to the `(status, reason)` CommOS relays back to the caller in the
/// `on_decline = busy` policy.
fn decline_status(code: u16) -> (u16, &'static str) {
    match code {
        486 => (486, "Busy Here"),
        600 => (600, "Busy Everywhere"),
        _ => (603, "Decline"),
    }
}

/// Send a non-INVITE request (a mid-dialog BYE) reliably on a caller-provided socket,
/// retransmitting per RFC 3261 §17.1.2 until a final response arrives or [`NON_INVITE_TIMEOUT`]
/// elapses. Returns whether the peer confirmed — so a lost BYE is retried and the callee leg is
/// actually torn down. The caller binds the socket so it can advertise that socket's address in
/// the request's `Via` (see [`SipServer::send_bye_to_leg`]) and receive the final response on it.
async fn send_request_reliable_on(sock: &UdpSocket, request: &[u8], dst: SocketAddr) -> bool {
    if sock.send_to(request, dst).await.is_err() {
        return false;
    }
    let deadline = tokio::time::sleep(NON_INVITE_TIMEOUT);
    tokio::pin!(deadline);
    let mut buf = vec![0u8; MAX_DATAGRAM];
    let mut interval = T1;
    loop {
        let retx = tokio::time::sleep(interval);
        tokio::select! {
            _ = &mut deadline => return false,
            _ = retx => {
                let _ = sock.send_to(request, dst).await;
                interval = (interval * 2).min(T2);
            }
            r = sock.recv_from(&mut buf) => {
                if let Ok((n, _)) = r {
                    if message::parse(&buf[..n]).ok().and_then(|m| m.status()).is_some_and(|s| s >= 200) {
                        return true; // any final response ends the transaction
                    }
                }
            }
        }
    }
}

/// Resolve a contact URI (`sip:200@192.168.1.5:5060`) to the socket address to send requests
/// to. Parses `host[:port]` (default port 5060), returning a literal IP directly and falling
/// back to async DNS for hostnames. Best-effort: returns `None` if nothing resolves.
pub(crate) async fn resolve_contact_addr(contact_uri: &str) -> Option<SocketAddr> {
    let after_scheme = contact_uri
        .trim()
        .trim_start_matches('<')
        .trim_start_matches("sips:")
        .trim_start_matches("sip:");
    // Drop any userinfo (`user@`) then any URI parameters / headers / closing bracket.
    let host_part = match after_scheme.rsplit_once('@') {
        Some((_, h)) => h,
        None => after_scheme,
    };
    let host_port = host_part
        .split([';', '?', '>'])
        .next()
        .unwrap_or(host_part)
        .trim();
    if host_port.is_empty() {
        return None;
    }

    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => match p.parse::<u16>() {
            Ok(p) => (h, p),
            // A colon that is not a port (e.g. an unbracketed IPv6) → treat whole as host.
            Err(_) => (host_port, 5060),
        },
        None => (host_port, 5060),
    };

    if let Ok(ip) = host.parse::<IpAddr>() {
        return Some(SocketAddr::new(ip, port));
    }
    tokio::net::lookup_host((host, port)).await.ok()?.next()
}

/// Extract the first `sip:`/`sips:`/`tel:` URI from a header value (prefer the
/// angle-bracketed `<...>` form).
fn extract_uri(value: &str) -> Option<String> {
    if let (Some(a), Some(b)) = (value.find('<'), value.find('>')) {
        if a < b {
            return Some(value[a + 1..b].trim().to_string());
        }
    }
    let v = value.trim();
    for scheme in ["sips:", "sip:", "tel:"] {
        if let Some(i) = v.find(scheme) {
            let rest = &v[i..];
            let end = rest
                .find(|c: char| c == ';' || c == '>' || c.is_whitespace())
                .unwrap_or(rest.len());
            return Some(rest[..end].to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_part_extracts_and_rejects_domain_only() {
        assert_eq!(user_part("sip:200@example.com"), Some("200"));
        assert_eq!(user_part("<sip:alice@host:5060>"), Some("alice"));
        assert_eq!(user_part("tel:+15551230000@carrier"), Some("+15551230000"));
        assert_eq!(user_part("sip:example.com"), None);
    }

    #[test]
    fn decline_status_maps_final_codes_to_relayable_status() {
        assert_eq!(decline_status(486), (486, "Busy Here"));
        assert_eq!(decline_status(600), (600, "Busy Everywhere"));
        assert_eq!(decline_status(603), (603, "Decline"));
        // Any other declined code collapses to a generic 603 Decline.
        assert_eq!(decline_status(488), (603, "Decline"));
    }

    #[test]
    fn commos_from_header_carries_optional_display_name() {
        let ip: IpAddr = "10.0.0.5".parse().unwrap();
        // No display name → the bare identity, exactly as before.
        assert_eq!(
            commos_from_header(ip, None, "abc"),
            "<sip:commos@10.0.0.5>;tag=abc"
        );
        // With a display name → a quoted display-name prefix the phone renders as the caller.
        assert_eq!(
            commos_from_header(ip, Some("Front Desk"), "abc"),
            "\"Front Desk\" <sip:commos@10.0.0.5>;tag=abc"
        );
        // An empty display name is treated as absent.
        assert_eq!(
            commos_from_header(ip, Some(""), "abc"),
            "<sip:commos@10.0.0.5>;tag=abc"
        );
    }

    #[test]
    fn caller_from_header_presents_caller_identity_to_the_callee() {
        let ip: IpAddr = "10.0.0.5".parse().unwrap();
        // Number + display name → the callee's phone shows the real caller, not "commos".
        assert_eq!(
            caller_from_header(ip, Some("100"), Some("Alice"), "tg"),
            "\"Alice\" <sip:100@10.0.0.5>;tag=tg"
        );
        // Number only → bare caller URI (still the caller's number, not the service identity).
        assert_eq!(
            caller_from_header(ip, Some("100"), None, "tg"),
            "<sip:100@10.0.0.5>;tag=tg"
        );
        // No usable number → fall back to the service `From` so the header is still well-formed.
        assert_eq!(
            caller_from_header(ip, None, Some("Anon"), "tg"),
            "\"Anon\" <sip:commos@10.0.0.5>;tag=tg"
        );
        assert_eq!(
            caller_from_header(ip, Some(""), None, "tg"),
            "<sip:commos@10.0.0.5>;tag=tg"
        );
    }

    #[test]
    fn header_display_name_extracts_quoted_bare_and_none() {
        assert_eq!(
            header_display_name("\"Alice\" <sip:100@host>;tag=x").as_deref(),
            Some("Alice")
        );
        assert_eq!(
            header_display_name("Bob <sip:101@host>").as_deref(),
            Some("Bob")
        );
        // A bare URI (quoted or angle-only) has no display name.
        assert_eq!(header_display_name("<sip:100@host>;tag=x"), None);
        assert_eq!(header_display_name("sip:100@host"), None);
        // CRLF injection in the display name is neutralised (sanitised).
        assert_eq!(
            header_display_name("\"Eve\r\nX: y\" <sip:1@h>").as_deref(),
            Some("EveX: y")
        );
    }

    #[test]
    fn via_header_is_reachable_and_carries_rport_and_a_magic_cookie_branch() {
        let sent_by: SocketAddr = "10.0.0.5:41000".parse().unwrap();
        let via = message::via_header(sent_by);
        assert!(
            via.starts_with("SIP/2.0/UDP 10.0.0.5:41000;rport;branch=z9hG4bK"),
            "reachable sent-by + rport + magic-cookie branch: {via}"
        );
        // A parsed request carrying this Via routes the response back to 10.0.0.5:41000 — never
        // the unreachable `commos.invalid` placeholder that loses the callee's answer.
        assert!(!via.contains("commos.invalid"));
    }

    #[test]
    fn sip_display_name_sanitizes_and_bounds() {
        // Control characters (incl. CRLF header-injection attempts) are dropped.
        assert_eq!(sip_display_name("Sales\r\nInjected: x"), "SalesInjected: x");
        // Quotes and backslashes are escaped per RFC 3261 quoted-string rules.
        assert_eq!(sip_display_name("A \"B\" \\C"), "A \\\"B\\\" \\\\C");
        // Length is bounded so a huge line can't bloat every INVITE.
        assert!(sip_display_name(&"x".repeat(500)).len() <= 64);
    }

    #[test]
    fn display_line_index_is_stable_per_call_and_in_range() {
        let id = Uuid::now_v7();
        // Deterministic for a given call, and always a valid index.
        assert_eq!(display_line_index(id, 3), display_line_index(id, 3));
        for n in 1..=5 {
            assert!(display_line_index(id, n) < n);
        }
    }

    #[test]
    fn parse_nc_accepts_hex_and_rejects_junk() {
        assert_eq!(parse_nc("00000001"), Some(1));
        assert_eq!(parse_nc("0000000a"), Some(10));
        assert_eq!(parse_nc("ffffffff"), Some(u32::MAX));
        // Malformed / overlong / non-hex → None (treated as "no nc").
        assert_eq!(parse_nc(""), None);
        assert_eq!(parse_nc("zzzz"), None);
        assert_eq!(parse_nc("100000000"), None); // 9 hex digits
    }

    #[test]
    fn dial_target_handles_ivr_destination_forms() {
        // An IVR option value may be a URI, a bare number, or an `ext:` shorthand.
        assert_eq!(dial_target("sip:200@host"), Some("200"));
        assert_eq!(dial_target("<sip:alice@host:5060>"), Some("alice"));
        assert_eq!(dial_target("ext:201"), Some("201"));
        assert_eq!(dial_target("202"), Some("202"));
        // Non-endpoint targets (queues) don't resolve to a plain user-part match.
        assert_eq!(dial_target("queue:sales"), Some("queue:sales"));
        assert_eq!(dial_target(""), None);
    }

    #[test]
    fn find_registered_matches_destination_to_a_live_registration() {
        let regs = RegistrationRegistry::new();
        let tenant = Uuid::now_v7();
        regs.register(
            tenant,
            "sip:200@example.com".to_string(),
            "sip:200@192.168.1.9:5060".to_string(),
            None,
            3600,
        );
        // A bare number, an `ext:` form, and a full URI all resolve to the registration.
        assert!(find_registered(&regs, tenant, "200").is_some());
        assert!(find_registered(&regs, tenant, "ext:200").is_some());
        assert!(find_registered(&regs, tenant, "sip:200@anywhere").is_some());
        // A different number, a queue target, and another tenant do not.
        assert!(find_registered(&regs, tenant, "999").is_none());
        assert!(find_registered(&regs, tenant, "queue:sales").is_none());
        assert!(find_registered(&regs, Uuid::now_v7(), "200").is_none());
    }

    /// Drive [`send_mwi_notify`] at a local UDP "phone" and assert the datagram is a
    /// well-formed SIP `NOTIFY` carrying a correct `message-summary` body (RFC 3842).
    #[tokio::test]
    async fn mwi_notify_is_well_formed_message_summary() {
        let phone = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let phone_addr = phone.local_addr().unwrap();
        let media_ip: IpAddr = "127.0.0.1".parse().unwrap();

        // Two new, one old message for mailbox 200.
        send_mwi_notify(
            phone_addr,
            "sip:200@127.0.0.1",
            "sip:200@host",
            media_ip,
            2,
            1,
        )
        .await;

        let mut buf = vec![0u8; 2048];
        let (n, _from) =
            tokio::time::timeout(std::time::Duration::from_secs(1), phone.recv_from(&mut buf))
                .await
                .expect("MWI NOTIFY not received")
                .expect("recv");

        let msg = message::parse(&buf[..n]).expect("NOTIFY parses");
        assert_eq!(msg.method(), Some("NOTIFY"));
        assert_eq!(msg.header("Event"), Some("message-summary"));
        assert_eq!(msg.header("Subscription-State"), Some("active"));
        assert_eq!(
            msg.header("Content-Type"),
            Some("application/simple-message-summary")
        );
        assert_eq!(msg.header("To"), Some("<sip:200@host>"));

        let body = String::from_utf8_lossy(msg.body());
        assert!(body.contains("Messages-Waiting: yes"), "body: {body}");
        assert!(body.contains("Voice-Message: 2/1 (0/0)"), "body: {body}");
        assert!(
            body.contains("Message-Account: sip:200@host"),
            "body: {body}"
        );
    }

    /// When the mailbox is empty, the summary says "no" waiting messages.
    #[tokio::test]
    async fn mwi_notify_reports_no_waiting_when_empty() {
        let phone = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let phone_addr = phone.local_addr().unwrap();
        let media_ip: IpAddr = "127.0.0.1".parse().unwrap();

        send_mwi_notify(
            phone_addr,
            "sip:200@127.0.0.1",
            "sip:200@host",
            media_ip,
            0,
            0,
        )
        .await;

        let mut buf = vec![0u8; 2048];
        let (n, _from) =
            tokio::time::timeout(std::time::Duration::from_secs(1), phone.recv_from(&mut buf))
                .await
                .expect("MWI NOTIFY not received")
                .expect("recv");
        let msg = message::parse(&buf[..n]).expect("NOTIFY parses");
        let body = String::from_utf8_lossy(msg.body());
        assert!(body.contains("Messages-Waiting: no"), "body: {body}");
        assert!(body.contains("Voice-Message: 0/0 (0/0)"), "body: {body}");
    }

    /// A reliable non-INVITE transaction retransmits a lost request and completes when the peer
    /// finally answers — the mechanism that makes a mid-dialog BYE actually tear the callee down.
    #[tokio::test]
    async fn reliable_request_retransmits_until_final_response() {
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = peer.local_addr().unwrap();
        // Peer drops the first datagram, then answers the retransmit with a 200.
        let peer_task = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let _ = peer.recv_from(&mut buf).await; // first send — "lost"
            let (_, from) = peer.recv_from(&mut buf).await.unwrap(); // retransmit
            peer.send_to(b"SIP/2.0 200 OK\r\nContent-Length: 0\r\n\r\n", from)
                .await
                .unwrap();
        });
        let bye = b"BYE sip:x@127.0.0.1 SIP/2.0\r\nCSeq: 2 BYE\r\nContent-Length: 0\r\n\r\n";
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        assert!(
            send_request_reliable_on(&sock, bye, peer_addr).await,
            "should retransmit the lost BYE and observe the 200"
        );
        peer_task.await.unwrap();
    }

    /// The outbound INVITE transaction retransmits until the callee responds, then returns the
    /// final response (skipping provisional 1xx).
    #[tokio::test]
    async fn invite_retransmits_then_returns_final() {
        let callee = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let callee_addr = callee.local_addr().unwrap();
        let callee_task = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let _ = callee.recv_from(&mut buf).await; // first INVITE — "lost"
            let (_, from) = callee.recv_from(&mut buf).await.unwrap(); // retransmit
                                                                       // Provisional first (stops retransmission), then the final 200.
            callee
                .send_to(b"SIP/2.0 180 Ringing\r\nContent-Length: 0\r\n\r\n", from)
                .await
                .unwrap();
            callee
                .send_to(b"SIP/2.0 200 OK\r\nContent-Length: 0\r\n\r\n", from)
                .await
                .unwrap();
        });
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let invite =
            b"INVITE sip:x@127.0.0.1 SIP/2.0\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
        let resp =
            send_invite_await_final(&sock, invite, callee_addr, Duration::from_secs(3)).await;
        assert_eq!(
            resp.and_then(|m| m.status()),
            Some(200),
            "should return the final 200"
        );
        callee_task.await.unwrap();
    }

    /// A losing leg of a ring-all fork: while the callee is ringing (180), a cancel signal makes
    /// the await send a `CANCEL` for the INVITE transaction and give up (`None`) — this is what
    /// stops the other members' phones once one answers.
    #[tokio::test]
    async fn cancellable_await_sends_cancel_when_signalled() {
        let callee = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let callee_addr = callee.local_addr().unwrap();
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

        // The awaiter runs concurrently; it will INVITE, see the 180, then observe the cancel.
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let invite =
            b"INVITE sip:x@127.0.0.1 SIP/2.0\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
                .to_vec();
        let cancel_req =
            b"CANCEL sip:x@127.0.0.1 SIP/2.0\r\nCSeq: 1 CANCEL\r\nContent-Length: 0\r\n\r\n"
                .to_vec();
        let awaiter = tokio::spawn(async move {
            send_invite_await_final_cancellable(
                &sock,
                &invite,
                callee_addr,
                Duration::from_secs(5),
                cancel_rx,
                cancel_req,
            )
            .await
        });

        let mut buf = [0u8; 2048];
        // Receive the INVITE and answer 180 (so retransmits stop and the leg is "ringing").
        let (n, from) = callee.recv_from(&mut buf).await.unwrap();
        assert!(buf[..n].starts_with(b"INVITE"));
        callee
            .send_to(b"SIP/2.0 180 Ringing\r\nContent-Length: 0\r\n\r\n", from)
            .await
            .unwrap();

        // A sibling won → cancel this leg. The next datagram the callee sees must be a CANCEL.
        cancel_tx.send(true).unwrap();
        let (n2, _) = callee.recv_from(&mut buf).await.unwrap();
        assert!(
            buf[..n2].starts_with(b"CANCEL"),
            "cancelled leg must send a SIP CANCEL"
        );

        // And the await resolves to None (this leg did not win).
        assert!(awaiter.await.unwrap().is_none());
    }

    #[test]
    fn hold_direction_classifies_sdp() {
        // Hold: sendonly / inactive.
        assert_eq!(hold_direction("v=0\r\na=sendonly\r\n"), Some(true));
        assert_eq!(
            hold_direction("m=audio 5004 RTP/AVP 0\r\na=inactive\r\n"),
            Some(true)
        );
        // Resume / active: sendrecv / recvonly.
        assert_eq!(hold_direction("a=sendrecv\r\n"), Some(false));
        assert_eq!(hold_direction("a=recvonly\r\n"), Some(false));
        // No direction attribute → unchanged (plain retransmit).
        assert_eq!(hold_direction("v=0\r\nm=audio 5004 RTP/AVP 0\r\n"), None);
    }

    /// The queue-wait driver latches the caller, plays treatment audio, and — with no member to
    /// answer — overflows and exits cleanly once `max_wait` elapses (never hangs on hold music).
    #[tokio::test]
    async fn queue_wait_driver_overflows_and_exits_with_no_members() {
        use crate::control::registrations::RegistrationRegistry;

        let driver_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let driver_addr = driver_sock.local_addr().unwrap();
        let caller = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // The caller sends one RTP-sized packet so the driver latches its address.
        caller.send_to(&[0u8; 172], driver_addr).await.unwrap();

        let (_stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let moh = Arc::new(crate::sip::moh::MohSource::synth());
        let wait = crate::sip::queuewait::WaitConfig {
            max_wait: Some(Duration::from_millis(300)),
            announce_every: Duration::from_secs(30),
            poll_every: Duration::from_millis(50),
        };
        let beep = g711::beep(20, g711::G711::Ulaw);
        let driver = tokio::spawn(SipServer::queue_wait_driver(
            driver_sock,
            g711::G711::Ulaw,
            dtmf::TELEPHONE_EVENT_PT,
            "127.0.0.1".parse().unwrap(),
            Uuid::now_v7(),
            moh,
            true,
            RegistrationRegistry::new(),
            Vec::new(), // no members → nobody to place the caller with
            Uuid::now_v7(),
            wait,
            None, // no overflow target
            Duration::from_secs(1),
            beep.clone(),
            beep,
            None,
            stop_rx,
        ));

        // The caller receives treatment audio (greeting / hold music).
        let mut buf = [0u8; 2048];
        assert!(
            tokio::time::timeout(Duration::from_secs(2), caller.recv_from(&mut buf))
                .await
                .is_ok(),
            "caller should hear queue treatment audio"
        );
        // The driver overflows shortly after max_wait and finishes (does not hang on MoH).
        tokio::time::timeout(Duration::from_secs(3), driver)
            .await
            .expect("driver should finish after overflow")
            .unwrap();
    }
}
