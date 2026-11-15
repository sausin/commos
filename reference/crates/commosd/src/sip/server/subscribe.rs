//! SUBSCRIBE / NOTIFY glue (RFC 6665) for BLF lamps: `Event: dialog` (RFC 4235),
//! `message-summary` (RFC 3842) and a basic `presence` (PIDF).
//!
//! The subscription table, call-dialog tracking and XML rendering live in
//! [`crate::control::blf`]; this file owns the SIP side: authenticating the request, answering
//! `200 OK`, and emitting in-dialog NOTIFYs (correct From/To tags, Call-ID, incrementing CSeq,
//! Contact, a reachable Via) over the transport the SUBSCRIBE arrived on. Call-state pushes are
//! driven by the event bus ([`SipServer::run_blf`]), so the bridge/INVITE paths are untouched.

use std::time::Instant;


use crate::bus::EventBus;
use crate::control::blf::{self, BlfError, EventKind, SubKey, Subscription};

use super::*;

/// Event packages we accept, advertised in `Allow-Events` and on a `489`.
pub(super) const ALLOW_EVENTS: &str = "dialog, message-summary, presence";
/// Methods we advertise in `Allow`.
pub(super) const ALLOW_METHODS: &str =
    "INVITE, ACK, CANCEL, BYE, OPTIONS, REGISTER, INFO, SUBSCRIBE, NOTIFY, UPDATE, REFER";

/// How often expired subscriptions are swept.
const SWEEP_INTERVAL: Duration = Duration::from_secs(10);

/// Local port of the ingress socket a request arrived on (for Contact/Via); 5060 for streams.
fn local_port(sink: &Responder) -> u16 {
    match sink {
        Responder::Udp { socket, .. } => socket.local_addr().map(|a| a.port()).unwrap_or(5060),
        _ => 5060,
    }
}

/// Host part of a SIP URI (`sip:200@h:5060` → `h:5060`).
fn host_of(uri: &str) -> &str {
    let s = uri.trim().trim_start_matches("sips:").trim_start_matches("sip:");
    s.rsplit_once('@').map(|(_, h)| h).unwrap_or(s)
}

impl SipServer {
    fn blf_contact(&self, sink: &Responder) -> String {
        format!("<sip:commos@{}:{}>", self.media_ip, local_port(sink))
    }

    /// Does `ext` exist — registered right now, or configured as an Extension?
    async fn extension_known(&self, ext: &str) -> bool {
        self.is_registered(ext) || self.routing.resolve_extension(self.default_tenant, ext).await.is_some()
    }

    fn is_registered(&self, ext: &str) -> bool {
        self.registrations
            .list(self.default_tenant)
            .iter()
            .any(|r| blf::ext_of(&r.aor).as_deref() == Some(ext))
    }

    /// The NOTIFY body for `sub` at dialog-info `version`.
    async fn blf_body(&self, sub: &Subscription, version: u32) -> String {
        match sub.event {
            EventKind::Dialog => blf::render_dialog_info(
                version,
                &sub.entity,
                host_of(&sub.entity),
                &self.blf.dialogs_for(&sub.target),
            ),
            EventKind::Presence => blf::render_pidf(&sub.entity, self.is_registered(&sub.target)),
            EventKind::MessageSummary => {
                let (new, old) = self
                    .voicemails
                    .mailbox_summary(self.default_tenant, &sub.target)
                    .await
                    .unwrap_or((0, 0));
                blf::render_message_summary(&sub.entity, new, old)
            }
        }
    }

    /// Send one in-dialog NOTIFY. Returns `false` if the transport is gone.
    async fn send_blf_notify(
        &self,
        sub: &Subscription,
        cseq: u32,
        state: &str,
        body: Option<&str>,
    ) -> bool {
        let sent_by = SocketAddr::new(self.media_ip, local_port(&sub.sink));
        let notify = message::request(
            "NOTIFY",
            &sub.remote_uri,
            &[
                ("Via", message::via_header(sent_by)),
                ("From", sub.local_hdr.clone()),
                ("To", sub.remote_hdr.clone()),
                ("Call-ID", sub.key.call_id.clone()),
                ("CSeq", format!("{cseq} NOTIFY")),
                ("Contact", self.blf_contact(&sub.sink)),
                ("Event", sub.event.name().to_string()),
                ("Subscription-State", state.to_string()),
            ],
            body.map(|b| (sub.event.content_type(), b)),
        );
        sub.sink.send(notify.as_bytes()).await.is_ok()
    }

    /// NOTIFY the current full state to one subscription (advances version/CSeq).
    async fn notify_one(&self, key: &SubKey) {
        let Some(t) = self.blf.prepare_notify(key, Instant::now()) else {
            return;
        };
        let body = self.blf_body(&t.sub, t.version).await;
        let state = format!("active;expires={}", t.remaining_secs);
        if !self.send_blf_notify(&t.sub, t.cseq, &state, Some(&body)).await {
            // The connection that carried the SUBSCRIBE is gone; drop the subscription.
            self.blf.remove(key);
        }
    }

    /// NOTIFY every `dialog` watcher of `ext`.
    async fn notify_extension(&self, ext: &str) {
        for key in self.blf.watchers_of(ext, EventKind::Dialog) {
            self.notify_one(&key).await;
        }
    }

    /// Fold one bus event into the call tracker and push NOTIFYs to the affected lamps.
    pub(crate) async fn on_blf_event(&self, event: &serde_json::Value) {
        for ext in self.blf.on_event(event) {
            self.notify_extension(&ext).await;
        }
    }

    /// Terminate and NOTIFY every subscription whose lifetime has elapsed.
    pub(crate) async fn sweep_blf(&self) {
        for s in self.blf.sweep(Instant::now()) {
            let body = (s.event == EventKind::Dialog).then(|| self.dialog_final_body(&s));
            self.send_blf_notify(&s, s.cseq + 1, "terminated;reason=timeout", body.as_deref())
                .await;
        }
    }

    fn dialog_final_body(&self, s: &Subscription) -> String {
        blf::render_dialog_info(s.version + 1, &s.entity, host_of(&s.entity), &[])
    }

    /// Bus-driven BLF task: tracks call state from the event stream, pushes NOTIFYs to
    /// watchers, and sweeps expired subscriptions. Spawned once from `main`.
    pub async fn run_blf(self: Arc<Self>, bus: EventBus) {
        let mut rx = bus.subscribe();
        let mut tick = tokio::time::interval(SWEEP_INTERVAL);
        loop {
            tokio::select! {
                _ = tick.tick() => self.sweep_blf().await,
                recv = rx.recv() => match recv {
                    Ok(ev) => self.on_blf_event(&ev).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "BLF listener lagged; lamps may be stale");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    /// SUBSCRIBE: create / refresh / terminate a subscription.
    pub(super) async fn on_subscribe(
        &self,
        resp: &Responder,
        msg: &SipMessage,
    ) -> std::io::Result<()> {
        let src = resp.peer();
        let Some(event) = msg.header("Event").and_then(EventKind::parse) else {
            let r = message::response_with(
                msg,
                489,
                "Bad Event",
                &[("Allow-Events", ALLOW_EVENTS.to_string())],
            );
            return resp.send(r.as_bytes()).await;
        };

        if self.auth_required_from(src) && self.digest_ok(msg, "SUBSCRIBE").await.is_none() {
            tracing::info!(%src, "SIP SUBSCRIBE challenged (digest auth required)");
            return self.send_challenge(resp, msg).await;
        }

        let (Some(call_id), Some(from_tag), Some(from_hdr), Some(to_hdr)) = (
            msg.call_id(),
            msg.from_tag(),
            msg.header("From"),
            msg.header("To"),
        ) else {
            return self.reply(resp, msg, 400, "Bad Request").await;
        };
        let key = SubKey { call_id: call_id.to_string(), remote_tag: from_tag };
        let expires = blf::clamp_expires(msg.expires());
        let ok_headers = |n: u64| {
            [
                ("Expires", n.to_string()),
                ("Contact", self.blf_contact(resp)),
                ("Allow-Events", ALLOW_EVENTS.to_string()),
            ]
        };

        // In-dialog refresh / unsubscribe.
        if msg.to_tag().is_some() {
            let Some(existing) = self.blf.get(&key) else {
                return self.reply(resp, msg, 481, "Subscription Does Not Exist").await;
            };
            resp.send(message::response_with(msg, 200, "OK", &ok_headers(expires)).as_bytes())
                .await?;
            if expires == 0 {
                self.blf.remove(&key);
                let cseq = existing.cseq + 1;
                let body = (existing.event == EventKind::Dialog)
                    .then(|| self.dialog_final_body(&existing));
                self.send_blf_notify(&existing, cseq, "terminated;reason=timeout", body.as_deref())
                    .await;
            } else {
                self.blf.refresh(&key, expires, Instant::now());
                self.notify_one(&key).await;
            }
            return Ok(());
        }

        // New subscription: resolve the watched extension.
        let Some(target) = msg
            .request_uri()
            .and_then(blf::ext_of)
            .or_else(|| msg.to_aor().as_deref().and_then(blf::ext_of))
        else {
            return self.reply(resp, msg, 404, "Not Found").await;
        };
        if !self.extension_known(&target).await {
            tracing::info!(%src, %target, "SIP SUBSCRIBE for unknown extension");
            return self.reply(resp, msg, 404, "Not Found").await;
        }

        let reply = message::response_with(msg, 200, "OK", &ok_headers(expires));
        if expires == 0 {
            return resp.send(reply.as_bytes()).await;
        }
        // Our dialog tag is whatever `response_with` added to the To header of the 200.
        let Some(local_tag) = message::parse(reply.as_bytes()).ok().and_then(|m| m.to_tag()) else {
            return self.reply(resp, msg, 500, "Server Internal Error").await;
        };
        let from_aor = msg.from_aor().unwrap_or_default();
        let sub = Subscription {
            key: key.clone(),
            event,
            target: target.clone(),
            watcher: blf::ext_of(&from_aor).unwrap_or_else(|| src.ip().to_string()),
            local_hdr: format!("{to_hdr};tag={local_tag}"),
            remote_hdr: from_hdr.to_string(),
            remote_uri: msg.contact_uri().unwrap_or_else(|| from_aor.clone()),
            entity: msg.to_aor().unwrap_or_else(|| format!("sip:{target}@{}", self.media_ip)),
            sink: resp.clone(),
            expires_at: Instant::now() + Duration::from_secs(expires),
            version: 0,
            cseq: 0,
        };
        if let Err(BlfError::TooManySubscriptions) = self.blf.subscribe(sub) {
            tracing::warn!(%src, %target, "SIP SUBSCRIBE rejected: subscription cap reached");
            return self.reply(resp, msg, 503, "Service Unavailable").await;
        }
        tracing::info!(%src, %target, event = event.name(), expires, "SIP SUBSCRIBE");
        resp.send(reply.as_bytes()).await?;
        self.notify_one(&key).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::ivr::IvrService;
    use crate::control::objects::ObjectService;
    use crate::control::policy::PolicyLimits;
    use crate::control::recordings::RecordingService;
    use crate::media::LoopbackMedia;
    use crate::objectstore::{LocalObjectStore, ObjectStore};
    use crate::relay::RelaySignal;
    use crate::store::MemStore;

    fn test_server() -> Arc<SipServer> {
        let store: Arc<dyn Store> = Arc::new(MemStore::new());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let signal = RelaySignal::new();
        let routing = Routing::new(
            store.clone(),
            Arc::new(LoopbackMedia::new(tx)),
            signal.clone(),
            PolicyLimits::default(),
            "1",
        );
        let root = std::env::temp_dir().join(format!("commos-blf-{}", Uuid::now_v7()));
        let blob: Arc<dyn ObjectStore> = Arc::new(LocalObjectStore::new(root));
        let objects = ObjectService::new(blob, store.clone(), signal.clone());
        let regs = RegistrationRegistry::new();
        let tenant = Uuid::now_v7();
        regs.register(tenant, "sip:200@127.0.0.1".into(), "sip:200@127.0.0.1:5999".into(), None, 600);
        Arc::new(SipServer::new(
            regs,
            routing,
            "127.0.0.1".parse().unwrap(),
            tenant,
            store.clone(),
            false,
            "commos",
            false,
            RecordingService::new(objects.clone(), store.clone(), signal.clone()),
            false,
            OnDecline::default(),
            5,
            VoicemailService::new(objects.clone(), store.clone(), signal.clone()),
            IvrService::new(store.clone(), signal.clone()),
            objects,
            "1",
            false,
            false,
            "/nonexistent",
            "/nonexistent/display_name.txt",
            Arc::new(crate::sip::moh::MohSource::synth()),
            false,
        ))
    }

    fn subscribe_msg(call_id: &str, target: &str, event: &str, expires: u64, to_tag: Option<&str>) -> String {
        let to = to_tag.map(|t| format!(";tag={t}")).unwrap_or_default();
        format!(
            "SUBSCRIBE sip:{target}@127.0.0.1 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5999;branch=z9hG4bKabc\r\n\
             From: <sip:100@127.0.0.1>;tag=ft1\r\n\
             To: <sip:{target}@127.0.0.1>{to}\r\n\
             Call-ID: {call_id}\r\nCSeq: 1 SUBSCRIBE\r\n\
             Contact: <sip:100@127.0.0.1:5999>\r\nEvent: {event}\r\nExpires: {expires}\r\n\
             Content-Length: 0\r\n\r\n"
        )
    }

    async fn recv(sock: &UdpSocket) -> SipMessage {
        let mut buf = vec![0u8; 4096];
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), sock.recv_from(&mut buf))
            .await
            .expect("datagram expected")
            .unwrap();
        message::parse(&buf[..n]).unwrap()
    }

    #[tokio::test]
    async fn subscribe_dialog_gets_200_notify_and_call_pushes() {
        let server = test_server();
        let srv_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let srv_addr = srv_sock.local_addr().unwrap();
        let s2 = server.clone();
        let ss = srv_sock.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            while let Ok((n, src)) = ss.recv_from(&mut buf).await {
                let r = Responder::Udp { socket: ss.clone(), dst: src };
                let d = buf[..n].to_vec();
                let s = s2.clone();
                tokio::spawn(async move {
                    let _ = s.handle(&r, &d).await;
                });
            }
        });
        let phone = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        phone.send_to(subscribe_msg("sub1", "200", "dialog", 300, None).as_bytes(), srv_addr).await.unwrap();
        let ok = recv(&phone).await;
        assert_eq!(ok.status(), Some(200));
        assert_eq!(ok.header("Expires"), Some("300"));
        let tag = ok.to_tag().expect("To tag on 200");
        let n1 = recv(&phone).await;
        assert_eq!(n1.method(), Some("NOTIFY"));
        assert_eq!(n1.header("Event"), Some("dialog"));
        assert!(n1.header("Subscription-State").unwrap().starts_with("active;expires="));
        assert_eq!(n1.header("Call-ID"), Some("sub1"));
        assert_eq!(n1.to_tag().as_deref(), Some("ft1"));
        assert_eq!(n1.from_tag().as_deref(), Some(tag.as_str()));
        assert_eq!(n1.cseq_parts(), Some((1, "NOTIFY")));
        assert!(n1.header("Via").unwrap().contains(";rport;branch=z9hG4bK"));
        let body = String::from_utf8_lossy(n1.body()).to_string();
        assert!(body.contains("state=\"full\"") && !body.contains("<dialog "), "{body}");

        // A call to 200 starts ringing → early/recipient; answered → confirmed; ended → idle.
        let ev = |ty: &str| {
            serde_json::json!({"type": ty, "data": {"call_id": "c-1",
                "from_ref": "sip:100@127.0.0.1", "to_ref": "sip:200@127.0.0.1"}})
        };
        server.on_blf_event(&ev("CallStarted")).await;
        let n = recv(&phone).await;
        assert!(String::from_utf8_lossy(n.body()).contains("<state>trying</state>"));
        server.on_blf_event(&ev("CallRinging")).await;
        let n = recv(&phone).await;
        let b = String::from_utf8_lossy(n.body()).to_string();
        assert!(b.contains("<state>early</state>") && b.contains("direction=\"recipient\""), "{b}");
        assert_eq!(n.cseq_parts(), Some((3, "NOTIFY")));
        server.on_blf_event(&ev("CallAnswered")).await;
        assert!(String::from_utf8_lossy(recv(&phone).await.body()).contains("<state>confirmed</state>"));
        server.on_blf_event(&ev("CallEnded")).await;
        assert!(!String::from_utf8_lossy(recv(&phone).await.body()).contains("<dialog "));

        // In-dialog unsubscribe → 200 + terminated NOTIFY.
        phone
            .send_to(subscribe_msg("sub1", "200", "dialog", 0, Some(&tag)).as_bytes(), srv_addr)
            .await
            .unwrap();
        assert_eq!(recv(&phone).await.status(), Some(200));
        let fin = recv(&phone).await;
        assert!(fin.header("Subscription-State").unwrap().starts_with("terminated"));
        assert_eq!(server.blf.count(), 0);

        // Unknown extension → 404; unknown event → 489 with Allow-Events.
        phone.send_to(subscribe_msg("s2", "999", "dialog", 300, None).as_bytes(), srv_addr).await.unwrap();
        assert_eq!(recv(&phone).await.status(), Some(404));
        phone.send_to(subscribe_msg("s3", "200", "refer", 300, None).as_bytes(), srv_addr).await.unwrap();
        let r = recv(&phone).await;
        assert_eq!(r.status(), Some(489));
        assert!(r.header("Allow-Events").unwrap().contains("dialog"));
    }
}
