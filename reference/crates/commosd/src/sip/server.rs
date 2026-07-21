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
use super::rtp;

/// Largest UDP SIP datagram we accept (the UDP ceiling; ample for INVITE+SDP).
const MAX_DATAGRAM: usize = 65_535;

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
}

impl Media {
    /// Tear the media plane down (abort the relay/echo task).
    fn abort(self) {
        match self {
            Media::Echo(task) => task.abort(),
            Media::Bridge(bridge) => bridge.abort(),
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
            other => {
                tracing::info!(method = %other, %src, "SIP method not implemented");
                self.reply(socket, &msg, 501, "Not Implemented", src).await
            }
        }
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

        // If the (routed) request-URI names a REGISTERED endpoint, try to bridge the two legs:
        // bind a two-leg RTP relay, INVITE the callee (offering leg B), and on its 200 OK
        // answer the caller (offering leg A). A callee that never answers (or an internal
        // extension that is offline) diverts to voicemail; a non-mailbox destination
        // (PSTN-style / +E.164) falls through to the echo path so the dial still completes.
        let mut voicemail_target: Option<VoicemailBox> = None;
        if let Some(callee_reg) = self.find_registered_callee(effective_uri) {
            match self.try_bridge(&callee_reg, call_id, capture.clone()).await {
                Some((bridge, callee_leg)) => {
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
                            },
                        );
                        // Index the callee-leg Call-ID so a callee-side BYE finds this dialog.
                        self.bye_aliases
                            .lock()
                            .expect("aliases mutex")
                            .insert(callee_call_id, call_id_hdr);
                    } else {
                        bridge.abort();
                    }
                    let sdp = self.build_sdp(leg_a_port);
                    let ok = self.build_invite_ok(msg, &sdp, call_id);
                    tracing::info!(%call_id, leg_a_port, callee = %callee_reg.contact,
                        "SIP INVITE bridged to registered callee");
                    return self.send(socket, ok.as_bytes(), src).await;
                }
                None if self.voicemail_enabled => {
                    // Rang but never answered → take a voicemail. MWI is pushed to the callee's
                    // registered contact on hangup.
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
        } else if self.voicemail_enabled && routed_uri.is_some() {
            // The dialled number is an internal extension (it resolves to a SIP endpoint) but
            // no device is registered for it — the mailbox owner is offline. Take a voicemail;
            // its MWI is delivered on the phone's next REGISTER.
            tracing::info!(%call_id, mailbox = %effective_uri,
                "internal extension is offline; diverting to voicemail");
            voicemail_target = Some(VoicemailBox { aor: effective_uri.to_string(), notify: None });
        }

        // Voicemail path: answer the caller and capture their audio — stored as a Voicemail on
        // hangup — regardless of `record_calls` (a voicemail is always captured). Greeting/beep
        // prompt playback is future work (it ties into the IVR prompt runtime); for now the
        // caller is connected to a capturing echo, exactly as the recording path is.
        if let Some(vmbox) = voicemail_target {
            let vm_capture: rtp::Capture = Arc::new(Mutex::new(Vec::new()));
            let rtp_port = match rtp::bind_echo(Some(vm_capture.clone())).await {
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
            let sdp = self.build_sdp(rtp_port);
            let ok = self.build_invite_ok(msg, &sdp, call_id);
            tracing::info!(%call_id, rtp_port, "SIP INVITE answered (voicemail)");
            return self.send(socket, ok.as_bytes(), src).await;
        }

        // Echo path: non-mailbox destination (PSTN-style / +E.164), or a no-answer with
        // voicemail disabled. One UDP socket reflecting RTP back to the caller.
        let rtp_port = match rtp::bind_echo(capture.clone()).await {
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

        // Answer with SDP.
        let sdp = self.build_sdp(rtp_port);
        let ok = self.build_invite_ok(msg, &sdp, call_id);
        tracing::info!(%call_id, rtp_port, "SIP INVITE answered (RTP echo)");
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

    /// Find a registered callee whose AoR user-part matches the request-URI's user-part
    /// (e.g. request-URI `sip:200@example.com` ↔ registration AoR `sip:200@host`). Returns
    /// `None` for a domain-only URI or an unregistered destination (external `+E.164`).
    fn find_registered_callee(&self, request_uri: &str) -> Option<Registration> {
        let want = user_part(request_uri)?;
        self.registrations
            .list(self.default_tenant)
            .into_iter()
            .find(|r| user_part(&r.aor).is_some_and(|u| u.eq_ignore_ascii_case(want)))
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
    ) -> Option<(rtp::Bridge, CalleeLeg)> {
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

        // Offer leg B to the callee in SDP.
        let sdp = self.build_sdp(bridge.leg_b_port);
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

        let leg = CalleeLeg {
            addr,
            request_uri: callee_target,
            from: from_hdr,
            to: callee_to,
            call_id: leg_call_id,
            cseq: cseq_num,
        };
        Some((bridge, leg))
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

    /// Build the SDP answer advertising the RTP echo port (PCMU/8000, sendrecv).
    fn build_sdp(&self, rtp_port: u16) -> String {
        format!(
            "v=0\r\n\
             o=commos 0 0 IN IP4 {ip}\r\n\
             s=CommOS\r\n\
             c=IN IP4 {ip}\r\n\
             t=0 0\r\n\
             m=audio {port} RTP/AVP 0\r\n\
             a=rtpmap:0 PCMU/8000\r\n\
             a=sendrecv\r\n",
            ip = self.media_ip,
            port = rtp_port
        )
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
