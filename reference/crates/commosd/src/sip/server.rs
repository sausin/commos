//! The UDP SIP signalling ingress (Volume 7) — the front door a real softphone talks to.
//!
//! It binds a [`UdpSocket`], parses each datagram with [`super::message`], and dispatches by
//! method. REGISTER is fully handled: the AoR→contact binding is driven into the
//! [`RegistrationRegistry`], so a phone that sends REGISTER actually appears in the platform
//! (`GET /registrations`). OPTIONS, BYE and CANCEL get a `200 OK`; INVITE gets `100 Trying`
//! then `200 OK` with media negotiation still to come (see the TODO in [`Self::handle`]);
//! ACK, per SIP, gets no response.
//!
//! Robustness is a hard requirement: a malformed datagram is logged at debug and dropped —
//! it must never break the receive loop.

use std::net::SocketAddr;

use tokio::net::UdpSocket;

use commos_core::common::Uuid;

use crate::control::registrations::RegistrationRegistry;

use super::message::{self, SipMessage};

/// Largest UDP SIP datagram we accept. RFC 3261 keeps unreliable-transport messages well
/// under the path MTU; 64 KiB is the UDP ceiling and leaves ample headroom for INVITE+SDP.
const MAX_DATAGRAM: usize = 65_535;

/// The UDP SIP server. Cheap to construct; [`Self::run`] takes ownership and drives the
/// receive loop until the socket errors fatally.
pub struct SipServer {
    registrations: RegistrationRegistry,
    /// The tenant every registration on this ingress is attributed to.
    ///
    /// **Single-tenant simplification.** The SIP plane here binds one UDP port to one
    /// tenant. A real deployment maps the inbound SIP domain / trunk (or an authenticated
    /// identity) to a tenant — multi-tenant SIP routing is Volume 9 work. Until then every
    /// REGISTER on this socket lands in `default_tenant`.
    default_tenant: Uuid,
}

impl SipServer {
    /// Create a server bound (logically) to one tenant, sharing the hub's registration
    /// registry so registrations are visible through the control plane / API.
    pub fn new(registrations: RegistrationRegistry, default_tenant: Uuid) -> Self {
        SipServer {
            registrations,
            default_tenant,
        }
    }

    /// Bind `bind` and serve SIP over UDP forever. Returns only on a fatal socket error;
    /// per-datagram errors are contained and logged.
    pub async fn run(self, bind: SocketAddr) -> std::io::Result<()> {
        let socket = UdpSocket::bind(bind).await?;
        let local = socket.local_addr().unwrap_or(bind);
        tracing::info!(addr = %local, "SIP signalling ingress listening (UDP)");

        let mut buf = vec![0u8; MAX_DATAGRAM];
        loop {
            let (len, src) = match socket.recv_from(&mut buf).await {
                Ok(v) => v,
                Err(e) => {
                    // A recv error is transient on UDP (e.g. ICMP port-unreachable surfaced
                    // as an error on some platforms); log and keep serving.
                    tracing::debug!(error = %e, "SIP recv_from error; continuing");
                    continue;
                }
            };

            // Isolate handling so a panic-free error path never breaks the loop.
            if let Err(e) = self.handle(&socket, &buf[..len], src).await {
                tracing::debug!(error = %e, %src, "dropping SIP datagram");
            }
        }
    }

    /// Parse and dispatch a single datagram. Any parse failure or send error is returned so
    /// the caller can log-and-drop it.
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

        // Responses arriving at an ingress socket are not something we act on here.
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
                // ACK is hop-by-hop and never answered (RFC 3261 §17). Nothing to send.
                tracing::info!(method = %method, %src, "SIP ACK");
                Ok(())
            }
            "BYE" | "CANCEL" => {
                tracing::info!(method = %method, %src, "SIP {method}");
                self.reply(socket, &msg, 200, "OK", src).await
            }
            other => {
                tracing::info!(method = %other, %src, "SIP method not implemented");
                self.reply(socket, &msg, 501, "Not Implemented", src).await
            }
        }
    }

    /// Handle REGISTER: bind the AoR to its contact in the registry and confirm with a
    /// `200 OK` that echoes the accepted Contact and Expires (RFC 3261 §10.3).
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
        // Fall back to the transport source if the phone sent no Contact (or a wildcard),
        // so the binding still points somewhere reachable.
        let contact = msg
            .contact_uri()
            .unwrap_or_else(|| format!("sip:{}", src));
        let user_agent = msg.user_agent().map(str::to_string);

        // Drive the shared registry. Expires == 0 is a de-registration: register with a
        // zero lifetime (immediate expiry) as a best-effort unbind, then still 200 — a full
        // remove-by-AoR path is a later refinement, but the binding lapses either way.
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
            tracing::info!(
                method = "REGISTER",
                %aor,
                contact = %contact,
                expires,
                registration_id = %reg.id,
                user_agent = user_agent.as_deref().unwrap_or("-"),
                "SIP REGISTER"
            );
        }

        // Echo the Contact (with the granted expiry) and the Expires header the client asked
        // for, so the phone knows its binding was accepted and when to refresh.
        let contact_header = format!("<{contact}>;expires={expires}");
        let extra = [
            ("Contact", contact_header),
            ("Expires", expires.to_string()),
        ];
        let resp = message::response_with(msg, 200, "OK", &extra);
        self.send(socket, resp.as_bytes(), src).await
    }

    /// Handle INVITE: provisionally accept with `100 Trying`, then `200 OK`.
    async fn on_invite(
        &self,
        socket: &UdpSocket,
        msg: &SipMessage,
        src: SocketAddr,
    ) -> std::io::Result<()> {
        tracing::info!(
            method = "INVITE",
            request_uri = msg.request_uri().unwrap_or("-"),
            %src,
            "SIP INVITE"
        );

        // Provisional: tell the caller we got it and are working on it (RFC 3261 §21.1.2).
        let trying = message::response(msg, 100, "Trying");
        self.send(socket, trying.as_bytes(), src).await?;

        // TODO(Volume 7 media): a real INVITE must negotiate SDP against the MediaPlane
        // boundary and create a Call in the control plane (the hub wires routing→media).
        // Here we only ack signalling so a softphone's dial attempt completes at the SIP
        // layer; no RTP is set up and no Call entity is created yet. Answering 200 without
        // an SDP answer body is intentionally incomplete and will be replaced when the
        // media plane is connected.
        let ok = message::response(msg, 200, "OK");
        self.send(socket, ok.as_bytes(), src).await
    }

    /// Build a plain (bodyless) response for `msg` and send it.
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
