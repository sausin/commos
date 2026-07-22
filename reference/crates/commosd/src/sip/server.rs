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
    Ivr { task: JoinHandle<()>, stop: tokio::sync::watch::Sender<bool> },
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
    /// whether hold music is enabled. Streamed to a held/waiting caller instead of silence.
    /// Loaded and available; the splice into the live hold bridge is a documented follow-up.
    #[allow(dead_code)]
    moh: Arc<super::moh::MohSource>,
    #[allow(dead_code)]
    music_on_hold: bool,
    /// Per-call rotation counter for hunt-group / round-robin member ordering (the pure ring
    /// planner takes this as its rotation input, spreading load across successive calls).
    ring_rotation: Arc<std::sync::atomic::AtomicUsize>,
}

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
            no_answer_timeout: Duration::from_secs(no_answer_rings.max(1) as u64 * SECONDS_PER_RING),
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

    /// How long a challenge nonce stays valid (seconds).
    const NONCE_TTL: i64 = 300;

    /// Issue a fresh nonce, remember it (with a zero nonce-count baseline for the replay guard),
    /// and return it.
    fn issue_nonce(&self) -> String {
        // CSPRNG-backed via UUIDv7's random component. (Predictable dialog tags — a separate,
        // lower-severity item — are tracked elsewhere; the nonce here is not guessable in a way
        // that matters because it is also validated against this server-side set.)
        let nonce: String = format!("{}{}", Uuid::now_v7(), Uuid::now_v7())
            .chars()
            .filter(|c| c.is_ascii_hexdigit())
            .take(32)
            .collect();
        let exp = now_unix() + Self::NONCE_TTL;
        let mut g = self.nonces.lock().expect("nonces mutex");
        g.retain(|_, s| s.exp > now_unix());
        g.insert(nonce.clone(), NonceState { exp, highest_nc: 0 });
        nonce
    }

    /// Validate that `nonce` is one we issued and unexpired, and that `nc` (if the client sent a
    /// nonce-count) is strictly greater than the highest we have already accepted for it — the
    /// replay guard. Read-only: it does not advance state (that happens in [`nonce_commit`] only
    /// after the digest response itself verifies, so a wrong-password attempt can't burn state).
    ///
    /// [`nonce_commit`]: Self::nonce_commit
    fn nonce_check(&self, nonce: &str, nc: Option<&str>) -> bool {
        let now = now_unix();
        let mut g = self.nonces.lock().expect("nonces mutex");
        g.retain(|_, s| s.exp > now);
        let Some(state) = g.get(nonce) else { return false };
        match nc.and_then(parse_nc) {
            Some(n) => n > state.highest_nc,
            // No nc (client didn't use qop): the nonce is single-use — allowed once while present.
            None => true,
        }
    }

    /// Commit a successful authentication against `nonce`: advance the highest accepted `nc`, or,
    /// when the client sent no `nc`, consume the nonce outright (single-use). Called only after
    /// the digest response has verified.
    fn nonce_commit(&self, nonce: &str, nc: Option<&str>) {
        let mut g = self.nonces.lock().expect("nonces mutex");
        match nc.and_then(parse_nc) {
            Some(n) => {
                if let Some(state) = g.get_mut(nonce) {
                    state.highest_nc = state.highest_nc.max(n);
                }
            }
            None => {
                g.remove(nonce);
            }
        }
    }

    /// Verify the request's `Authorization` digest for `method` against the stored per-device
    /// secret. On success returns the authenticated **username** so the caller can bind it to the
    /// request's claimed identity (REGISTER AoR / INVITE From); `None` on any failure. Enforces
    /// nonce validity and replay protection.
    async fn digest_ok(&self, msg: &SipMessage, method: &str) -> Option<String> {
        let creds = msg
            .header("Authorization")
            .and_then(super::digest::Credentials::parse)?;
        if !self.nonce_check(&creds.nonce, creds.nc.as_deref()) {
            return None;
        }
        let secret = match self
            .store
            .get_sip_credential(self.default_tenant, &creds.username)
            .await
        {
            Ok(Some(secret)) => secret,
            _ => return None,
        };
        if !super::digest::verify(&creds, method, &secret) {
            return None;
        }
        // Authentication succeeded — advance/consume the nonce so this exact request can't be
        // replayed within the nonce's lifetime.
        self.nonce_commit(&creds.nonce, creds.nc.as_deref());
        Some(creds.username)
    }

    /// Whether digest auth must be enforced for a request from `src`. Always enforced when
    /// `require_auth` is configured; additionally enforced (regardless of the flag) for any
    /// **untrusted (public) source address**, so an internet-exposed SIP port never accepts an
    /// unauthenticated REGISTER/INVITE even in a zero-config deployment.
    fn auth_required_from(&self, src: SocketAddr) -> bool {
        self.require_auth || !crate::net::is_trusted_ip(&src.ip())
    }

    /// Send a `401 Unauthorized` with a fresh digest challenge, prompting the phone to
    /// re-send the request with an `Authorization` header.
    async fn send_challenge(&self, resp: &Responder, msg: &SipMessage) -> std::io::Result<()> {
        let challenge = super::digest::Challenge::new(self.realm.clone(), self.issue_nonce());
        let reply = message::response_with(
            msg,
            401,
            "Unauthorized",
            &[("WWW-Authenticate", challenge.header_value())],
        );
        resp.send(reply.as_bytes()).await
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
            let responder = Responder::Udp { socket: socket.clone(), dst: src };
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

    /// INFO: out-of-band DTMF (`application/dtmf-relay` / `application/dtmf`). If the datagram's
    /// dialog is a live IVR session, inject the pressed digit into it; always `200 OK`.
    async fn on_info(&self, resp: &Responder, msg: &SipMessage) -> std::io::Result<()> {
        let body = String::from_utf8_lossy(msg.body());
        if let (Some(call_id), Some(digit)) = (msg.call_id(), dtmf::parse_info_dtmf(&body)) {
            let injected = self
                .dialogs
                .lock()
                .expect("dialogs mutex")
                .get(call_id)
                .and_then(|d| d.info_tx.as_ref().map(|tx| tx.send(digit).is_ok()))
                .unwrap_or(false);
            if injected {
                tracing::info!(%call_id, %digit, "SIP INFO DTMF injected into IVR");
            }
        }
        self.reply(resp, msg, 200, "OK").await
    }

    /// REGISTER: bind the AoR to its contact and confirm with a `200 OK`.
    async fn on_register(&self, resp: &Responder, msg: &SipMessage) -> std::io::Result<()> {
        let src = resp.peer();
        // Digest auth gate (Volume 9): an unauthenticated REGISTER is challenged with 401 +
        // WWW-Authenticate; the phone re-sends with credentials we verify against its stored
        // per-device secret. Auth is enforced when configured, and always for a public source.
        let authed_user = if self.auth_required_from(src) {
            match self.digest_ok(msg, "REGISTER").await {
                Some(user) => Some(user),
                None => {
                    tracing::info!(%src, "SIP REGISTER challenged (digest auth required)");
                    return self.send_challenge(resp, msg).await;
                }
            }
        } else {
            None
        };

        let aor = match msg.register_aor() {
            Some(a) if !a.is_empty() => a,
            _ => {
                tracing::debug!(%src, "REGISTER without a usable To/From AoR");
                return self.reply(resp, msg, 400, "Bad Request").await;
            }
        };

        // Identity binding: a device authenticated as `user` may only register its own AoR. This
        // stops a device that holds one extension's credential from hijacking another extension's
        // registration (and thus its calls/voicemail) by putting a different AoR in To/From.
        if let Some(user) = &authed_user {
            let aor_user = user_part(&aor);
            if aor_user.map(|u| !u.eq_ignore_ascii_case(user)).unwrap_or(true) {
                tracing::warn!(
                    %src, authed = %user, aor = %aor,
                    "SIP REGISTER rejected: authenticated user does not own the registered AoR"
                );
                return self.reply(resp, msg, 403, "Forbidden").await;
            }
        }
        let expires = msg.expires();
        let contact = msg.contact_uri().unwrap_or_else(|| format!("sip:{}", src));
        let user_agent = msg.user_agent().map(str::to_string);

        let reg = self.registrations.register(
            self.default_tenant,
            aor.clone(),
            contact.clone(),
            user_agent.clone(),
            expires,
        );
        if expires == 0 {
            tracing::info!(method = "REGISTER", %aor, %src, "SIP de-register (expires=0)");
        } else {
            tracing::info!(method = "REGISTER", %aor, contact = %contact, expires,
                registration_id = %reg.id, "SIP REGISTER");
        }

        let contact_header = format!("<{contact}>;expires={expires}");
        let extra = [("Contact", contact_header), ("Expires", expires.to_string())];
        let reply = message::response_with(msg, 200, "OK", &extra);
        let sent = resp.send(reply.as_bytes()).await;

        // A returning phone should light its message-waiting lamp: push MWI after the 200 OK
        // if the mailbox has unheard voicemails. Skipped on de-register (expires=0) and when
        // voicemail is off. Fire-and-forget, so the REGISTER response is never delayed.
        if self.voicemail_enabled && expires > 0 {
            self.maybe_notify_mwi(aor, contact);
        }
        sent
    }

    /// INVITE: create an inbound Call, report ring+answer facts, set up RTP echo, answer
    /// `200 OK` with an SDP answer.
    async fn on_invite(&self, resp: &Responder, msg: &SipMessage) -> std::io::Result<()> {
        let src = resp.peer();
        let call_id_hdr = msg.call_id().unwrap_or("").to_string();
        let to_ref = msg
            .request_uri()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "sip:unknown".to_string());
        let from_ref = msg
            .header("From")
            .and_then(extract_uri)
            .unwrap_or_else(|| format!("sip:{}", src));

        tracing::info!(method = "INVITE", from = %from_ref, to = %to_ref, %src, "SIP INVITE");

        // In-dialog INVITE: a retransmission (our 200 OK was lost) or a re-INVITE (media refresh /
        // hold) for a dialog we already answered. Re-answer 200 OK from the stored media — echoing
        // the incoming INVITE's headers/CSeq — without creating a duplicate Call or re-binding
        // media. (Full re-negotiation of a hold's media direction is future B2BUA work.)
        let existing = self
            .dialogs
            .lock()
            .expect("dialogs mutex")
            .get(&call_id_hdr)
            .map(|d| (d.call_id, d.answer_sdp.clone()));
        if let Some((dialog_call_id, answer_sdp)) = existing {
            tracing::info!(%dialog_call_id, %src, "SIP re-INVITE / retransmit → replaying answer");
            let ok = self.build_invite_ok(msg, &answer_sdp, dialog_call_id);
            return resp.send(ok.as_bytes()).await;
        }

        // Digest auth gate: an unauthenticated INVITE is challenged with 401 before any Call is
        // created; the phone re-sends with credentials. (REGISTER auth already limits who is
        // reachable; challenging INVITE too stops direct unauthenticated dialing.) Enforced when
        // configured, and always for a public source address.
        if self.auth_required_from(src) {
            match self.digest_ok(msg, "INVITE").await {
                Some(user) => {
                    // Caller-identity binding: the From user-part must match the authenticated
                    // user, so a device cannot originate calls (CDRs, trunk/PSTN caller-ID) under
                    // a spoofed identity.
                    let from_user = user_part(&from_ref);
                    if from_user.map(|u| !u.eq_ignore_ascii_case(&user)).unwrap_or(true) {
                        tracing::warn!(
                            %src, authed = %user, from = %from_ref,
                            "SIP INVITE rejected: From identity does not match the authenticated user"
                        );
                        return self.reply(resp, msg, 403, "Forbidden").await;
                    }
                }
                None => {
                    tracing::info!(%src, "SIP INVITE challenged (digest auth required)");
                    return self.send_challenge(resp, msg).await;
                }
            }
        }

        // Provisional response.
        let trying = message::response(msg, 100, "Trying");
        resp.send(trying.as_bytes()).await?;

        // Create the inbound Call in the control plane. Clone `from_ref` so the caller's identity
        // remains available below (e.g. to resolve the caller's own mailbox for `*97`).
        let call = match self
            .routing
            .create_inbound_call(self.default_tenant, from_ref.clone(), to_ref.clone())
            .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "INVITE could not create Call");
                return self.reply(resp, msg, 500, "Server Internal Error").await;
            }
        };
        let call_id = call.base.id;

        // When recording is on, all RTP payload for this call accumulates in this shared buffer
        // (caller leg, header-stripped, as-is). It is threaded into whichever media path is set
        // up below and drained into a Recording on hangup.
        let capture: Option<rtp::Capture> = if self.record_calls {
            Some(Arc::new(Mutex::new(Vec::new())))
        } else {
            None
        };

        // SIP is the media plane here: report the ring now. The Call goes to RINGING (a fast BYE
        // while ringing is still a legal hang-up: Ringing → Ended). The ANSWERED fact is applied
        // later, at the moment CommOS actually answers the caller with 200 OK — for a bridged call
        // that is when the callee really picks up, so `answered_at` (and the billed duration) is
        // the true connect time, not the INVITE-receipt time.
        if let Err(e) = self
            .routing
            .apply_fact(MediaFact::Rang { tenant_id: self.default_tenant, call_id })
            .await
        {
            tracing::warn!(error = %e, %call_id, "applying SIP ring fact failed");
        }

        // Route the dialled number through the Extension→Route table (control-plane routing,
        // Volume 3). A route to a SIP endpoint rewrites the effective target so the registered
        // callee is found by the route's user-part, not just a bare request-URI match; a
        // queue/external destination has no registered endpoint and falls through to echo.
        let request_uri = msg.request_uri().unwrap_or("");
        let routed_uri = self.resolve_route(request_uri).await;
        let effective_uri = routed_uri.as_deref().unwrap_or(request_uri);

        // Negotiate codecs from the caller's SDP offer (CMOS-07-SIP-041). `te_pt` is the DTMF
        // payload type; `g711` is the G.711 variant CommOS synthesises where it is itself the
        // endpoint (IVR/voicemail); `reflect` is the caller's preferred codec, answered verbatim
        // on the echo path (byte-transparent). Bridge/trunk pass the whole offer through.
        let body = String::from_utf8_lossy(msg.body()).into_owned();
        let offer = codec::AudioMedia::parse(&body);
        let te_pt = offer.telephone_event_pt().unwrap_or(dtmf::TELEPHONE_EVENT_PT);
        let g711 = offer
            .select_g711()
            .and_then(|c| g711::G711::from_name(&c.name))
            .unwrap_or(g711::G711::Ulaw);
        let g711_codec = codec::Codec { pt: g711.payload_type(), name: g711.sdp_name().to_string(), clock: 8000 };
        let reflect = offer.preferred_audio().unwrap_or_else(default_codec);
        tracing::info!(%call_id, endpoint_codec = %g711.sdp_name(), reflect_codec = %reflect.name, te_pt, "codecs negotiated");

        // Whether the INVITE arrived over a confidential (TLS) transport. SDES SRTP keying is
        // only honoured on a secure transport — see `caller_crypto` — so the media key is never
        // exposed in cleartext SDP over plain UDP.
        let secure = resp.is_secure();
        if !secure && self.srtp_enabled && sdes::offers_savp(&body) {
            tracing::warn!(%call_id, %src,
                "caller offered SRTP/SAVP over a non-TLS transport; answering plain RTP (SDES key \
                 would otherwise be exposed in cleartext SDP). Use SIPS to enable SRTP.");
        }

        // The caller's SDES key, if it offered SRTP over a secure transport — used to key the
        // caller (leg A) side of a bridge/trunk and to answer the caller over RTP/SAVP.
        let caller_crypto = self.caller_crypto(&body, secure);

        // Inbound DID: an INVITE from a carrier to a provisioned external number is routed to its
        // `destination_ref`. The effective target is that DID destination if matched, else the
        // extension route, else the raw request-URI.
        let did_dest = self.resolve_did(request_uri).await;
        if did_dest.is_some() {
            tracing::info!(%call_id, number = %request_uri, dest = ?did_dest, "inbound DID routed");
        }
        let target: String = did_dest.clone().unwrap_or_else(|| effective_uri.to_string());

        // Feature codes for voicemail retrieval (the "voicemail button" / dial-in). Handled here
        // in the SIP layer like `ivr:`/`voicemail` targets — no dialplan entry needed:
        //   *97 — listen to your OWN mailbox (the caller's extension),
        //   *98 — listen to ANOTHER mailbox (the extension is entered via DTMF).
        let dialed = user_part(request_uri).unwrap_or("");
        if dialed == "*97" || dialed == "*98" {
            let own_mailbox = if dialed == "*97" { user_part(&from_ref).map(str::to_string) } else { None };
            return self
                .answer_with_voicemail_retrieval(resp, msg, call_id, &call_id_hdr, g711, te_pt, own_mailbox)
                .await;
        }

        // The target (from a DID or an extension route) may name an `ivr:<id>` menu: run the IVR
        // runtime (answer with SDP, play the prompt, collect DTMF).
        if let Some(ivr_id) = match ivr_id_of(&target) {
            Some(id) => Some(id),
            None => self.resolve_ivr_target(request_uri).await,
        } {
            return self.answer_with_ivr(resp, msg, call_id, &call_id_hdr, ivr_id, g711, te_pt).await;
        }

        let mut voicemail_target: Option<VoicemailBox> = None;
        // A direct voicemail target (e.g. a DID → "voicemail").
        if target == "voicemail" || target.starts_with("voicemail:") {
            voicemail_target = Some(VoicemailBox { aor: target.clone(), notify: None });
        }

        // Multi-destination routing: a ring group (`ringgroup:<uuid>`) or an active forwarding
        // rule for the dialled number is executed as a resolved DialPlan (fan-out / follow-me),
        // reusing the single-leg bridge/trunk primitives. This branch engages ONLY when there is
        // genuinely a group or a forwarding rule in play; the plain single-extension bridge path
        // below is left completely untouched otherwise.
        if voicemail_target.is_none() {
            let dialled = user_part(&to_ref).map(|s| s.to_string()).unwrap_or_default();
            let is_group = target.starts_with(crate::control::ringresolve::RING_GROUP_SCHEME);
            let has_forwarding = !dialled.is_empty()
                && crate::control::ringresolve::active_forwarding(
                    &self.store, self.default_tenant, &dialled).await.is_some();
            if is_group || has_forwarding {
                // Tell the caller's phone we're ringing while we walk the plan.
                let _ = resp.send(self.build_ringing(msg, call_id).as_bytes()).await;
                let caller_display = msg.header("From").and_then(header_display_name);
                let caller_id = CallerId { number: user_part(&from_ref), display: caller_display.as_deref() };
                let opts = crate::control::ringplan::PlanOpts {
                    default_ring_seconds: self.no_answer_timeout.as_secs().max(1) as u32,
                    voicemail_enabled: self.voicemail_enabled,
                };
                let rotation = self.ring_rotation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let regs = &self.registrations;
                let tenant = self.default_tenant;
                let plan = crate::control::ringresolve::resolve_plan(
                    &self.store, tenant, &dialled, &target, opts, rotation,
                    |r: &str| find_registered(regs, tenant, r).is_some(),
                ).await;

                match self
                    .execute_ring_plan(&plan, call_id, capture.clone(), &offer, caller_crypto.as_ref(), caller_id)
                    .await
                {
                    Some((bridge, callee_leg, sel_codec, sel_te, leg_a_crypto)) => {
                        let leg_a_port = bridge.leg_a_port;
                        let sdp = self.build_sdp(leg_a_port, &sel_codec, sel_te, leg_a_crypto.as_ref());
                        if !call_id_hdr.is_empty() {
                            let callee_call_id = callee_leg.call_id.clone();
                            self.dialogs.lock().expect("dialogs mutex").insert(
                                call_id_hdr.clone(),
                                Dialog {
                                    call_id,
                                    media: Media::Bridge(bridge),
                                    callee: Some(callee_leg),
                                    caller: Some(self.caller_leg(msg, call_id, src)),
                                    capture: capture.clone(),
                                    voicemail: None,
                                    info_tx: None,
                                    answer_sdp: sdp.clone(),
                                },
                            );
                            self.bye_aliases.lock().expect("aliases mutex").insert(callee_call_id, call_id_hdr.clone());
                        } else {
                            bridge.abort();
                        }
                        self.mark_answered(call_id).await;
                        let ok = self.build_invite_ok(msg, &sdp, call_id);
                        tracing::info!(%call_id, leg_a_port, dialled = %dialled, group = is_group,
                            "SIP INVITE bridged via ring plan");
                        return resp.send(ok.as_bytes()).await;
                    }
                    None => {
                        // Nobody answered → apply the plan's final action.
                        match &plan.final_action {
                            crate::control::ringplan::FinalAction::Voicemail(num) if self.voicemail_enabled => {
                                voicemail_target = Some(VoicemailBox { aor: format!("sip:{num}"), notify: None });
                            }
                            crate::control::ringplan::FinalAction::Redirect(_) if self.voicemail_enabled => {
                                // A redirect (queue / another group) is not yet re-resolved here;
                                // divert the dialled number to voicemail as the safe terminus.
                                voicemail_target = Some(VoicemailBox { aor: format!("sip:{dialled}"), notify: None });
                            }
                            _ => {}
                        }
                        // Fall through: a set voicemail_target is picked up by the deposit path;
                        // otherwise the echo fallthrough applies (voicemail disabled).
                    }
                }
            }
        }

        // If the target names a REGISTERED endpoint, bridge the two legs: bind a two-leg RTP
        // relay, INVITE the callee (offering leg B), and on its 200 OK answer the caller
        // (offering leg A). A callee that never answers (or an internal extension that is
        // offline) diverts to voicemail.
        if voicemail_target.is_none() {
            if let Some(callee_reg) = find_registered(&self.registrations, self.default_tenant, &target) {
                // Tell the caller's phone the callee is ringing (early dialog) so it shows a
                // "ringing" state instead of dead air while we ring the callee for real.
                let _ = resp.send(self.build_ringing(msg, call_id).as_bytes()).await;
                // Present the caller's own identity to the callee (number + display name), so the
                // callee's phone shows who is calling instead of the bare service identity.
                let caller_display = msg.header("From").and_then(header_display_name);
                let caller_id = CallerId {
                    number: user_part(&from_ref),
                    display: caller_display.as_deref(),
                };
                match self.try_bridge(&callee_reg, call_id, capture.clone(), &offer,
                    caller_crypto.as_ref(), caller_id).await {
                    Some((bridge, callee_leg, sel_codec, sel_te, leg_a_crypto)) => {
                        let leg_a_port = bridge.leg_a_port;
                        // Answer the caller with the codec the callee selected (transparent relay),
                        // plus our SRTP key when the caller leg is encrypted.
                        let sdp = self.build_sdp(leg_a_port, &sel_codec, sel_te, leg_a_crypto.as_ref());
                        if !call_id_hdr.is_empty() {
                            let callee_call_id = callee_leg.call_id.clone();
                            self.dialogs.lock().expect("dialogs mutex").insert(
                                call_id_hdr.clone(),
                                Dialog {
                                    call_id,
                                    media: Media::Bridge(bridge),
                                    callee: Some(callee_leg),
                                    caller: Some(self.caller_leg(msg, call_id, src)),
                                    capture: capture.clone(),
                                    voicemail: None,
                                    info_tx: None,
                                    answer_sdp: sdp.clone(),
                                },
                            );
                            // Index the callee-leg Call-ID so a callee-side BYE finds this dialog.
                            self.bye_aliases
                                .lock()
                                .expect("aliases mutex")
                                .insert(callee_call_id, call_id_hdr.clone());
                        } else {
                            bridge.abort();
                        }
                        // The callee actually picked up: this is the true connect time.
                        self.mark_answered(call_id).await;
                        let ok = self.build_invite_ok(msg, &sdp, call_id);
                        tracing::info!(%call_id, leg_a_port, callee = %callee_reg.contact,
                            codec = %sel_codec.name, srtp = leg_a_crypto.is_some(), "SIP INVITE bridged to registered callee");
                        return resp.send(ok.as_bytes()).await;
                    }
                    None if self.voicemail_enabled => {
                        // Rang but never answered → take a voicemail. MWI is pushed to the
                        // callee's registered contact on hangup.
                        let notify = resolve_contact_addr(&callee_reg.contact)
                            .await
                            .map(|addr| (addr, callee_reg.contact.clone()));
                        tracing::info!(%call_id, mailbox = %callee_reg.aor,
                            "registered callee did not answer; diverting to voicemail");
                        voicemail_target = Some(VoicemailBox { aor: callee_reg.aor.clone(), notify });
                    }
                    None => {
                        tracing::warn!(%call_id, callee = %callee_reg.contact,
                            "registered callee did not answer within timeout; falling back to echo");
                    }
                }
            } else if self.voicemail_enabled && (routed_uri.is_some() || did_dest.is_some()) {
                // The target is an internal endpoint (an extension route or a DID destination) but
                // nobody is registered for it — the mailbox owner is offline. Take a voicemail; its
                // MWI is delivered on the phone's next REGISTER.
                tracing::info!(%call_id, mailbox = %target, "internal endpoint is offline; diverting to voicemail");
                voicemail_target = Some(VoicemailBox { aor: target.clone(), notify: None });
            }
        }

        // Outbound PSTN / SIP trunk: an external E.164 destination with a configured ONLINE SIP
        // gateway is placed to the carrier and relayed (reuses the two-leg bridge). A caller
        // dialling a real phone number reaches it. Falls through to echo if the trunk fails.
        if voicemail_target.is_none() {
            if let Some((gateway, e164)) = self.select_outbound_gateway(&target).await {
                // Signal ringing to the caller while we place the outbound leg to the carrier.
                let _ = resp.send(self.build_ringing(msg, call_id).as_bytes()).await;
                if let Some((bridge, leg, sel_codec, sel_te, leg_a_crypto)) = self.try_trunk(&gateway, &e164, call_id, capture.clone(), &offer, caller_crypto.as_ref()).await {
                    let leg_a_port = bridge.leg_a_port;
                    // Answer the caller with the carrier's selected codec (transparent relay),
                    // plus our SRTP key when the caller leg is encrypted.
                    let sdp = self.build_sdp(leg_a_port, &sel_codec, sel_te, leg_a_crypto.as_ref());
                    if !call_id_hdr.is_empty() {
                        let callee_call_id = leg.call_id.clone();
                        self.dialogs.lock().expect("dialogs mutex").insert(
                            call_id_hdr.clone(),
                            Dialog {
                                call_id,
                                media: Media::Bridge(bridge),
                                callee: Some(leg),
                                caller: Some(self.caller_leg(msg, call_id, src)),
                                capture: capture.clone(),
                                voicemail: None,
                                info_tx: None,
                                answer_sdp: sdp.clone(),
                            },
                        );
                        self.bye_aliases.lock().expect("aliases mutex").insert(callee_call_id, call_id_hdr.clone());
                    } else {
                        bridge.abort();
                    }
                    // The carrier answered: true connect time.
                    self.mark_answered(call_id).await;
                    let ok = self.build_invite_ok(msg, &sdp, call_id);
                    tracing::info!(%call_id, %e164, gateway = ?gateway.address, codec = %sel_codec.name, srtp = leg_a_crypto.is_some(), "SIP INVITE routed outbound via trunk");
                    return resp.send(ok.as_bytes()).await;
                }
                tracing::warn!(%call_id, %e164, "outbound trunk failed; falling back to echo");
            }
        }

        // Voicemail deposit path: answer the caller, play a greeting ("please leave your message
        // after the tone") and a beep, then capture ONLY the audio after the beep — stored as a
        // Voicemail on hangup, with an MWI pushed to the mailbox. This reuses the IVR prompt
        // runtime, so — like the IVR menu path — the media is plaintext G.711 (SRTP for
        // prompt-bearing media is future work); a phone that offered SRTP is answered plain RTP.
        if let Some(vmbox) = voicemail_target {
            let sock = match UdpSocket::bind("0.0.0.0:0").await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "could not bind RTP for voicemail; answering without media");
                    // Answer anyway so the caller isn't left hanging; no capture is possible.
                    self.mark_answered(call_id).await;
                    let sdp = self.build_sdp(0, &g711_codec, te_pt, None);
                    let ok = self.build_invite_ok(msg, &sdp, call_id);
                    return resp.send(ok.as_bytes()).await;
                }
            };
            let rtp_port = sock.local_addr().map(|a| a.port()).unwrap_or(0);
            // Greeting = the recorded "leave a message after the tone" prompt (when the sound pack
            // is installed) followed by a 250 ms beep. With no prompt installed it is just the beep.
            let greeting = self.voicemail_greeting(g711).await;
            let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
            let task = tokio::spawn(Self::voicemail_deposit_driver(
                sock,
                g711,
                te_pt,
                greeting,
                self.default_tenant,
                call_id,
                vmbox,
                self.voicemails.clone(),
                self.media_ip,
                stop_rx,
            ));
            // Answer plaintext G.711 (prompt-bearing media path, like the IVR menu).
            let sdp = self.build_sdp(rtp_port, &g711_codec, te_pt, None);
            if !call_id_hdr.is_empty() {
                self.dialogs.lock().expect("dialogs mutex").insert(
                    call_id_hdr,
                    Dialog {
                        call_id,
                        // Media::Ivr tears down gracefully on BYE (signals `stop` so the deposit
                        // driver saves whatever was recorded before exiting).
                        media: Media::Ivr { task, stop: stop_tx },
                        callee: None,
                        caller: None,
                        capture: None,
                        voicemail: None,
                        info_tx: None,
                        answer_sdp: sdp.clone(),
                    },
                );
            } else {
                let _ = stop_tx.send(true);
            }
            self.mark_answered(call_id).await;
            let ok = self.build_invite_ok(msg, &sdp, call_id);
            tracing::info!(%call_id, rtp_port, codec = %g711_codec.name, "SIP INVITE answered (voicemail deposit)");
            return resp.send(ok.as_bytes()).await;
        }

        // Echo path: non-mailbox destination (PSTN-style / +E.164), or a no-answer with
        // voicemail disabled. One UDP socket reflecting RTP back to the caller — decrypting then
        // re-encrypting each packet when the caller negotiated SRTP.
        let (echo_crypto, echo_srtp) = match self.negotiate_srtp(&body, secure) {
            Some((c, s)) => (Some(c), Some(s)),
            None => (None, None),
        };
        let (rtp_port, task) = match rtp::bind_echo(capture.clone(), echo_srtp).await {
            Ok((port, task)) => (port, Some(task)),
            Err(e) => {
                tracing::warn!(error = %e, "could not bind RTP; answering without media");
                (0, None)
            }
        };

        // Answer with the caller's preferred codec — the echo path reflects it byte-for-byte.
        let sdp = self.build_sdp(rtp_port, &reflect, te_pt, echo_crypto.as_ref());
        if let Some(task) = task {
            if !call_id_hdr.is_empty() {
                self.dialogs.lock().expect("dialogs mutex").insert(
                    call_id_hdr,
                    Dialog {
                        call_id,
                        media: Media::Echo(task),
                        callee: None,
                        caller: None,
                        capture: capture.clone(),
                        voicemail: None,
                        info_tx: None,
                        answer_sdp: sdp.clone(),
                    },
                );
            } else {
                task.abort();
            }
        }
        // CommOS answers the caller directly (echo/PSTN-style): connect time is now.
        self.mark_answered(call_id).await;
        let ok = self.build_invite_ok(msg, &sdp, call_id);
        tracing::info!(%call_id, rtp_port, codec = %reflect.name, srtp = echo_crypto.is_some(), "SIP INVITE answered (RTP echo)");
        resp.send(ok.as_bytes()).await
    }

    /// Resolve the request-URI through the Extension→Route table. When the dialled number
    /// routes to a SIP endpoint (`sip:<user>@<host>`), returns that destination so the callee
    /// is found by the *route's* target; a queue/external (or unrouted) destination returns
    /// `None`, leaving the raw request-URI to be matched (or the echo path to take over).
    async fn resolve_route(&self, request_uri: &str) -> Option<String> {
        let number = user_part(request_uri)?;
        let dest = self
            .routing
            .resolve_extension(self.default_tenant, number)
            .await?;
        if dest.starts_with("sip:") || dest.starts_with("sips:") {
            Some(dest)
        } else {
            tracing::info!(%dest, number, "extension routes to a non-SIP destination; using echo path");
            None
        }
    }

    /// Resolve an `ivr:<uuid>` routing target for the dialled number, if the extension routes
    /// to an IVR menu. Returns the IVR id, else `None` (a non-IVR destination).
    async fn resolve_ivr_target(&self, request_uri: &str) -> Option<Uuid> {
        let number = user_part(request_uri)?;
        let dest = self.routing.resolve_extension(self.default_tenant, number).await?;
        ivr_id_of(&dest)
    }

    /// Resolve an inbound **DID**: if the dialled number (in E.164) is a provisioned DID, return
    /// its `destination_ref` — where an inbound carrier call to this number is routed. `None`
    /// when the request-URI is not a known external number for this tenant.
    async fn resolve_did(&self, request_uri: &str) -> Option<String> {
        let e164 = dialplan::normalize_e164(request_uri, &self.default_cc)?;
        let mut cursor = None;
        loop {
            let page = self.store.list_dids(self.default_tenant, 200, cursor).await.ok()?;
            if let Some(did) = page.items.iter().find(|d| d.e164 == e164) {
                return Some(did.destination_ref.clone());
            }
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => return None,
            }
        }
    }

    /// Select an outbound gateway for a **dialled target**: if it is an external E.164 number and
    /// an `ONLINE` `SIP` gateway with an address exists, return `(gateway, e164)` to place the
    /// call to the carrier. `None` for an internal target or when no usable gateway is configured.
    async fn select_outbound_gateway(&self, target: &str) -> Option<(Gateway, String)> {
        let e164 = dialplan::normalize_e164(target, &self.default_cc)?;
        let mut cursor = None;
        loop {
            let page = self.store.list_gateways(self.default_tenant, 200, cursor).await.ok()?;
            if let Some(gw) = page.items.iter().find(|g| {
                g.kind == GatewayKind::Sip
                    && g.health == GatewayHealth::Online
                    && g.address.as_deref().is_some_and(|a| !a.is_empty())
            }) {
                return Some((gw.clone(), e164));
            }
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => return None,
            }
        }
    }

    /// The digest credentials to authenticate to `carrier_id`'s carrier, from its Trunk's `auth`.
    async fn trunk_credentials(&self, carrier_id: Uuid) -> Option<(String, String)> {
        let mut cursor = None;
        loop {
            let page = self.store.list_trunks(self.default_tenant, 200, cursor).await.ok()?;
            if let Some(t) = page.items.iter().find(|t| t.carrier_id == carrier_id) {
                return t.credentials();
            }
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => return None,
            }
        }
    }

    /// Load an IVR's prompt audio: the recorded prompt Object when set, else a short tone
    /// synthesised in the negotiated `codec` so the caller hears that the menu is live. (A
    /// recorded prompt Object is assumed to already be in the negotiated codec.)
    async fn load_ivr_prompt(&self, tenant: Uuid, ivr: &Ivr, codec: g711::G711) -> Vec<u8> {
        if let Some(obj_id) = ivr.prompt_object_id {
            match self.objects.get_bytes(tenant, obj_id).await {
                Ok((_obj, bytes)) => return bytes,
                Err(e) => tracing::warn!(error = %e, %obj_id, "IVR prompt object missing; using a tone"),
            }
        }
        g711::beep(400, codec)
    }

    /// Answer an INVITE that routes to an IVR: bind an RTP socket, answer `200 OK` with SDP, and
    /// spawn the menu session (play prompt + collect DTMF → resolve destination). Falls back to
    /// the echo path if the IVR is missing or its media socket can't bind.
    #[allow(clippy::too_many_arguments)]
    async fn answer_with_ivr(
        &self,
        resp: &Responder,
        msg: &SipMessage,
        call_id: Uuid,
        call_id_hdr: &str,
        ivr_id: Uuid,
        g711: g711::G711,
        te_pt: u8,
    ) -> std::io::Result<()> {
        let tenant = self.default_tenant;
        let ivr = match self.ivrs.get(tenant, ivr_id).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, %ivr_id, "IVR not found; falling back to echo");
                return self.answer_with_echo(resp, msg, call_id, call_id_hdr).await;
            }
        };
        // The prompt is synthesised/served in the negotiated G.711 codec.
        let prompt = self.load_ivr_prompt(tenant, &ivr, g711).await;
        let cfg = ivr::IvrConfig::from_ivr(
            prompt,
            g711,
            te_pt,
            &ivr.options,
            ivr.timeout_ms,
            ivr.invalid_action.as_deref(),
        );

        let sock = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "could not bind IVR RTP socket; falling back to echo");
                return self.answer_with_echo(resp, msg, call_id, call_id_hdr).await;
            }
        };
        let rtp_port = sock.local_addr().map(|a| a.port()).unwrap_or(0);

        let (info_tx, info_rx) = tokio::sync::mpsc::unbounded_channel::<char>();
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        // The display name to present if this IVR session transfers the caller to an extension.
        let display = self.call_display_name(call_id).await;
        let task = tokio::spawn(Self::ivr_driver(
            sock,
            cfg,
            tenant,
            call_id,
            self.voicemails.clone(),
            self.registrations.clone(),
            self.media_ip,
            self.no_answer_timeout,
            display,
            info_rx,
            stop_rx,
        ));

        let audio = codec::Codec { pt: g711.payload_type(), name: g711.sdp_name().to_string(), clock: 8000 };
        let sdp = self.build_sdp(rtp_port, &audio, te_pt, None);
        if !call_id_hdr.is_empty() {
            self.dialogs.lock().expect("dialogs mutex").insert(
                call_id_hdr.to_string(),
                Dialog {
                    call_id,
                    media: Media::Ivr { task, stop: stop_tx },
                    callee: None,
                    caller: None,
                    capture: None,
                    voicemail: None,
                    info_tx: Some(info_tx),
                    answer_sdp: sdp.clone(),
                },
            );
        } else {
            let _ = stop_tx.send(true); // no dialog key to track it → stop the session
        }

        // The caller is connected to the IVR menu: answered.
        self.mark_answered(call_id).await;
        let ok = self.build_invite_ok(msg, &sdp, call_id);
        tracing::info!(%call_id, %ivr_id, rtp_port, codec = %g711.sdp_name(), "SIP INVITE answered (IVR menu)");
        resp.send(ok.as_bytes()).await
    }

    /// Drive an IVR session to completion, then enact its outcome. A `voicemail*` selection
    /// records the caller (after a beep) until they hang up (graceful `stop`) or the cap, and
    /// stores a [`Voicemail`]. Other selections/timeouts hold the line until hangup — full
    /// mid-call transfer to the chosen destination is future work (tied to B2BUA transfer).
    #[allow(clippy::too_many_arguments)]
    async fn ivr_driver(
        sock: UdpSocket,
        cfg: ivr::IvrConfig,
        tenant: Uuid,
        call_id: Uuid,
        voicemails: VoicemailService,
        registrations: RegistrationRegistry,
        media_ip: IpAddr,
        no_answer_timeout: Duration,
        display: Option<String>,
        mut info_rx: tokio::sync::mpsc::UnboundedReceiver<char>,
        mut stop_rx: tokio::sync::watch::Receiver<bool>,
    ) {
        let result = tokio::select! {
            r = ivr::run_ivr(&sock, &cfg, &mut info_rx) => r,
            _ = stop_rx.changed() => {
                tracing::info!(%call_id, "caller hung up during IVR menu");
                return;
            }
        };
        tracing::info!(%call_id, outcome = ?result.outcome, "IVR menu resolved");

        match result.outcome {
            ivr::IvrOutcome::Selected { destination, .. } if destination.starts_with("voicemail") => {
                if let Some(peer) = result.peer {
                    ivr::play(&sock, peer, cfg.codec.payload_type(), &g711::beep(250, cfg.codec)).await;
                }
                let capture: rtp::Capture = Arc::new(Mutex::new(Vec::new()));
                ivr::record_until_stop(&sock, cfg.te_pt, &capture, &mut stop_rx, IVR_VOICEMAIL_MAX).await;
                let audio = std::mem::take(&mut *capture.lock().expect("capture mutex"));
                if audio.is_empty() {
                    tracing::info!(%call_id, "IVR voicemail: nothing recorded");
                    return;
                }
                match voicemails.save(tenant, call_id, None, &audio).await {
                    Ok(vm) => tracing::info!(%call_id, voicemail_id = %vm.base.id, bytes = audio.len(),
                        "IVR voicemail saved"),
                    Err(e) => tracing::warn!(error = %e, %call_id, "saving IVR voicemail failed"),
                }
            }
            ivr::IvrOutcome::Selected { destination, .. } => {
                // A dial target: bridge the caller to a live registered extension, mid-call.
                if let (Some(peer), Some(callee)) =
                    (result.peer, find_registered(&registrations, tenant, &destination))
                {
                    if ivr_transfer(&sock, peer, &callee, media_ip, call_id, cfg.codec, cfg.te_pt, no_answer_timeout, display.clone(), &mut stop_rx).await {
                        return; // relayed until the caller hung up
                    }
                } else {
                    tracing::info!(%call_id, %destination, "IVR: no registered endpoint for selection");
                }
                // Unreachable/unregistered/queue target → hold the line until hangup.
                let _ = stop_rx.changed().await;
            }
            ivr::IvrOutcome::Timeout | ivr::IvrOutcome::Invalid => {
                // No usable selection → hold the line until the caller hangs up.
                let _ = stop_rx.changed().await;
            }
        }
    }

    /// Load an audio prompt (`<sounds_dir>/en/<name>.ulaw`, raw G.711 μ-law) and transcode it to
    /// `codec`. Returns `None` when the file is missing/empty — the caller falls back to a
    /// synthesized tone, so the system works with no sound pack installed. `name` may include a
    /// subdirectory (e.g. `digits/5`).
    async fn load_prompt(&self, name: &str, codec: g711::G711) -> Option<Vec<u8>> {
        let path = format!("{}/en/{}.ulaw", self.sounds_dir, name);
        match tokio::fs::read(&path).await {
            Ok(bytes) if !bytes.is_empty() => Some(g711::transcode_ulaw(&bytes, codec)),
            _ => None,
        }
    }

    /// The voicemail deposit greeting: the recorded "please leave your message after the tone"
    /// prompt (`vm-intro`) when the sound pack is installed, followed by a 250 ms beep. Only audio
    /// after the beep is captured. With no sound pack it is the beep alone.
    async fn voicemail_greeting(&self, codec: g711::G711) -> Vec<u8> {
        let mut greeting = self.load_prompt("vm-intro", codec).await.unwrap_or_default();
        greeting.extend_from_slice(&g711::beep(250, codec));
        greeting
    }

    /// Preload the retrieval prompt buffers (transcoded to `codec`) so the spawned driver — which
    /// has no `&self` handle — can voice the count/feedback. Missing files become empty buffers.
    async fn load_retrieval_prompts(&self, codec: g711::G711) -> RetrievalPrompts {
        let mut digits: Vec<Vec<u8>> = Vec::with_capacity(10);
        for d in 0..10 {
            digits.push(self.load_prompt(&format!("digits/{d}"), codec).await.unwrap_or_default());
        }
        RetrievalPrompts {
            youhave: self.load_prompt("vm-youhave", codec).await.unwrap_or_default(),
            messages: self.load_prompt("vm-messages", codec).await.unwrap_or_default(),
            message: self.load_prompt("vm-message", codec).await.unwrap_or_default(),
            no_more: self.load_prompt("vm-nomore", codec).await.unwrap_or_default(),
            deleted: self.load_prompt("vm-deleted", codec).await.unwrap_or_default(),
            enter_mailbox: self.load_prompt("vm-extension", codec).await.unwrap_or_default(),
            digits,
        }
    }

    /// Answer a `*97`/`*98` retrieval call: bind RTP, answer `200 OK` (plaintext G.711, like the
    /// IVR menu path), and spawn the retrieval session (announce the count, play each message,
    /// handle the DTMF menu). `own_mailbox` is the caller's own extension for `*97`; for `*98` it
    /// is `None` and the driver prompts for the mailbox number over DTMF. Falls back to the echo
    /// path if the media socket cannot bind.
    #[allow(clippy::too_many_arguments)]
    async fn answer_with_voicemail_retrieval(
        &self,
        resp: &Responder,
        msg: &SipMessage,
        call_id: Uuid,
        call_id_hdr: &str,
        g711: g711::G711,
        te_pt: u8,
        own_mailbox: Option<String>,
    ) -> std::io::Result<()> {
        let sock = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "could not bind RTP for voicemail retrieval; echo fallback");
                return self.answer_with_echo(resp, msg, call_id, call_id_hdr).await;
            }
        };
        let rtp_port = sock.local_addr().map(|a| a.port()).unwrap_or(0);
        let prompts = self.load_retrieval_prompts(g711).await;
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(Self::voicemail_retrieval_driver(
            sock,
            g711,
            te_pt,
            own_mailbox,
            prompts,
            self.voicemails.clone(),
            self.registrations.clone(),
            self.default_tenant,
            self.media_ip,
            stop_rx,
        ));
        let audio = codec::Codec { pt: g711.payload_type(), name: g711.sdp_name().to_string(), clock: 8000 };
        let sdp = self.build_sdp(rtp_port, &audio, te_pt, None);
        if !call_id_hdr.is_empty() {
            self.dialogs.lock().expect("dialogs mutex").insert(
                call_id_hdr.to_string(),
                Dialog {
                    call_id,
                    media: Media::Ivr { task, stop: stop_tx },
                    callee: None,
                    caller: None,
                    capture: None,
                    voicemail: None,
                    info_tx: None,
                    answer_sdp: sdp.clone(),
                },
            );
        } else {
            let _ = stop_tx.send(true);
        }
        self.mark_answered(call_id).await;
        let ok = self.build_invite_ok(msg, &sdp, call_id);
        tracing::info!(%call_id, rtp_port, "SIP INVITE answered (voicemail retrieval *97/*98)");
        resp.send(ok.as_bytes()).await
    }

    /// Play the greeting/beep, then record the caller until they hang up (graceful `stop`) or the
    /// recording cap, and store the captured audio as a Voicemail with an MWI push. Reuses the IVR
    /// media primitives so only post-tone audio is captured.
    #[allow(clippy::too_many_arguments)]
    async fn voicemail_deposit_driver(
        sock: UdpSocket,
        codec: g711::G711,
        te_pt: u8,
        greeting: Vec<u8>,
        tenant: Uuid,
        call_id: Uuid,
        vmbox: VoicemailBox,
        voicemails: VoicemailService,
        media_ip: IpAddr,
        mut stop_rx: tokio::sync::watch::Receiver<bool>,
    ) {
        let (_info_tx, mut info_rx) = tokio::sync::mpsc::unbounded_channel::<char>();
        let mut peer: Option<SocketAddr> = None;
        // Play the greeting (latching the caller). The window is the greeting's own length plus a
        // small margin (G.711 is 8 bytes/ms); a DTMF keypress skips straight to recording.
        let window = Duration::from_millis((greeting.len() as u64 / 8) + 400);
        tokio::select! {
            _ = ivr::play_and_collect(&sock, &greeting, codec.payload_type(), te_pt, window, &mut info_rx, &mut peer) => {}
            _ = stop_rx.changed() => return, // hung up during the greeting
        }
        // Record only what comes after the tone, until hangup or the cap.
        let capture: rtp::Capture = Arc::new(Mutex::new(Vec::new()));
        ivr::record_until_stop(&sock, te_pt, &capture, &mut stop_rx, IVR_VOICEMAIL_MAX).await;
        let audio = std::mem::take(&mut *capture.lock().expect("capture mutex"));
        if audio.is_empty() {
            tracing::info!(%call_id, "voicemail deposit: nothing recorded after the tone");
            return;
        }
        match voicemails.save(tenant, call_id, None, &audio).await {
            Ok(vm) => {
                tracing::info!(%call_id, voicemail_id = %vm.base.id, bytes = audio.len(), mailbox = %vmbox.aor,
                    "voicemail saved (after greeting)");
                if let Some((addr, contact)) = &vmbox.notify {
                    let number = user_part(&vmbox.aor).unwrap_or("");
                    let (new, old) = voicemails.mailbox_summary(tenant, number).await.unwrap_or((1, 0));
                    send_mwi_notify(*addr, contact, &vmbox.aor, media_ip, new, old).await;
                }
            }
            Err(e) => tracing::warn!(error = %e, %call_id, "saving voicemail failed"),
        }
    }

    /// Drive a `*97`/`*98` retrieval session: (optionally prompt for a mailbox number), announce
    /// the message count, then play each message and act on the DTMF menu — `7` delete, `9` save,
    /// `#`/timeout next. Playing a message marks it read (heard). On exit, push a fresh MWI so the
    /// phone's lamp reflects what remains. All playback is interruptible by hangup.
    #[allow(clippy::too_many_arguments)]
    async fn voicemail_retrieval_driver(
        sock: UdpSocket,
        codec: g711::G711,
        te_pt: u8,
        own_mailbox: Option<String>,
        prompts: RetrievalPrompts,
        voicemails: VoicemailService,
        registrations: RegistrationRegistry,
        tenant: Uuid,
        media_ip: IpAddr,
        mut stop_rx: tokio::sync::watch::Receiver<bool>,
    ) {
        let audio_pt = codec.payload_type();
        let (_info_tx, mut info_rx) = tokio::sync::mpsc::unbounded_channel::<char>();
        let mut peer: Option<SocketAddr> = None;

        // Resolve the mailbox: *97 already knows it (the caller); *98 collects it via DTMF.
        let mailbox = match own_mailbox {
            Some(m) if !m.is_empty() => m,
            _ => {
                let entered = tokio::select! {
                    e = collect_digits(&sock, &prompts.enter_mailbox, audio_pt, te_pt, &mut info_rx, &mut peer) => e,
                    _ = stop_rx.changed() => return,
                };
                match entered {
                    Some(m) if !m.is_empty() => m,
                    _ => return,
                }
            }
        };

        let msgs = match voicemails.list_for_mailbox(tenant, &mailbox).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, mailbox = %mailbox, "voicemail retrieval: list failed");
                return;
            }
        };
        let new_count = msgs.iter().filter(|m| !m.read).count();
        tracing::info!(mailbox = %mailbox, total = msgs.len(), new = new_count, "voicemail retrieval started");

        // "You have N message(s)."
        let count_prompt = build_count_prompt(&prompts, new_count);
        if !count_prompt.is_empty() {
            let window = Duration::from_millis((count_prompt.len() as u64 / 8) + 400);
            tokio::select! {
                _ = ivr::play_and_collect(&sock, &count_prompt, audio_pt, te_pt, window, &mut info_rx, &mut peer) => {}
                _ = stop_rx.changed() => return,
            }
        }

        for vm in &msgs {
            // Fetch + transcode the stored message audio (voicemails are stored as μ-law).
            let audio = match voicemails.get_audio(tenant, vm.base.id).await {
                Ok((_v, raw)) => g711::transcode_ulaw(&raw, codec),
                Err(e) => {
                    tracing::warn!(error = %e, "voicemail retrieval: audio fetch failed");
                    continue;
                }
            };
            if let Some(p) = peer {
                tokio::select! {
                    _ = ivr::play(&sock, p, audio_pt, &audio) => {}
                    _ = stop_rx.changed() => return,
                }
            }
            // Per-message menu: 7 = delete, anything else (9/#/timeout) = keep & mark read.
            let digit = tokio::select! {
                d = ivr::play_and_collect(&sock, &[], audio_pt, te_pt, VM_MENU_TIMEOUT, &mut info_rx, &mut peer) => d,
                _ = stop_rx.changed() => return,
            };
            match digit {
                Some('7') => {
                    if let Err(e) = voicemails.delete(tenant, vm.base.id).await {
                        tracing::warn!(error = %e, "voicemail retrieval: delete failed");
                    } else if let (Some(p), false) = (peer, prompts.deleted.is_empty()) {
                        ivr::play(&sock, p, audio_pt, &prompts.deleted).await;
                    }
                }
                _ => {
                    let _ = voicemails.mark_read(tenant, vm.base.id).await;
                }
            }
        }

        // "No more messages."
        if let (Some(p), false) = (peer, prompts.no_more.is_empty()) {
            ivr::play(&sock, p, audio_pt, &prompts.no_more).await;
        }

        // Refresh the MWI lamp to whatever remains for this mailbox.
        if let Some(reg) = find_registered(&registrations, tenant, &mailbox) {
            if let Some(addr) = resolve_contact_addr(&reg.contact).await {
                let (new, old) = voicemails.mailbox_summary(tenant, &mailbox).await.unwrap_or((0, 0));
                send_mwi_notify(addr, &reg.contact, &reg.aor, media_ip, new, old).await;
            }
        }
        tracing::info!(mailbox = %mailbox, "voicemail retrieval finished");
    }

    /// Answer an INVITE with a plain RTP echo path (the IVR fallback). One UDP socket
    /// reflecting RTP back to the caller.
    async fn answer_with_echo(
        &self,
        resp: &Responder,
        msg: &SipMessage,
        call_id: Uuid,
        call_id_hdr: &str,
    ) -> std::io::Result<()> {
        let (rtp_port, task) = match rtp::bind_echo(None, None).await {
            Ok((port, task)) => (port, Some(task)),
            Err(e) => {
                tracing::warn!(error = %e, "could not bind RTP; answering without media");
                (0, None)
            }
        };
        // Reflect the caller's preferred codec (the echo path relays bytes verbatim).
        let offer = codec::AudioMedia::parse(&String::from_utf8_lossy(msg.body()));
        let audio = offer.preferred_audio().unwrap_or_else(default_codec);
        let te_pt = offer.telephone_event_pt().unwrap_or(dtmf::TELEPHONE_EVENT_PT);
        let sdp = self.build_sdp(rtp_port, &audio, te_pt, None);
        if let Some(task) = task {
            if !call_id_hdr.is_empty() {
                self.dialogs.lock().expect("dialogs mutex").insert(
                    call_id_hdr.to_string(),
                    Dialog {
                        call_id,
                        media: Media::Echo(task),
                        callee: None,
                        caller: None,
                        capture: None,
                        voicemail: None,
                        info_tx: None,
                        answer_sdp: sdp.clone(),
                    },
                );
            } else {
                task.abort();
            }
        }
        self.mark_answered(call_id).await;
        let ok = self.build_invite_ok(msg, &sdp, call_id);
        resp.send(ok.as_bytes()).await
    }

    /// Execute a resolved [`DialPlan`](crate::control::ringplan::DialPlan)'s ring stages
    /// **sequentially**, reusing the single-leg bridge/trunk primitives: for each contact in
    /// each stage, ring a registered internal endpoint ([`Self::try_bridge`]) or an external
    /// number over a trunk ([`Self::try_trunk`]), returning the first leg that answers. `None`
    /// means no contact answered — the caller then applies the plan's `FinalAction`.
    ///
    /// NOTE: a `RING_ALL` stage is currently rung one member at a time (a linear hunt) rather
    /// than truly simultaneously; simultaneous forking (parallel INVITE + CANCEL of the losers)
    /// is a documented media-plane follow-up. The call still reaches the whole group, and the
    /// hunt strategies (`SEQUENTIAL`/`ROUND_ROBIN`/`RANDOM`) and follow-me are already exact.
    #[allow(clippy::type_complexity)]
    async fn execute_ring_plan(
        &self,
        plan: &crate::control::ringplan::DialPlan,
        call_id: Uuid,
        capture: Option<rtp::Capture>,
        offer: &codec::AudioMedia,
        caller_crypto: Option<&sdes::CryptoAttr>,
        caller_id: CallerId<'_>,
    ) -> Option<(rtp::Bridge, CalleeLeg, codec::Codec, u8, Option<sdes::CryptoAttr>)> {
        for stage in &plan.stages {
            for contact in &stage.contacts {
                // An internal registered endpoint takes priority.
                if let Some(reg) = find_registered(&self.registrations, self.default_tenant, contact) {
                    if let Some(won) = self
                        .try_bridge(&reg, call_id, capture.clone(), offer, caller_crypto, caller_id)
                        .await
                    {
                        return Some(won);
                    }
                    continue;
                }
                // Otherwise treat it as an external number reachable over a trunk. Strip the
                // `external:` scheme so E.164 normalisation sees the bare number.
                let external = contact.strip_prefix("external:").unwrap_or(contact);
                if let Some((gw, e164)) = self.select_outbound_gateway(external).await {
                    if let Some(won) = self
                        .try_trunk(&gw, &e164, call_id, capture.clone(), offer, caller_crypto)
                        .await
                    {
                        return Some(won);
                    }
                }
            }
        }
        None
    }

    /// Best-effort outbound (UAC) INVITE to a registered callee, bridged to the caller.
    ///
    /// Binds a two-leg [`rtp::Bridge`], sends an INVITE offering leg B to the callee's
    /// contact over a **dedicated** UDP socket (so it never contends with the main ingress
    /// loop), waits (skipping 1xx) for a 2xx up to the configured no-answer timeout, ACKs it, and
    /// returns the live bridge plus the callee-leg dialog state. Returns `None` — after
    /// aborting the bridge — on any failure (unresolvable contact, no answer, rejection).
    ///
    /// The relay latches onto each side's RTP source address from its first packet, so the
    /// callee's advertised SDP address is not required for media to flow.
    async fn try_bridge(
        &self,
        callee: &Registration,
        call_id: Uuid,
        capture: Option<rtp::Capture>,
        offer: &codec::AudioMedia,
        caller_crypto: Option<&sdes::CryptoAttr>,
        caller_id: CallerId<'_>,
    ) -> Option<(rtp::Bridge, CalleeLeg, codec::Codec, u8, Option<sdes::CryptoAttr>)> {
        let pending = match rtp::bind_bridge_sockets().await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "could not bind RTP bridge");
                return None;
            }
        };

        // Leg A (caller): when the caller offered SRTP, key our side of it and remember the
        // `a=crypto` to answer the caller with. When the caller leg is encrypted, extend SRTP to
        // the callee (leg B) too by offering it a fresh key — end-to-end, both legs encrypted.
        let (leg_a_crypto, srtp_a) = match caller_crypto {
            Some(c) => {
                let (attr, session) = srtp_answer(c);
                (Some(attr), Some(session))
            }
            None => (None, None),
        };
        let legb_offer = leg_a_crypto.as_ref().map(|_| srtp::random_key_salt());
        let legb_crypto = legb_offer.map(|ks| sdes::CryptoAttr { tag: 1, key_salt: ks });

        let addr = match resolve_contact_addr(&callee.contact).await {
            Some(a) => a,
            None => {
                tracing::warn!(contact = %callee.contact, "callee contact is unresolvable");
                return None;
            }
        };

        let sock = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "could not bind outbound SIP socket");
                return None;
            }
        };

        // Our reachable sent-by for the outbound leg: `media_ip` at the ephemeral port we send from
        // and await the response on. The callee returns its 100/180/200 here (see `via_header`);
        // without a reachable Via the answer is lost and the call wrongly diverts to voicemail.
        let sent_by = SocketAddr::new(self.media_ip, sock.local_addr().map(|a| a.port()).unwrap_or(0));

        // Outbound-leg dialog identifiers, derived from the CommOS Call id.
        let leg_call_id = format!("{}@commos", call_id.to_string().replace('-', ""));
        let from_tag: String = call_id.to_string().chars().filter(|c| *c != '-').take(16).collect();
        // Present the *caller's* identity to the callee (its number + display name) so the callee's
        // phone shows who is calling. When the caller is anonymous, fall back to the operator's
        // configurable display name (or the bare "commos") via `caller_from_header`.
        let display = match caller_id.display {
            Some(_) => caller_id.display.map(str::to_string),
            None => self.call_display_name(call_id).await,
        };
        let from_hdr = caller_from_header(self.media_ip, caller_id.number, display.as_deref(), &from_tag);
        let contact_hdr = format!("<sip:commos@{}>", self.media_ip);
        let cseq_num: u32 = 1;

        // Offer the caller's full codec list to the callee on leg B, so the two ends converge on
        // a shared codec CommOS relays untouched (transparent pass-through, no transcoding), plus
        // an SDES key when the caller leg is encrypted.
        let sdp = reoffer_sdp(self.media_ip, pending.leg_b_port, offer, legb_crypto.as_ref());
        let invite = message::request(
            "INVITE",
            &callee.contact,
            &[
                ("Via", message::via_header(sent_by)),
                ("From", from_hdr.clone()),
                ("To", format!("<{}>", callee.aor)),
                ("Call-ID", leg_call_id.clone()),
                ("CSeq", format!("{cseq_num} INVITE")),
                ("Contact", contact_hdr),
            ],
            Some(("application/sdp", &sdp)),
        );

        // Send the INVITE and wait for the callee's final answer, retransmitting until it responds
        // (RFC 3261 client transaction); a non-2xx (or no answer) falls back to voicemail/echo.
        let resp = match send_invite_await_final(&sock, invite.as_bytes(), addr, self.no_answer_timeout).await {
            Some(r) if (200..300).contains(&r.status().unwrap_or(0)) => r,
            _ => return None,
        };

        // Capture the callee's To (with its tag) and Contact for the mid-dialog ACK/BYE.
        let callee_to = resp
            .header("To")
            .map(str::to_string)
            .unwrap_or_else(|| format!("<{}>", callee.aor));
        let callee_target = resp
            .header("Contact")
            .and_then(extract_uri)
            .unwrap_or_else(|| callee.contact.clone());

        // ACK the 2xx (a separate transaction; best-effort dialog headers).
        let ack = message::request(
            "ACK",
            &callee_target,
            &[
                ("Via", message::via_header(sent_by)),
                ("From", from_hdr.clone()),
                ("To", callee_to.clone()),
                ("Call-ID", leg_call_id.clone()),
                ("CSeq", format!("{cseq_num} ACK")),
            ],
            None,
        );
        let _ = sock.send_to(ack.as_bytes(), addr).await;

        // The callee's chosen codec (from its 200 SDP) is what we answer the *caller* with, so
        // both legs use it and the relay is byte-transparent. Fall back to the caller's preferred
        // codec if the callee's answer is unparseable.
        let callee_body = String::from_utf8_lossy(resp.body());
        let callee_answer = codec::AudioMedia::parse(&callee_body);
        let sel_codec = callee_answer
            .preferred_audio()
            .or_else(|| offer.preferred_audio())
            .unwrap_or_else(default_codec);
        let sel_te = callee_answer
            .telephone_event_pt()
            .or_else(|| offer.telephone_event_pt())
            .unwrap_or(dtmf::TELEPHONE_EVENT_PT);

        // Leg B (callee): if we offered SRTP and the callee answered with its own SDES key, key the
        // callee side too. Otherwise leg B is plaintext (a callee that declined SRTP).
        let srtp_b = legb_offer
            .as_ref()
            .and_then(|offered| sdes::CryptoAttr::from_sdp(&callee_body).map(|k| srtp_offered(offered, &k)));
        if leg_a_crypto.is_some() {
            tracing::info!(%call_id, leg_b_encrypted = srtp_b.is_some(),
                "SRTP bridge: caller leg encrypted; callee leg {}",
                if srtp_b.is_some() { "encrypted" } else { "plaintext (callee declined)" });
        }
        let bridge = pending.start(capture, srtp_a, srtp_b);

        let leg = CalleeLeg {
            addr,
            request_uri: callee_target,
            from: from_hdr,
            to: callee_to,
            call_id: leg_call_id,
            cseq: cseq_num,
        };
        Some((bridge, leg, sel_codec, sel_te, leg_a_crypto))
    }

    /// Place an **outbound** call to the PSTN/SIP carrier via `gateway`, bridged to the caller.
    ///
    /// Binds a two-leg [`rtp::Bridge`], sends an INVITE to the gateway for `sip:<e164>@<gateway>`
    /// offering leg B, and — if the carrier challenges with `401`/`407` — retries once with a
    /// digest `Authorization`/`Proxy-Authorization` computed from the carrier's [`Trunk`] auth.
    /// On a 2xx it ACKs and returns the live bridge + callee-leg state (so a BYE tears the trunk
    /// leg down). Returns `None` (after aborting the bridge) on any failure.
    ///
    /// TODO(B2BUA): the outbound leg is best-effort like [`Self::try_bridge`]; full transaction
    /// state / retransmission and codec negotiation with the carrier are future work.
    async fn try_trunk(
        &self,
        gateway: &Gateway,
        e164: &str,
        call_id: Uuid,
        capture: Option<rtp::Capture>,
        offer: &codec::AudioMedia,
        caller_crypto: Option<&sdes::CryptoAttr>,
    ) -> Option<(rtp::Bridge, CalleeLeg, codec::Codec, u8, Option<sdes::CryptoAttr>)> {
        let gw_address = gateway.address.as_deref()?;
        let addr = match resolve_contact_addr(gw_address).await {
            Some(a) => a,
            None => {
                tracing::warn!(gateway = %gw_address, "outbound trunk: gateway address unresolvable");
                return None;
            }
        };
        let pending = rtp::bind_bridge_sockets().await.ok()?;

        // Leg A (caller) SRTP. The carrier (leg B) is offered SRTP only when `trunk_srtp` is on:
        // a carrier that can't answer RTP/SAVP would reject the call, so by default the trunk leg
        // stays plaintext (the caller's access leg is still encrypted) and the call always connects.
        let (leg_a_crypto, srtp_a) = match caller_crypto {
            Some(c) => {
                let (attr, session) = srtp_answer(c);
                (Some(attr), Some(session))
            }
            None => (None, None),
        };
        let legb_offer = (self.trunk_srtp && leg_a_crypto.is_some()).then(srtp::random_key_salt);
        let legb_crypto = legb_offer.map(|ks| sdes::CryptoAttr { tag: 1, key_salt: ks });

        let sock = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "outbound trunk: could not bind SIP socket");
                return None;
            }
        };

        // Our reachable sent-by (see `try_bridge`): the carrier returns its responses here.
        let sent_by = SocketAddr::new(self.media_ip, sock.local_addr().map(|a| a.port()).unwrap_or(0));

        // Request-URI toward the carrier, and outbound-leg dialog identifiers.
        let request_uri = format!("sip:{e164}@{gw_address}");
        let leg_call_id = format!("{}@commos-trunk", call_id.to_string().replace('-', ""));
        let from_tag: String = call_id.to_string().chars().filter(|c| *c != '-').take(16).collect();
        let from_hdr = format!("<sip:commos@{}>;tag={from_tag}", self.media_ip);
        let contact_hdr = format!("<sip:commos@{}>", self.media_ip);
        let cnonce = from_tag.clone();
        // Offer the caller's codec list to the carrier (transparent pass-through), plus an SDES
        // key when the caller leg is encrypted.
        let sdp = reoffer_sdp(self.media_ip, pending.leg_b_port, offer, legb_crypto.as_ref());
        let creds = self.trunk_credentials(gateway.carrier_id).await;

        // Send the INVITE (retransmitting until the carrier responds), retrying once with digest
        // auth if it challenges.
        let mut auth: Option<(&str, String)> = None;
        let mut resp = None;
        for cseq in 1u32..=2 {
            let via = message::via_header(sent_by);
            let mut headers = vec![
                ("Via", via),
                ("From", from_hdr.clone()),
                ("To", format!("<{request_uri}>")),
                ("Call-ID", leg_call_id.clone()),
                ("CSeq", format!("{cseq} INVITE")),
                ("Contact", contact_hdr.clone()),
            ];
            if let Some((name, value)) = &auth {
                headers.push((name, value.clone()));
            }
            let invite = message::request("INVITE", &request_uri, &headers, Some(("application/sdp", &sdp)));
            let msg = match send_invite_await_final(&sock, invite.as_bytes(), addr, self.no_answer_timeout).await {
                Some(m) => m,
                None => return None,
            };
            let status = msg.status().unwrap_or(0);
            if (200..300).contains(&status) {
                resp = Some(msg);
                break;
            }
            // A challenge on the first attempt → compute digest auth and retry.
            if (status == 401 || status == 407) && auth.is_none() {
                let (chdr, ahdr) = if status == 407 {
                    ("Proxy-Authenticate", "Proxy-Authorization")
                } else {
                    ("WWW-Authenticate", "Authorization")
                };
                match (msg.header(chdr).and_then(super::digest::parse_challenge), &creds) {
                    (Some(challenge), Some((user, pass))) => {
                        let value = super::digest::authorization_value(user, pass, "INVITE", &request_uri, &challenge, &cnonce);
                        auth = Some((ahdr, value));
                        continue;
                    }
                    _ => {
                        tracing::warn!(gateway = %gw_address, status, "outbound trunk: auth required but no usable trunk credentials");
                        return None;
                    }
                }
            }
            tracing::info!(gateway = %gw_address, status, "outbound trunk: carrier rejected the call");
            return None;
        }
        let resp = resp?;

        let callee_to = resp.header("To").map(str::to_string).unwrap_or_else(|| format!("<{request_uri}>"));
        let callee_target = resp.header("Contact").and_then(extract_uri).unwrap_or_else(|| request_uri.clone());
        let ack_cseq = if auth.is_some() { 2 } else { 1 };
        let mut ack_headers = vec![
            ("Via", message::via_header(sent_by)),
            ("From", from_hdr.clone()),
            ("To", callee_to.clone()),
            ("Call-ID", leg_call_id.clone()),
            ("CSeq", format!("{ack_cseq} ACK")),
        ];
        if let Some((name, value)) = &auth {
            ack_headers.push((name, value.clone()));
        }
        let ack = message::request("ACK", &callee_target, &ack_headers, None);
        let _ = sock.send_to(ack.as_bytes(), addr).await;
        // The carrier's chosen codec is what we answer the caller with (transparent relay).
        let carrier_body = String::from_utf8_lossy(resp.body());
        let carrier_answer = codec::AudioMedia::parse(&carrier_body);
        let sel_codec = carrier_answer
            .preferred_audio()
            .or_else(|| offer.preferred_audio())
            .unwrap_or_else(default_codec);
        let sel_te = carrier_answer
            .telephone_event_pt()
            .or_else(|| offer.telephone_event_pt())
            .unwrap_or(dtmf::TELEPHONE_EVENT_PT);

        // Leg B (carrier) SRTP, if offered and the carrier answered with its own SDES key.
        let srtp_b = legb_offer
            .as_ref()
            .and_then(|offered| sdes::CryptoAttr::from_sdp(&carrier_body).map(|k| srtp_offered(offered, &k)));
        tracing::info!(%call_id, gateway = %gw_address, %e164, leg_b_port = pending.leg_b_port,
            codec = %sel_codec.name, srtp = leg_a_crypto.is_some(), carrier_srtp = srtp_b.is_some(),
            "outbound trunk: call placed to carrier");
        let bridge = pending.start(capture, srtp_a, srtp_b);

        let leg = CalleeLeg {
            addr,
            request_uri: callee_target,
            from: from_hdr,
            to: callee_to,
            call_id: leg_call_id,
            cseq: ack_cseq,
        };
        Some((bridge, leg, sel_codec, sel_te, leg_a_crypto))
    }

    /// Mid-dialog BYE toward one leg of a bridged call, sent as a reliable non-INVITE
    /// transaction: retransmitted until the endpoint confirms with a final response (or the
    /// transaction times out), so a lost BYE still tears that leg down. Used for both directions —
    /// the callee leg (caller hung up) and the caller leg (callee hung up).
    ///
    /// TODO(B2BUA): this reconstructs the BYE from captured identifiers only; full RFC 3261
    /// mid-dialog correctness (route sets, contact refresh) is still out of scope.
    async fn send_bye_to_leg(&self, leg: &CalleeLeg) {
        // Bind the socket first so the BYE's Via advertises the exact port we await the response
        // on: with an unreachable Via the endpoint's 200-to-BYE is lost and the transaction is
        // retransmitted needlessly (the leg still tears down, but unconfirmed).
        let sock = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(error = %e, addr = %leg.addr, "could not bind socket for BYE");
                return;
            }
        };
        let sent_by = SocketAddr::new(self.media_ip, sock.local_addr().map(|a| a.port()).unwrap_or(0));
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
            tracing::info!(addr = %leg.addr, "bridged leg BYE confirmed");
        } else {
            tracing::debug!(addr = %leg.addr, "bridged leg BYE unconfirmed (no final response)");
        }
    }

    /// Persist captured caller audio as a call [`Recording`], fire-and-forget off the BYE path.
    fn spawn_save_recording(&self, call_id: Uuid, bytes: Vec<u8>) {
        let recordings = self.recordings.clone();
        let tenant = self.default_tenant;
        let n = bytes.len();
        tokio::spawn(async move {
            match recordings.save(tenant, call_id, &bytes).await {
                Ok(_) => tracing::info!(%call_id, bytes = n, "call recording saved"),
                Err(e) => tracing::warn!(error = %e, %call_id, "saving call recording failed"),
            }
        });
    }

    /// Persist captured caller audio as a [`Voicemail`] for `vmbox`, then push a message-waiting
    /// indication to the mailbox's phone if it is registered — fire-and-forget off the BYE path.
    fn spawn_save_voicemail(&self, call_id: Uuid, vmbox: VoicemailBox, bytes: Vec<u8>) {
        let voicemails = self.voicemails.clone();
        let tenant = self.default_tenant;
        let media_ip = self.media_ip;
        let n = bytes.len();
        tokio::spawn(async move {
            let vm = match voicemails.save(tenant, call_id, None, &bytes).await {
                Ok(vm) => vm,
                Err(e) => {
                    tracing::warn!(error = %e, %call_id, "saving voicemail failed");
                    return;
                }
            };
            tracing::info!(%call_id, voicemail_id = %vm.base.id, bytes = n, mailbox = %vmbox.aor,
                "voicemail saved");
            // Push MWI to the mailbox's registered contact, if any (an offline mailbox gets its
            // MWI on the phone's next REGISTER instead).
            if let Some((addr, contact_uri)) = &vmbox.notify {
                let number = user_part(&vmbox.aor).unwrap_or("");
                let (new, old) = voicemails.mailbox_summary(tenant, number).await.unwrap_or((1, 0));
                send_mwi_notify(*addr, contact_uri, &vmbox.aor, media_ip, new, old).await;
            }
        });
    }

    /// After a device (re-)registers, light its message-waiting lamp: if the mailbox for `aor`
    /// has unheard voicemails, push an MWI NOTIFY to its fresh `contact`. Fire-and-forget so the
    /// REGISTER `200 OK` is never delayed.
    fn maybe_notify_mwi(&self, aor: String, contact: String) {
        let voicemails = self.voicemails.clone();
        let tenant = self.default_tenant;
        let media_ip = self.media_ip;
        tokio::spawn(async move {
            let Some(number) = user_part(&aor) else { return };
            let (new, old) = match voicemails.mailbox_summary(tenant, number).await {
                Ok(s) => s,
                Err(_) => return,
            };
            if new == 0 {
                return; // Nothing waiting — do not bother the phone.
            }
            if let Some(addr) = resolve_contact_addr(&contact).await {
                send_mwi_notify(addr, &contact, &aor, media_ip, new, old).await;
            }
        });
    }

    /// BYE/CANCEL: hang the Call up (produces the CDR), abort its RTP, and `200 OK`.
    async fn on_bye(&self, resp: &Responder, msg: &SipMessage) -> std::io::Result<()> {
        let method = msg.method().unwrap_or("BYE").to_string();
        if let Some(incoming_call_id) = msg.call_id() {
            // The incoming Call-ID is either a primary (caller-leg) dialog key, or — for a
            // bridged call whose callee hung up — a callee-leg alias pointing at the primary.
            let (primary, from_callee) = {
                let dialogs = self.dialogs.lock().expect("dialogs mutex");
                if dialogs.contains_key(incoming_call_id) {
                    (Some(incoming_call_id.to_string()), false)
                } else {
                    let alias = self
                        .bye_aliases
                        .lock()
                        .expect("aliases mutex")
                        .get(incoming_call_id)
                        .cloned();
                    (alias, true)
                }
            };

            if let Some(primary) = primary {
                let dialog = self.dialogs.lock().expect("dialogs mutex").remove(&primary);
                if let Some(d) = dialog {
                    if let Some(callee) = &d.callee {
                        // Drop the callee-leg alias index.
                        self.bye_aliases
                            .lock()
                            .expect("aliases mutex")
                            .remove(&callee.call_id);
                        if from_callee {
                            // The CALLEE hung up: propagate the BYE to the CALLER so its phone
                            // disconnects too (otherwise it stays "up" with dead air). The callee
                            // is already gone, so we do not echo a BYE back to it.
                            if let Some(caller) = &d.caller {
                                self.send_bye_to_leg(caller).await;
                            }
                        } else {
                            // The CALLER hung up: tear the callee leg down with a BYE.
                            self.send_bye_to_leg(callee).await;
                        }
                    }
                    d.media.abort();
                    // Drain the captured audio and persist it (as-is, no transcode), off the BYE
                    // path so the caller's 200 OK isn't delayed by the object write. A voicemail
                    // dialog stores a Voicemail and pushes MWI; otherwise, when call recording is
                    // on, it stores a Recording.
                    if let Some(cap) = &d.capture {
                        let bytes = std::mem::take(&mut *cap.lock().expect("capture mutex"));
                        if !bytes.is_empty() {
                            match d.voicemail {
                                Some(vmbox) => self.spawn_save_voicemail(d.call_id, vmbox, bytes),
                                None => self.spawn_save_recording(d.call_id, bytes),
                            }
                        }
                    }
                    if let Err(e) = self
                        .routing
                        .hangup(self.default_tenant, d.call_id, Some(method.clone()))
                        .await
                    {
                        tracing::warn!(error = %e, call_id = %d.call_id, "SIP {method} hangup failed");
                    } else {
                        tracing::info!(method = %method, call_id = %d.call_id, "SIP {method} → hangup");
                    }
                }
            }
        }
        self.reply(resp, msg, 200, "OK").await
    }

    /// Build the SDP answer advertising `rtp_port` for the negotiated `audio` codec + DTMF
    /// payload type `te_pt` (see [`media_sdp`]). Passes `crypto` through so an SRTP-negotiated
    /// endpoint answers over `RTP/SAVP` with its SDES key.
    fn build_sdp(&self, rtp_port: u16, audio: &codec::Codec, te_pt: u8, crypto: Option<&sdes::CryptoAttr>) -> String {
        media_sdp(self.media_ip, rtp_port, audio, te_pt, crypto)
    }

    /// Negotiate SRTP for an endpoint media path from the caller's SDP `body`: when SRTP is
    /// enabled and the caller offers the secure profile with a supported SDES key, return the
    /// `a=crypto` line to advertise back (a fresh CommOS key) and the [`srtp::SrtpSession`] that
    /// keys the media task — `inbound` from the caller's key, `outbound` from ours. `None` when
    /// SRTP is off or the caller offered plain RTP (which is then answered in the clear).
    fn negotiate_srtp(&self, body: &str, secure: bool) -> Option<(sdes::CryptoAttr, srtp::SrtpSession)> {
        Some(srtp_answer(&self.caller_crypto(body, secure)?))
    }

    /// The caller's SDES key from its SDP `body`, when SRTP is enabled, the caller offered the
    /// secure profile, AND the signalling arrived over a confidential (TLS) transport. SDES puts
    /// the master key in the SDP, so accepting it over plaintext UDP would hand the key to any
    /// passive observer of the signalling — no real confidentiality. Over cleartext transports we
    /// therefore decline SRTP and answer plain RTP rather than pretend to encrypt. `None` for a
    /// plain-RTP caller or an insecure transport.
    fn caller_crypto(&self, body: &str, secure: bool) -> Option<sdes::CryptoAttr> {
        (self.srtp_enabled && secure && sdes::offers_savp(body))
            .then(|| sdes::CryptoAttr::from_sdp(body))
            .flatten()
    }

    /// Mark the Call ANSWERED at the instant CommOS answers the caller with 200 OK — the true
    /// connect time. Called from every answer path (bridge, trunk, voicemail, echo, IVR) just
    /// before the 200 OK goes out. Best-effort: a failure is logged, never fatal to the call
    /// (an already-answered Call — e.g. an IVR that then bridges — simply logs an illegal
    /// transition, which is harmless).
    async fn mark_answered(&self, call_id: Uuid) {
        if let Err(e) = self
            .routing
            .apply_fact(MediaFact::Answered {
                tenant_id: self.default_tenant,
                call_id,
                answered_at: Timestamp::now(),
            })
            .await
        {
            tracing::debug!(error = %e, %call_id, "marking call answered failed");
        }
    }

    /// Capture the caller leg's dialog identifiers from its INVITE, so a callee-originated BYE can
    /// be propagated back to the caller (tearing its phone down too). CommOS is the UAS on this
    /// leg: our local identity is the caller's `To` plus the tag we answered with, the remote is
    /// the caller's `From`, and the BYE is sent to the caller's actual socket (`src`) — reliable on
    /// UDP even behind NAT. Uses the caller's `Contact` as the request-URI, falling back to `From`.
    fn caller_leg(&self, msg: &SipMessage, call_id: Uuid, src: SocketAddr) -> CalleeLeg {
        let our_tag: String = call_id.to_string().chars().filter(|c| *c != '-').take(16).collect();
        let from = match msg.header("To") {
            Some(to) if msg.to_tag().is_some() => to.to_string(),
            Some(to) => format!("{to};tag={our_tag}"),
            None => format!("<sip:commos@{}>;tag={our_tag}", self.media_ip),
        };
        let to = msg
            .header("From")
            .map(str::to_string)
            .unwrap_or_else(|| format!("<sip:{src}>"));
        let request_uri = msg
            .header("Contact")
            .and_then(extract_uri)
            .or_else(|| msg.header("From").and_then(extract_uri))
            .unwrap_or_else(|| format!("sip:{src}"));
        CalleeLeg {
            addr: src,
            request_uri,
            from,
            to,
            call_id: msg.call_id().unwrap_or("").to_string(),
            cseq: 1,
        }
    }

    /// The display name a called phone should show as the calling party for this call — the
    /// operator's configurable text, read from `display_name_file`. One non-empty line → that text
    /// on every call; multiple lines → one selected per call (varied by `call_id`). `None` when the
    /// file is absent or empty, so callers fall back to the bare "commos" identity. Re-read per
    /// call so edits to the file apply without a restart (the file is tiny and local).
    async fn call_display_name(&self, call_id: Uuid) -> Option<String> {
        let content = tokio::fs::read_to_string(&self.display_name_file).await.ok()?;
        let lines: Vec<&str> = content.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        let line = match lines.len() {
            0 => return None,
            1 => lines[0],
            n => lines[display_line_index(call_id, n)],
        };
        let sanitized = sip_display_name(line);
        (!sanitized.is_empty()).then_some(sanitized)
    }

    /// Build a `180 Ringing` provisional for the caller's INVITE, establishing the early dialog
    /// with the same To-tag the eventual 200 OK will carry (so the phone treats them as one
    /// dialog and shows a ringing indication while CommOS rings the callee). No SDP.
    fn build_ringing(&self, msg: &SipMessage, call_id: Uuid) -> String {
        let mut out = String::with_capacity(256);
        out.push_str("SIP/2.0 180 Ringing\r\n");
        for via in msg.header_all("Via") {
            out.push_str(&format!("Via: {via}\r\n"));
        }
        if let Some(from) = msg.header("From") {
            out.push_str(&format!("From: {from}\r\n"));
        }
        if let Some(to) = msg.header("To") {
            if msg.to_tag().is_some() {
                out.push_str(&format!("To: {to}\r\n"));
            } else {
                // Same tag derivation as `build_invite_ok`, so 180 and 200 share one dialog.
                let tag: String = call_id.to_string().chars().filter(|c| *c != '-').take(16).collect();
                out.push_str(&format!("To: {to};tag={tag}\r\n"));
            }
        }
        if let Some(cid) = msg.call_id() {
            out.push_str(&format!("Call-ID: {cid}\r\n"));
        }
        if let Some(cseq) = msg.header("CSeq") {
            out.push_str(&format!("CSeq: {cseq}\r\n"));
        }
        out.push_str(&format!("Contact: <sip:commos@{}>\r\n", self.media_ip));
        out.push_str("Server: commosd\r\n");
        out.push_str("Content-Length: 0\r\n\r\n");
        out
    }

    /// Build a `200 OK` for an INVITE with an SDP body, echoing the dialog headers. (The
    /// bodyless [`message::response`] builder can't carry SDP, so INVITE answers are built
    /// here.)
    fn build_invite_ok(&self, msg: &SipMessage, sdp: &str, call_id: Uuid) -> String {
        let mut out = String::with_capacity(512);
        out.push_str("SIP/2.0 200 OK\r\n");
        for via in msg.header_all("Via") {
            out.push_str(&format!("Via: {via}\r\n"));
        }
        if let Some(from) = msg.header("From") {
            out.push_str(&format!("From: {from}\r\n"));
        }
        if let Some(to) = msg.header("To") {
            if msg.to_tag().is_some() {
                out.push_str(&format!("To: {to}\r\n"));
            } else {
                // Our (callee) dialog tag, derived from the Call it created.
                let tag: String = call_id.to_string().chars().filter(|c| *c != '-').take(16).collect();
                out.push_str(&format!("To: {to};tag={tag}\r\n"));
            }
        }
        if let Some(cid) = msg.call_id() {
            out.push_str(&format!("Call-ID: {cid}\r\n"));
        }
        if let Some(cseq) = msg.header("CSeq") {
            out.push_str(&format!("CSeq: {cseq}\r\n"));
        }
        out.push_str(&format!("Contact: <sip:commos@{}>\r\n", self.media_ip));
        out.push_str("Server: commosd\r\n");
        out.push_str("Content-Type: application/sdp\r\n");
        out.push_str(&format!("Content-Length: {}\r\n\r\n", sdp.len()));
        out.push_str(sdp);
        out
    }

    async fn reply(
        &self,
        resp: &Responder,
        msg: &SipMessage,
        status: u16,
        reason: &str,
    ) -> std::io::Result<()> {
        resp.send(message::response(msg, status, reason).as_bytes()).await
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
fn caller_from_header(media_ip: IpAddr, number: Option<&str>, display: Option<&str>, tag: &str) -> String {
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
    codec::Codec { pt: 0, name: "PCMU".to_string(), clock: 8000 }
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
    (sdes::CryptoAttr { tag: theirs.tag, key_salt: ours }, session)
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
fn media_sdp(media_ip: IpAddr, port: u16, audio: &codec::Codec, te_pt: u8, crypto: Option<&sdes::CryptoAttr>) -> String {
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
fn reoffer_sdp(media_ip: IpAddr, port: u16, offer: &codec::AudioMedia, crypto: Option<&sdes::CryptoAttr>) -> String {
    let te = offer.telephone_event_pt().unwrap_or(dtmf::TELEPHONE_EVENT_PT);
    let (pts, rtpmaps) = offer.reoffer_lines();
    let (pts, rtpmaps) = if pts.trim().is_empty() {
        (format!("0 {te}"), format!("a=rtpmap:0 PCMU/8000\r\na=rtpmap:{te} telephone-event/8000\r\n"))
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
fn find_registered(
    regs: &RegistrationRegistry,
    tenant: Uuid,
    dest: &str,
) -> Option<Registration> {
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
    buf.extend_from_slice(if count == 1 { &prompts.message } else { &prompts.messages });
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
        match ivr::play_and_collect(sock, this_prompt, audio_pt, te_pt, window, info_rx, peer).await {
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
    let (sock_b, sig) = match (UdpSocket::bind("0.0.0.0:0").await, UdpSocket::bind("0.0.0.0:0").await) {
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
    let from_tag: String = call_id.to_string().chars().filter(|c| *c != '-').take(16).collect();
    let from_hdr = commos_from_header(media_ip, display.as_deref(), &from_tag);
    // Offer the callee the IVR caller's negotiated codec (the caller is already on it).
    let audio = codec::Codec { pt: g711.payload_type(), name: g711.sdp_name().to_string(), clock: 8000 };
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
    let callee_to = resp.header("To").map(str::to_string).unwrap_or_else(|| format!("<{}>", callee.aor));
    let callee_target = resp.header("Contact").and_then(extract_uri).unwrap_or_else(|| callee.contact.clone());
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
    let host_port = host_part.split([';', '?', '>']).next().unwrap_or(host_part).trim();
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
        assert_eq!(commos_from_header(ip, Some(""), "abc"), "<sip:commos@10.0.0.5>;tag=abc");
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
        assert_eq!(caller_from_header(ip, Some(""), None, "tg"), "<sip:commos@10.0.0.5>;tag=tg");
    }

    #[test]
    fn header_display_name_extracts_quoted_bare_and_none() {
        assert_eq!(header_display_name("\"Alice\" <sip:100@host>;tag=x").as_deref(), Some("Alice"));
        assert_eq!(header_display_name("Bob <sip:101@host>").as_deref(), Some("Bob"));
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
        assert!(via.starts_with("SIP/2.0/UDP 10.0.0.5:41000;rport;branch=z9hG4bK"),
            "reachable sent-by + rport + magic-cookie branch: {via}");
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
        send_mwi_notify(phone_addr, "sip:200@127.0.0.1", "sip:200@host", media_ip, 2, 1).await;

        let mut buf = vec![0u8; 2048];
        let (n, _from) = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            phone.recv_from(&mut buf),
        )
        .await
        .expect("MWI NOTIFY not received")
        .expect("recv");

        let msg = message::parse(&buf[..n]).expect("NOTIFY parses");
        assert_eq!(msg.method(), Some("NOTIFY"));
        assert_eq!(msg.header("Event"), Some("message-summary"));
        assert_eq!(msg.header("Subscription-State"), Some("active"));
        assert_eq!(msg.header("Content-Type"), Some("application/simple-message-summary"));
        assert_eq!(msg.header("To"), Some("<sip:200@host>"));

        let body = String::from_utf8_lossy(msg.body());
        assert!(body.contains("Messages-Waiting: yes"), "body: {body}");
        assert!(body.contains("Voice-Message: 2/1 (0/0)"), "body: {body}");
        assert!(body.contains("Message-Account: sip:200@host"), "body: {body}");
    }

    /// When the mailbox is empty, the summary says "no" waiting messages.
    #[tokio::test]
    async fn mwi_notify_reports_no_waiting_when_empty() {
        let phone = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let phone_addr = phone.local_addr().unwrap();
        let media_ip: IpAddr = "127.0.0.1".parse().unwrap();

        send_mwi_notify(phone_addr, "sip:200@127.0.0.1", "sip:200@host", media_ip, 0, 0).await;

        let mut buf = vec![0u8; 2048];
        let (n, _from) = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            phone.recv_from(&mut buf),
        )
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
            peer.send_to(b"SIP/2.0 200 OK\r\nContent-Length: 0\r\n\r\n", from).await.unwrap();
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
            callee.send_to(b"SIP/2.0 180 Ringing\r\nContent-Length: 0\r\n\r\n", from).await.unwrap();
            callee.send_to(b"SIP/2.0 200 OK\r\nContent-Length: 0\r\n\r\n", from).await.unwrap();
        });
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let invite = b"INVITE sip:x@127.0.0.1 SIP/2.0\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
        let resp = send_invite_await_final(&sock, invite, callee_addr, Duration::from_secs(3)).await;
        assert_eq!(resp.and_then(|m| m.status()), Some(200), "should return the final 200");
        callee_task.await.unwrap();
    }
}
