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

use commos_core::common::{Timestamp, Uuid};

use crate::control::registrations::RegistrationRegistry;
use crate::control::routing::Routing;
use crate::media::MediaFact;

use super::message::{self, SipMessage};
use super::rtp;

/// Largest UDP SIP datagram we accept (the UDP ceiling; ample for INVITE+SDP).
const MAX_DATAGRAM: usize = 65_535;

/// Per-INVITE state, keyed by the SIP `Call-ID`, so BYE/CANCEL can find the Call and its RTP.
struct Dialog {
    call_id: Uuid,
    rtp: JoinHandle<()>,
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
    /// Active dialogs by SIP `Call-ID`.
    dialogs: Arc<Mutex<HashMap<String, Dialog>>>,
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

        // Set up the RTP echo path and remember the dialog for BYE.
        let rtp_port = match rtp::bind_echo().await {
            Ok((port, task)) => {
                if !call_id_hdr.is_empty() {
                    self.dialogs
                        .lock()
                        .expect("dialogs mutex")
                        .insert(call_id_hdr, Dialog { call_id, rtp: task });
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

    /// BYE/CANCEL: hang the Call up (produces the CDR), abort its RTP, and `200 OK`.
    async fn on_bye(
        &self,
        socket: &UdpSocket,
        msg: &SipMessage,
        src: SocketAddr,
    ) -> std::io::Result<()> {
        let method = msg.method().unwrap_or("BYE").to_string();
        if let Some(call_id_hdr) = msg.call_id() {
            let dialog = self.dialogs.lock().expect("dialogs mutex").remove(call_id_hdr);
            if let Some(d) = dialog {
                d.rtp.abort();
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
