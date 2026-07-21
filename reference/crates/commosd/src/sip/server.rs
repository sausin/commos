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

use crate::control::registrations::{Registration, RegistrationRegistry};
use crate::control::routing::Routing;
use crate::media::MediaFact;

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

/// Per-INVITE state, keyed by the SIP `Call-ID`, so BYE/CANCEL can find the Call and its
/// media. For a bridged call, `callee` carries the second leg so a BYE tears down both sides.
struct Dialog {
    call_id: Uuid,
    media: Media,
    /// Present only for bridged (internal) calls; `None` for the echo path.
    callee: Option<CalleeLeg>,
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
}

impl SipServer {
    pub fn new(
        registrations: RegistrationRegistry,
        routing: Routing,
        media_ip: IpAddr,
        default_tenant: Uuid,
    ) -> Self {
        SipServer {
            registrations,
            routing,
            media_ip,
            default_tenant,
            dialogs: Arc::new(Mutex::new(HashMap::new())),
            bye_aliases: Arc::new(Mutex::new(HashMap::new())),
        }
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
        self.send(socket, resp.as_bytes(), src).await
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

        // If the request-URI names a REGISTERED endpoint, try to bridge the two legs: bind a
        // two-leg RTP relay, INVITE the callee (offering leg B), and on its 200 OK answer the
        // caller (offering leg A). Any failure (no registered match, callee did not answer)
        // falls through to the echo path so the dial still completes.
        let request_uri = msg.request_uri().unwrap_or("");
        if let Some(callee_reg) = self.find_registered_callee(request_uri) {
            match self.try_bridge(&callee_reg, call_id).await {
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
                None => {
                    tracing::warn!(%call_id, callee = %callee_reg.contact,
                        "registered callee did not answer within timeout; falling back to echo");
                }
            }
        }

        // Echo path: non-registered destination (PSTN-style / +E.164), or the callee did not
        // answer. One UDP socket reflecting RTP back to the caller.
        let rtp_port = match rtp::bind_echo().await {
            Ok((port, task)) => {
                if !call_id_hdr.is_empty() {
                    self.dialogs.lock().expect("dialogs mutex").insert(
                        call_id_hdr,
                        Dialog {
                            call_id,
                            media: Media::Echo(task),
                            callee: None,
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
    ) -> Option<(rtp::Bridge, CalleeLeg)> {
        let bridge = match rtp::bind_bridge().await {
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
