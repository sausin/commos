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
use tokio::time::{timeout, Duration};

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
use super::{codec, dtmf, g711, ivr, rtp, sdes, srtp};

/// Largest UDP SIP datagram we accept (the UDP ceiling; ample for INVITE+SDP).
const MAX_DATAGRAM: usize = 65_535;

/// Cap an IVR-deposited voicemail recording (~2 min of G.711) so an abandoned line can't
/// record forever; a graceful hangup (BYE) stops it sooner.
const IVR_VOICEMAIL_MAX: Duration = Duration::from_secs(120);

/// How long we wait for a registered callee to answer our outbound INVITE before falling
/// back to the echo path so the caller's dial still completes.
const CALLEE_ANSWER_TIMEOUT: Duration = Duration::from_secs(4);

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

/// The callee leg of a bridged (B2BUA) call — the dialog identifiers we need to send a
/// mid-dialog BYE toward the callee when the caller hangs up.
///
/// TODO(B2BUA): this is best-effort. We reconstruct a BYE from the identifiers captured when
/// the callee answered, but full RFC 3261 mid-dialog correctness (route sets, contact
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
}

/// The UDP SIP server. [`Self::run`] takes ownership and drives the receive loop.
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
    /// Require SIP digest auth on REGISTER/INVITE (Volume 9).
    require_auth: bool,
    /// Digest realm advertised in the auth challenge.
    realm: String,
    /// Nonces we have issued → their expiry (unix seconds). In-memory; a restart re-challenges.
    nonces: Arc<Mutex<HashMap<String, i64>>>,
    /// Record calls (Volume 7): capture the caller's RTP audio and persist it on hangup.
    record_calls: bool,
    /// Recording service used to store captured audio when `record_calls` is on.
    recordings: RecordingService,
    /// Take a voicemail when an internal callee does not answer / is offline (Volume 7).
    voicemail_enabled: bool,
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
        voicemails: VoicemailService,
        ivrs: IvrService,
        objects: ObjectService,
        default_cc: impl Into<String>,
        srtp_enabled: bool,
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
            voicemails,
            ivrs,
            objects,
            default_cc: default_cc.into(),
            srtp_enabled,
        }
    }

    /// How long a challenge nonce stays valid (seconds).
    const NONCE_TTL: i64 = 300;

    /// Issue a fresh nonce, remember it, and return it.
    fn issue_nonce(&self) -> String {
        let nonce: String = format!("{}{}", Uuid::now_v7(), Uuid::now_v7())
            .chars()
            .filter(|c| c.is_ascii_hexdigit())
            .take(32)
            .collect();
        let exp = now_unix() + Self::NONCE_TTL;
        let mut g = self.nonces.lock().expect("nonces mutex");
        g.retain(|_, &mut e| e > now_unix());
        g.insert(nonce.clone(), exp);
        nonce
    }

    /// Whether `nonce` is one we issued and it hasn't expired.
    fn nonce_known(&self, nonce: &str) -> bool {
        let now = now_unix();
        let mut g = self.nonces.lock().expect("nonces mutex");
        g.retain(|_, &mut e| e > now);
        g.get(nonce).is_some_and(|&e| e > now)
    }

    /// Verify the request's `Authorization` digest for `method` against the stored per-device
    /// secret. Returns true only when the nonce is ours, the username has a credential, and the
    /// response hash matches.
    async fn digest_ok(&self, msg: &SipMessage, method: &str) -> bool {
        let creds = match msg.header("Authorization").and_then(super::digest::Credentials::parse) {
            Some(c) => c,
            None => return false,
        };
        if !self.nonce_known(&creds.nonce) {
            return false;
        }
        match self.store.get_sip_credential(self.default_tenant, &creds.username).await {
            Ok(Some(secret)) => super::digest::verify(&creds, method, &secret),
            _ => false,
        }
    }

    /// Send a `401 Unauthorized` with a fresh digest challenge, prompting the phone to
    /// re-send the request with an `Authorization` header.
    async fn send_challenge(
        &self,
        socket: &UdpSocket,
        msg: &SipMessage,
        src: SocketAddr,
    ) -> std::io::Result<()> {
        let challenge = super::digest::Challenge::new(self.realm.clone(), self.issue_nonce());
        let resp = message::response_with(
            msg,
            401,
            "Unauthorized",
            &[("WWW-Authenticate", challenge.header_value())],
        );
        self.send(socket, resp.as_bytes(), src).await
    }

    /// Bind `bind` and serve SIP over UDP forever. Returns only on a fatal socket error.
    pub async fn run(self, bind: SocketAddr) -> std::io::Result<()> {
        let socket = UdpSocket::bind(bind).await?;
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
            if let Err(e) = self.handle(&socket, &buf[..len], src).await {
                tracing::debug!(error = %e, %src, "dropping SIP datagram");
            }
        }
    }

    async fn handle(
        &self,
        socket: &UdpSocket,
        datagram: &[u8],
        src: SocketAddr,
    ) -> std::io::Result<()> {
        let msg = match message::parse(datagram) {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(error = %e, %src, "unparseable SIP datagram");
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
            "REGISTER" => self.on_register(socket, &msg, src).await,
            "OPTIONS" => {
                tracing::info!(method = %method, %src, "SIP OPTIONS");
                self.reply(socket, &msg, 200, "OK", src).await
            }
            "INVITE" => self.on_invite(socket, &msg, src).await,
            "ACK" => {
                tracing::info!(method = %method, %src, "SIP ACK");
                Ok(())
            }
            "BYE" | "CANCEL" => self.on_bye(socket, &msg, src).await,
            "INFO" => self.on_info(socket, &msg, src).await,
            other => {
                tracing::info!(method = %other, %src, "SIP method not implemented");
                self.reply(socket, &msg, 501, "Not Implemented", src).await
            }
        }
    }

    /// INFO: out-of-band DTMF (`application/dtmf-relay` / `application/dtmf`). If the datagram's
    /// dialog is a live IVR session, inject the pressed digit into it; always `200 OK`.
    async fn on_info(
        &self,
        socket: &UdpSocket,
        msg: &SipMessage,
        src: SocketAddr,
    ) -> std::io::Result<()> {
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
        self.reply(socket, msg, 200, "OK", src).await
    }

    /// REGISTER: bind the AoR to its contact and confirm with a `200 OK`.
    async fn on_register(
        &self,
        socket: &UdpSocket,
        msg: &SipMessage,
        src: SocketAddr,
    ) -> std::io::Result<()> {
        // Digest auth gate (Volume 9): an unauthenticated REGISTER is challenged with 401 +
        // WWW-Authenticate; the phone re-sends with credentials we verify against its stored
        // per-device secret.
        if self.require_auth && !self.digest_ok(msg, "REGISTER").await {
            tracing::info!(%src, "SIP REGISTER challenged (digest auth required)");
            return self.send_challenge(socket, msg, src).await;
        }

        let aor = match msg.register_aor() {
            Some(a) if !a.is_empty() => a,
            _ => {
                tracing::debug!(%src, "REGISTER without a usable To/From AoR");
                return self.reply(socket, msg, 400, "Bad Request", src).await;
            }
        };
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
        let resp = message::response_with(msg, 200, "OK", &extra);
        let sent = self.send(socket, resp.as_bytes(), src).await;

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
    async fn on_invite(
        &self,
        socket: &UdpSocket,
        msg: &SipMessage,
        src: SocketAddr,
    ) -> std::io::Result<()> {
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

        // Digest auth gate: an unauthenticated INVITE is challenged with 401 before any Call is
        // created; the phone re-sends with credentials. (REGISTER auth already limits who is
        // reachable; challenging INVITE too stops direct unauthenticated dialing.)
        if self.require_auth && !self.digest_ok(msg, "INVITE").await {
            tracing::info!(%src, "SIP INVITE challenged (digest auth required)");
            return self.send_challenge(socket, msg, src).await;
        }

        // Provisional response.
        let trying = message::response(msg, 100, "Trying");
        self.send(socket, trying.as_bytes(), src).await?;

        // Create the inbound Call in the control plane.
        let call = match self
            .routing
            .create_inbound_call(self.default_tenant, from_ref, to_ref)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "INVITE could not create Call");
                return self.reply(socket, msg, 500, "Server Internal Error", src).await;
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

        // SIP is the media plane here: report ring then answer as facts. Applied
        // synchronously (we hold the Routing handle) so the Call is ANSWERED before we send
        // 200 OK — a fast BYE then always finds an answered Call to hang up.
        for fact in [
            MediaFact::Rang { tenant_id: self.default_tenant, call_id },
            MediaFact::Answered {
                tenant_id: self.default_tenant,
                call_id,
                answered_at: Timestamp::now(),
            },
        ] {
            if let Err(e) = self.routing.apply_fact(fact).await {
                tracing::warn!(error = %e, %call_id, "applying SIP media fact failed");
            }
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

        // Inbound DID: an INVITE from a carrier to a provisioned external number is routed to its
        // `destination_ref`. The effective target is that DID destination if matched, else the
        // extension route, else the raw request-URI.
        let did_dest = self.resolve_did(request_uri).await;
        if did_dest.is_some() {
            tracing::info!(%call_id, number = %request_uri, dest = ?did_dest, "inbound DID routed");
        }
        let target: String = did_dest.clone().unwrap_or_else(|| effective_uri.to_string());

        // The target (from a DID or an extension route) may name an `ivr:<id>` menu: run the IVR
        // runtime (answer with SDP, play the prompt, collect DTMF).
        if let Some(ivr_id) = match ivr_id_of(&target) {
            Some(id) => Some(id),
            None => self.resolve_ivr_target(request_uri).await,
        } {
            return self.answer_with_ivr(socket, msg, src, call_id, &call_id_hdr, ivr_id, g711, te_pt).await;
        }

        let mut voicemail_target: Option<VoicemailBox> = None;
        // A direct voicemail target (e.g. a DID → "voicemail").
        if target == "voicemail" || target.starts_with("voicemail:") {
            voicemail_target = Some(VoicemailBox { aor: target.clone(), notify: None });
        }

        // If the target names a REGISTERED endpoint, bridge the two legs: bind a two-leg RTP
        // relay, INVITE the callee (offering leg B), and on its 200 OK answer the caller
        // (offering leg A). A callee that never answers (or an internal extension that is
        // offline) diverts to voicemail.
        if voicemail_target.is_none() {
            if let Some(callee_reg) = find_registered(&self.registrations, self.default_tenant, &target) {
                match self.try_bridge(&callee_reg, call_id, capture.clone(), &offer).await {
                    Some((bridge, callee_leg, sel_codec, sel_te)) => {
                        let leg_a_port = bridge.leg_a_port;
                        if !call_id_hdr.is_empty() {
                            let callee_call_id = callee_leg.call_id.clone();
                            self.dialogs.lock().expect("dialogs mutex").insert(
                                call_id_hdr.clone(),
                                Dialog {
                                    call_id,
                                    media: Media::Bridge(bridge),
                                    callee: Some(callee_leg),
                                    capture: capture.clone(),
                                    voicemail: None,
                                    info_tx: None,
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
                        // Answer the caller with the codec the callee selected (transparent relay).
                        let sdp = self.build_sdp(leg_a_port, &sel_codec, sel_te, None);
                        let ok = self.build_invite_ok(msg, &sdp, call_id);
                        tracing::info!(%call_id, leg_a_port, callee = %callee_reg.contact,
                            codec = %sel_codec.name, "SIP INVITE bridged to registered callee");
                        return self.send(socket, ok.as_bytes(), src).await;
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
                if let Some((bridge, leg, sel_codec, sel_te)) = self.try_trunk(&gateway, &e164, call_id, capture.clone(), &offer).await {
                    let leg_a_port = bridge.leg_a_port;
                    if !call_id_hdr.is_empty() {
                        let callee_call_id = leg.call_id.clone();
                        self.dialogs.lock().expect("dialogs mutex").insert(
                            call_id_hdr.clone(),
                            Dialog {
                                call_id,
                                media: Media::Bridge(bridge),
                                callee: Some(leg),
                                capture: capture.clone(),
                                voicemail: None,
                                info_tx: None,
                            },
                        );
                        self.bye_aliases.lock().expect("aliases mutex").insert(callee_call_id, call_id_hdr.clone());
                    } else {
                        bridge.abort();
                    }
                    // Answer the caller with the carrier's selected codec (transparent relay).
                    let sdp = self.build_sdp(leg_a_port, &sel_codec, sel_te, None);
                    let ok = self.build_invite_ok(msg, &sdp, call_id);
                    tracing::info!(%call_id, %e164, gateway = ?gateway.address, codec = %sel_codec.name, "SIP INVITE routed outbound via trunk");
                    return self.send(socket, ok.as_bytes(), src).await;
                }
                tracing::warn!(%call_id, %e164, "outbound trunk failed; falling back to echo");
            }
        }

        // Voicemail path: answer the caller and capture their audio — stored as a Voicemail on
        // hangup — regardless of `record_calls` (a voicemail is always captured). Greeting/beep
        // prompt playback is future work (it ties into the IVR prompt runtime); for now the
        // caller is connected to a capturing echo, exactly as the recording path is.
        if let Some(vmbox) = voicemail_target {
            let vm_capture: rtp::Capture = Arc::new(Mutex::new(Vec::new()));
            // Encrypt the voicemail media path when the caller offers SRTP: the capture stores the
            // decrypted G.711 (as before), only the wire is protected.
            let (vm_crypto, vm_srtp) = match self.negotiate_srtp(&body) {
                Some((c, s)) => (Some(c), Some(s)),
                None => (None, None),
            };
            let rtp_port = match rtp::bind_echo(Some(vm_capture.clone()), vm_srtp).await {
                Ok((port, task)) => {
                    if !call_id_hdr.is_empty() {
                        self.dialogs.lock().expect("dialogs mutex").insert(
                            call_id_hdr,
                            Dialog {
                                call_id,
                                media: Media::Echo(task),
                                callee: None,
                                capture: Some(vm_capture),
                                voicemail: Some(vmbox),
                                info_tx: None,
                            },
                        );
                    } else {
                        task.abort();
                    }
                    port
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not bind RTP for voicemail; answering without media");
                    0
                }
            };
            // Answer with G.711 so the captured voicemail is a storable/playable codec.
            let sdp = self.build_sdp(rtp_port, &g711_codec, te_pt, vm_crypto.as_ref());
            let ok = self.build_invite_ok(msg, &sdp, call_id);
            tracing::info!(%call_id, rtp_port, codec = %g711_codec.name, srtp = vm_crypto.is_some(), "SIP INVITE answered (voicemail)");
            return self.send(socket, ok.as_bytes(), src).await;
        }

        // Echo path: non-mailbox destination (PSTN-style / +E.164), or a no-answer with
        // voicemail disabled. One UDP socket reflecting RTP back to the caller — decrypting then
        // re-encrypting each packet when the caller negotiated SRTP.
        let (echo_crypto, echo_srtp) = match self.negotiate_srtp(&body) {
            Some((c, s)) => (Some(c), Some(s)),
            None => (None, None),
        };
        let rtp_port = match rtp::bind_echo(capture.clone(), echo_srtp).await {
            Ok((port, task)) => {
                if !call_id_hdr.is_empty() {
                    self.dialogs.lock().expect("dialogs mutex").insert(
                        call_id_hdr,
                        Dialog {
                            call_id,
                            media: Media::Echo(task),
                            callee: None,
                            capture: capture.clone(),
                            voicemail: None,
                            info_tx: None,
                        },
                    );
                } else {
                    task.abort();
                }
                port
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not bind RTP; answering without media");
                0
            }
        };

        // Answer with the caller's preferred codec — the echo path reflects it byte-for-byte.
        let sdp = self.build_sdp(rtp_port, &reflect, te_pt, echo_crypto.as_ref());
        let ok = self.build_invite_ok(msg, &sdp, call_id);
        tracing::info!(%call_id, rtp_port, codec = %reflect.name, srtp = echo_crypto.is_some(), "SIP INVITE answered (RTP echo)");
        self.send(socket, ok.as_bytes(), src).await
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
        socket: &UdpSocket,
        msg: &SipMessage,
        src: SocketAddr,
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
                return self.answer_with_echo(socket, msg, src, call_id, call_id_hdr).await;
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
                return self.answer_with_echo(socket, msg, src, call_id, call_id_hdr).await;
            }
        };
        let rtp_port = sock.local_addr().map(|a| a.port()).unwrap_or(0);

        let (info_tx, info_rx) = tokio::sync::mpsc::unbounded_channel::<char>();
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(Self::ivr_driver(
            sock,
            cfg,
            tenant,
            call_id,
            self.voicemails.clone(),
            self.registrations.clone(),
            self.media_ip,
            info_rx,
            stop_rx,
        ));

        if !call_id_hdr.is_empty() {
            self.dialogs.lock().expect("dialogs mutex").insert(
                call_id_hdr.to_string(),
                Dialog {
                    call_id,
                    media: Media::Ivr { task, stop: stop_tx },
                    callee: None,
                    capture: None,
                    voicemail: None,
                    info_tx: Some(info_tx),
                },
            );
        } else {
            let _ = stop_tx.send(true); // no dialog key to track it → stop the session
        }

        let audio = codec::Codec { pt: g711.payload_type(), name: g711.sdp_name().to_string(), clock: 8000 };
        let sdp = self.build_sdp(rtp_port, &audio, te_pt, None);
        let ok = self.build_invite_ok(msg, &sdp, call_id);
        tracing::info!(%call_id, %ivr_id, rtp_port, codec = %g711.sdp_name(), "SIP INVITE answered (IVR menu)");
        self.send(socket, ok.as_bytes(), src).await
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
                    if ivr_transfer(&sock, peer, &callee, media_ip, call_id, cfg.codec, cfg.te_pt, &mut stop_rx).await {
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

    /// Answer an INVITE with a plain RTP echo path (the IVR fallback). One UDP socket
    /// reflecting RTP back to the caller.
    async fn answer_with_echo(
        &self,
        socket: &UdpSocket,
        msg: &SipMessage,
        src: SocketAddr,
        call_id: Uuid,
        call_id_hdr: &str,
    ) -> std::io::Result<()> {
        let rtp_port = match rtp::bind_echo(None, None).await {
            Ok((port, task)) => {
                if !call_id_hdr.is_empty() {
                    self.dialogs.lock().expect("dialogs mutex").insert(
                        call_id_hdr.to_string(),
                        Dialog {
                            call_id,
                            media: Media::Echo(task),
                            callee: None,
                            capture: None,
                            voicemail: None,
                            info_tx: None,
                        },
                    );
                } else {
                    task.abort();
                }
                port
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not bind RTP; answering without media");
                0
            }
        };
        // Reflect the caller's preferred codec (the echo path relays bytes verbatim).
        let offer = codec::AudioMedia::parse(&String::from_utf8_lossy(msg.body()));
        let audio = offer.preferred_audio().unwrap_or_else(default_codec);
        let te_pt = offer.telephone_event_pt().unwrap_or(dtmf::TELEPHONE_EVENT_PT);
        let sdp = self.build_sdp(rtp_port, &audio, te_pt, None);
        let ok = self.build_invite_ok(msg, &sdp, call_id);
        self.send(socket, ok.as_bytes(), src).await
    }

    /// Best-effort outbound (UAC) INVITE to a registered callee, bridged to the caller.
    ///
    /// Binds a two-leg [`rtp::Bridge`], sends an INVITE offering leg B to the callee's
    /// contact over a **dedicated** UDP socket (so it never contends with the main ingress
    /// loop), waits (skipping 1xx) for a 2xx up to [`CALLEE_ANSWER_TIMEOUT`], ACKs it, and
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
    ) -> Option<(rtp::Bridge, CalleeLeg, codec::Codec, u8)> {
        let bridge = match rtp::bind_bridge(capture).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "could not bind RTP bridge");
                return None;
            }
        };

        let addr = match resolve_contact_addr(&callee.contact).await {
            Some(a) => a,
            None => {
                tracing::warn!(contact = %callee.contact, "callee contact is unresolvable");
                bridge.abort();
                return None;
            }
        };

        let sock = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "could not bind outbound SIP socket");
                bridge.abort();
                return None;
            }
        };

        // Outbound-leg dialog identifiers, derived from the CommOS Call id.
        let leg_call_id = format!("{}@commos", call_id.to_string().replace('-', ""));
        let from_tag: String = call_id.to_string().chars().filter(|c| *c != '-').take(16).collect();
        let from_hdr = format!("<sip:commos@{}>;tag={from_tag}", self.media_ip);
        let contact_hdr = format!("<sip:commos@{}>", self.media_ip);
        let cseq_num: u32 = 1;

        // Offer the caller's full codec list to the callee on leg B, so the two ends converge on
        // a shared codec CommOS relays untouched (transparent pass-through, no transcoding).
        let sdp = reoffer_sdp(self.media_ip, bridge.leg_b_port, offer);
        let invite = message::request(
            "INVITE",
            &callee.contact,
            &[
                ("From", from_hdr.clone()),
                ("To", format!("<{}>", callee.aor)),
                ("Call-ID", leg_call_id.clone()),
                ("CSeq", format!("{cseq_num} INVITE")),
                ("Contact", contact_hdr),
            ],
            Some(("application/sdp", &sdp)),
        );

        if let Err(e) = sock.send_to(invite.as_bytes(), addr).await {
            tracing::warn!(error = %e, %addr, "sending outbound INVITE failed");
            bridge.abort();
            return None;
        }

        // Wait for the callee's answer, ignoring provisional 1xx, until the timeout.
        let mut buf = vec![0u8; MAX_DATAGRAM];
        let answer = timeout(CALLEE_ANSWER_TIMEOUT, async {
            loop {
                let (n, _from) = sock.recv_from(&mut buf).await.ok()?;
                let resp = match message::parse(&buf[..n]) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                match resp.status() {
                    Some(s) if (100..200).contains(&s) => continue, // provisional; keep waiting
                    Some(s) if (200..300).contains(&s) => return Some(resp),
                    Some(_) => return None, // final non-2xx: callee rejected/failed
                    None => continue,       // stray request, not our response
                }
            }
        })
        .await;

        let resp = match answer {
            Ok(Some(r)) => r,
            _ => {
                bridge.abort();
                return None;
            }
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
        let callee_answer = codec::AudioMedia::parse(&String::from_utf8_lossy(resp.body()));
        let sel_codec = callee_answer
            .preferred_audio()
            .or_else(|| offer.preferred_audio())
            .unwrap_or_else(default_codec);
        let sel_te = callee_answer
            .telephone_event_pt()
            .or_else(|| offer.telephone_event_pt())
            .unwrap_or(dtmf::TELEPHONE_EVENT_PT);

        let leg = CalleeLeg {
            addr,
            request_uri: callee_target,
            from: from_hdr,
            to: callee_to,
            call_id: leg_call_id,
            cseq: cseq_num,
        };
        Some((bridge, leg, sel_codec, sel_te))
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
    ) -> Option<(rtp::Bridge, CalleeLeg, codec::Codec, u8)> {
        let gw_address = gateway.address.as_deref()?;
        let addr = match resolve_contact_addr(gw_address).await {
            Some(a) => a,
            None => {
                tracing::warn!(gateway = %gw_address, "outbound trunk: gateway address unresolvable");
                return None;
            }
        };
        let bridge = rtp::bind_bridge(capture).await.ok()?;
        let sock = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "outbound trunk: could not bind SIP socket");
                bridge.abort();
                return None;
            }
        };

        // Request-URI toward the carrier, and outbound-leg dialog identifiers.
        let request_uri = format!("sip:{e164}@{gw_address}");
        let leg_call_id = format!("{}@commos-trunk", call_id.to_string().replace('-', ""));
        let from_tag: String = call_id.to_string().chars().filter(|c| *c != '-').take(16).collect();
        let from_hdr = format!("<sip:commos@{}>;tag={from_tag}", self.media_ip);
        let contact_hdr = format!("<sip:commos@{}>", self.media_ip);
        let cnonce = from_tag.clone();
        // Offer the caller's codec list to the carrier (transparent pass-through).
        let sdp = reoffer_sdp(self.media_ip, bridge.leg_b_port, offer);
        let creds = self.trunk_credentials(gateway.carrier_id).await;

        // Send the INVITE, retrying once with digest auth if the carrier challenges.
        let mut buf = vec![0u8; MAX_DATAGRAM];
        let mut auth: Option<(&str, String)> = None;
        let mut resp = None;
        for cseq in 1u32..=2 {
            let mut headers = vec![
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
            if sock.send_to(invite.as_bytes(), addr).await.is_err() {
                bridge.abort();
                return None;
            }
            let answer = timeout(CALLEE_ANSWER_TIMEOUT, async {
                loop {
                    let (n, _from) = sock.recv_from(&mut buf).await.ok()?;
                    let m = match message::parse(&buf[..n]) {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    match m.status() {
                        Some(s) if (100..200).contains(&s) => continue,
                        Some(s) => return Some((s, m)),
                        None => continue,
                    }
                }
            })
            .await;
            let (status, msg) = match answer {
                Ok(Some(v)) => v,
                _ => {
                    bridge.abort();
                    return None;
                }
            };
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
                        bridge.abort();
                        return None;
                    }
                }
            }
            tracing::info!(gateway = %gw_address, status, "outbound trunk: carrier rejected the call");
            bridge.abort();
            return None;
        }
        let resp = match resp {
            Some(r) => r,
            None => {
                bridge.abort();
                return None;
            }
        };

        let callee_to = resp.header("To").map(str::to_string).unwrap_or_else(|| format!("<{request_uri}>"));
        let callee_target = resp.header("Contact").and_then(extract_uri).unwrap_or_else(|| request_uri.clone());
        let ack_cseq = if auth.is_some() { 2 } else { 1 };
        let mut ack_headers = vec![
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
        let carrier_answer = codec::AudioMedia::parse(&String::from_utf8_lossy(resp.body()));
        let sel_codec = carrier_answer
            .preferred_audio()
            .or_else(|| offer.preferred_audio())
            .unwrap_or_else(default_codec);
        let sel_te = carrier_answer
            .telephone_event_pt()
            .or_else(|| offer.telephone_event_pt())
            .unwrap_or(dtmf::TELEPHONE_EVENT_PT);
        tracing::info!(%call_id, gateway = %gw_address, %e164, leg_b_port = bridge.leg_b_port,
            codec = %sel_codec.name, "outbound trunk: call placed to carrier");

        let leg = CalleeLeg {
            addr,
            request_uri: callee_target,
            from: from_hdr,
            to: callee_to,
            call_id: leg_call_id,
            cseq: ack_cseq,
        };
        Some((bridge, leg, sel_codec, sel_te))
    }

    /// Best-effort mid-dialog BYE toward the callee leg of a bridged call, sent
    /// fire-and-forget over a throwaway socket.
    ///
    /// TODO(B2BUA): this reconstructs the BYE from captured identifiers only. Full RFC 3261
    /// mid-dialog correctness — route sets, contact refresh, a real client transaction and
    /// retransmission, waiting for the 200 — is not implemented; we send once and move on.
    async fn send_bye_to_callee(&self, callee: &CalleeLeg) {
        let bye = message::request(
            "BYE",
            &callee.request_uri,
            &[
                ("From", callee.from.clone()),
                ("To", callee.to.clone()),
                ("Call-ID", callee.call_id.clone()),
                ("CSeq", format!("{} BYE", callee.cseq + 1)),
            ],
            None,
        );
        match UdpSocket::bind("0.0.0.0:0").await {
            Ok(sock) => {
                if let Err(e) = sock.send_to(bye.as_bytes(), callee.addr).await {
                    tracing::debug!(error = %e, addr = %callee.addr, "sending BYE to callee failed");
                } else {
                    tracing::info!(addr = %callee.addr, "sent BYE to callee leg");
                }
            }
            Err(e) => tracing::debug!(error = %e, "could not bind socket for callee BYE"),
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
    async fn on_bye(
        &self,
        socket: &UdpSocket,
        msg: &SipMessage,
        src: SocketAddr,
    ) -> std::io::Result<()> {
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
                        // If the CALLER hung up, tear the callee leg down with a BYE. If the
                        // callee originated this BYE, it is already gone — do not echo one back.
                        if !from_callee {
                            self.send_bye_to_callee(callee).await;
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
        self.reply(socket, msg, 200, "OK", src).await
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
    fn negotiate_srtp(&self, body: &str) -> Option<(sdes::CryptoAttr, srtp::SrtpSession)> {
        if !self.srtp_enabled || !sdes::offers_savp(body) {
            return None;
        }
        let theirs = sdes::CryptoAttr::from_sdp(body)?;
        let (their_key, their_salt) = srtp::split_key_salt(&theirs.key_salt);
        let ours = srtp::random_key_salt();
        let (our_key, our_salt) = srtp::split_key_salt(&ours);
        let session = srtp::SrtpSession {
            inbound: srtp::SrtpContext::new(&their_key, &their_salt),
            outbound: srtp::SrtpContext::new(&our_key, &our_salt),
        };
        // Echo the caller's crypto tag so the peer correlates our answer with its offer.
        Some((sdes::CryptoAttr { tag: theirs.tag, key_salt: ours }, session))
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
        socket: &UdpSocket,
        msg: &SipMessage,
        status: u16,
        reason: &str,
        src: SocketAddr,
    ) -> std::io::Result<()> {
        let resp = message::response(msg, status, reason);
        self.send(socket, resp.as_bytes(), src).await
    }

    async fn send(&self, socket: &UdpSocket, bytes: &[u8], dst: SocketAddr) -> std::io::Result<()> {
        socket.send_to(bytes, dst).await?;
        Ok(())
    }
}

/// The user-part of a SIP URI: `sip:200@example.com` → `200`. Tolerates a leading `<` and
/// the `sip:`/`sips:`/`tel:` schemes. Returns `None` for a domain-only URI (no `@`).
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

/// A default codec (PCMU/8000) for when an offer carries no usable audio codec.
fn default_codec() -> codec::Codec {
    codec::Codec { pt: 0, name: "PCMU".to_string(), clock: 8000 }
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
/// when the caller advertised no audio codecs.
fn reoffer_sdp(media_ip: IpAddr, port: u16, offer: &codec::AudioMedia) -> String {
    let te = offer.telephone_event_pt().unwrap_or(dtmf::TELEPHONE_EVENT_PT);
    let (pts, rtpmaps) = offer.reoffer_lines();
    let (pts, rtpmaps) = if pts.trim().is_empty() {
        (format!("0 {te}"), format!("a=rtpmap:0 PCMU/8000\r\na=rtpmap:{te} telephone-event/8000\r\n"))
    } else {
        (pts, rtpmaps)
    };
    format!(
        "v=0\r\n\
         o=commos 0 0 IN IP4 {ip}\r\n\
         s=CommOS\r\n\
         c=IN IP4 {ip}\r\n\
         t=0 0\r\n\
         m=audio {port} RTP/AVP {pts}\r\n\
         {rtpmaps}\
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
    let from_hdr = format!("<sip:commos@{media_ip}>;tag={from_tag}");
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
    let ring_deadline = tokio::time::sleep(CALLEE_ANSWER_TIMEOUT);
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

/// Resolve a contact URI (`sip:200@192.168.1.5:5060`) to the socket address to send requests
/// to. Parses `host[:port]` (default port 5060), returning a literal IP directly and falling
/// back to async DNS for hostnames. Best-effort: returns `None` if nothing resolves.
async fn resolve_contact_addr(contact_uri: &str) -> Option<SocketAddr> {
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
}
