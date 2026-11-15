//! SIP `REFER` call transfer (RFC 3515, with RFC 3891 `Replaces` for attended transfer).
//!
//! A phone's Transfer key sends an in-dialog `REFER` with a `Refer-To` target. CommOS is a B2BUA,
//! so "transfer" is a media/dialog re-targeting rather than a signalling redirect:
//!
//! * the REFER is acknowledged `202 Accepted`, then progress is reported on the implicit
//!   subscription with `NOTIFY` (`Event: refer`, `message/sipfrag`): `100 Trying`, then the final
//!   status of the transfer attempt;
//! * **blind**: CommOS places a *new* outbound call to the target with the existing
//!   [`try_bridge`](SipServer::try_bridge) / [`try_trunk`](SipServer::try_trunk) primitives (so
//!   registration lookup, trunk selection + auth and SRTP all behave exactly like a normal call),
//!   then re-points the *transferor's* RTP leg of the live relay at that new call's relay and
//!   `BYE`s the transferor. The transferee never re-negotiates media — its leg of the relay is
//!   untouched, which is why this works with phones that ignore re-INVITEs;
//! * **attended** (`Refer-To` carries `?Replaces=`): the transferor's *other* bridged call is
//!   located by Call-ID (+ tags), its two relays are cross-linked so the two remote parties hear
//!   each other, and both of the transferor's legs are `BYE`d;
//! * on failure the original call is left alone (the transferee may be on hold; the transferor can
//!   resume) and the final `NOTIFY` carries the error status.
//!
//! The dialog bookkeeping afterwards is the same as for a normal bridged call (`caller` = the leg
//! in the primary key = relay leg A, `callee` = the other), so BYE/hold handling keeps working on
//! the new pair.

use std::net::{IpAddr, Ipv4Addr};

use commos_core::events::call_transferred::TransferType;

use super::*;
use crate::sip::message::{ReferTo, Replaces};

/// A transfer failure: the SIP status + reason reported in the final `NOTIFY` sipfrag.
type Fail = (u16, &'static str);

/// The two parties of a live bridged dialog, from the point of view of the REFER sender.
struct Parties {
    /// Key of the dialog in `dialogs` (the caller-leg Call-ID).
    primary: String,
    /// The leg that sent the REFER (the party that will be dropped).
    transferor: CalleeLeg,
    /// The other leg — it stays up and is re-bridged to the target.
    transferee: CalleeLeg,
    /// Whether the transferor is the dialog's caller (relay leg A) or callee (relay leg B).
    transferor_is_caller: bool,
    call_id: Uuid,
    answer_sdp: String,
}

/// The relay leg a party occupies.
fn leg_of(is_caller: bool) -> rtp::Leg {
    if is_caller {
        rtp::Leg::A
    } else {
        rtp::Leg::B
    }
}

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

fn sock_port(s: &UdpSocket) -> u16 {
    s.local_addr().map(|a| a.port()).unwrap_or(0)
}

impl SipServer {
    /// REFER: transfer the call (blind, or attended when `Refer-To` carries `Replaces`).
    pub(super) async fn on_refer(&self, resp: &Responder, msg: &SipMessage) -> std::io::Result<()> {
        let Some(cid) = msg.call_id() else {
            return self.reply(resp, msg, 400, "Bad Request").await;
        };
        let Some(parties) = self.refer_parties(cid) else {
            return self
                .reply(resp, msg, 481, "Call/Transaction Does Not Exist")
                .await;
        };
        // Only the dialog's own endpoint may transfer it (a REFER from elsewhere would be a way to
        // hijack a call or place toll calls).
        if resp.peer().ip() != parties.transferor.addr.ip() {
            tracing::warn!(src = %resp.peer(), "SIP REFER from a non-party address rejected");
            return self.reply(resp, msg, 403, "Forbidden").await;
        }
        // ...and only an *extension* (a registered device of ours): the carrier side of a call
        // (a trunk-origin transferor, e.g. the caller of an inbound DID) must never be able to
        // steer CommOS into placing PSTN legs.
        if !self.is_extension_leg(&parties.transferor).await {
            tracing::warn!(src = %resp.peer(), "SIP REFER from a non-extension (trunk) leg rejected");
            return self.reply(resp, msg, 403, "Forbidden").await;
        }
        let Some(refer) = msg.refer_to().filter(|r| r.user.is_some()) else {
            return self.reply(resp, msg, 400, "Bad Request").await;
        };
        let refer_cseq = msg.cseq_parts().map(|(n, _)| n).unwrap_or(1);
        let from_ref = msg
            .referred_by()
            .or_else(|| extract_uri(&parties.transferor.to));
        tracing::info!(call_id = %parties.call_id, to = %refer.uri,
            attended = refer.replaces.is_some(), "SIP REFER");

        let accepted = message::response_with(
            msg,
            202,
            "Accepted",
            &[("Contact", format!("<sip:commos@{}>", self.media_ip))],
        );
        resp.send(accepted.as_bytes()).await?;

        // From here the transfer runs and reports progress via NOTIFY on the REFER's subscription.
        let mut t = parties.transferor.clone();
        self.notify_refer(&mut t, refer_cseq, "SIP/2.0 100 Trying", false)
            .await;
        let (outcome, kind) = match &refer.replaces {
            Some(rep) => (
                self.attended_transfer(&parties, rep).await,
                TransferType::Attended,
            ),
            None => (
                self.blind_transfer(&parties, &refer).await,
                TransferType::Blind,
            ),
        };
        match outcome {
            Ok(also_bye) => {
                self.notify_refer(&mut t, refer_cseq, "SIP/2.0 200 OK", true)
                    .await;
                // Record the transfer (CallTransferred event); the Call itself carries on.
                if let Err(e) = self
                    .routing
                    .transfer(
                        self.default_tenant,
                        parties.call_id,
                        refer.uri.clone(),
                        kind,
                        from_ref,
                    )
                    .await
                {
                    tracing::warn!(error = %e, call_id = %parties.call_id, "recording transfer failed");
                }
                // Drop the transferor (both of its legs for an attended transfer).
                self.send_bye_to_leg(&t).await;
                for leg in &also_bye {
                    self.send_bye_to_leg(leg).await;
                }
                tracing::info!(call_id = %parties.call_id, to = %refer.uri, "SIP transfer complete");
            }
            Err((status, reason)) => {
                tracing::info!(call_id = %parties.call_id, status, "SIP transfer failed; call kept");
                self.notify_refer(&mut t, refer_cseq, &format!("SIP/2.0 {status} {reason}"), true)
                    .await;
                // The call lives on: keep our CSeq space for this leg monotonic for the later BYE.
                self.sync_leg_cseq(&t);
            }
        }
        Ok(())
    }

    /// Whether `leg` is one of our registered extensions: its identity (the user of the leg's
    /// remote party header) has a live registration whose contact host is the leg's address. A
    /// carrier can claim any user in a `From`, but cannot share a registered phone's address.
    async fn is_extension_leg(&self, leg: &CalleeLeg) -> bool {
        let Some(user) = extract_uri(&leg.to).and_then(|u| user_part(&u).map(str::to_string)) else {
            return false;
        };
        for r in self.registrations.list(self.default_tenant) {
            if user_part(&r.aor).is_some_and(|u| u.eq_ignore_ascii_case(&user)) {
                if let Some(a) = resolve_contact_addr(&r.contact).await {
                    if a.ip() == leg.addr.ip() {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Snapshot a bridged dialog (by either leg's Call-ID) from the REFER sender's viewpoint.
    fn refer_parties(&self, cid: &str) -> Option<Parties> {
        let dialogs = self.dialogs.lock().expect("dialogs mutex");
        let (primary, from_callee) = if dialogs.contains_key(cid) {
            (cid.to_string(), false)
        } else {
            let alias = self
                .bye_aliases
                .lock()
                .expect("aliases mutex")
                .get(cid)
                .cloned()?;
            (alias, true)
        };
        let d = dialogs.get(&primary)?;
        if !matches!(d.media, Media::Bridge(_)) {
            return None;
        }
        let (caller, callee) = (d.caller.as_ref()?, d.callee.as_ref()?);
        let (transferor, transferee) = if from_callee {
            (callee.clone(), caller.clone())
        } else {
            (caller.clone(), callee.clone())
        };
        Some(Parties {
            primary,
            transferor,
            transferee,
            transferor_is_caller: !from_callee,
            call_id: d.call_id,
            answer_sdp: d.answer_sdp.clone(),
        })
    }

    /// Persist `leg.cseq` (after NOTIFYs consumed CSeq numbers) into whichever dialog owns it.
    fn sync_leg_cseq(&self, leg: &CalleeLeg) {
        let mut dialogs = self.dialogs.lock().expect("dialogs mutex");
        for d in dialogs.values_mut() {
            for l in [d.caller.as_mut(), d.callee.as_mut()].into_iter().flatten() {
                if l.call_id == leg.call_id {
                    l.cseq = l.cseq.max(leg.cseq);
                }
            }
        }
    }

    /// Send one `NOTIFY` (`Event: refer`, `message/sipfrag`) to the transferor, in its dialog:
    /// our From/To (with tags) and Call-ID, the next CSeq (the leg's counter is advanced so the
    /// eventual BYE stays higher), and a reachable Via for the response.
    async fn notify_refer(&self, leg: &mut CalleeLeg, refer_cseq: u32, frag: &str, last: bool) {
        leg.cseq += 1;
        let sock = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(error = %e, "could not bind socket for refer NOTIFY");
                return;
            }
        };
        let sent_by = SocketAddr::new(self.media_ip, sock_port(&sock));
        let state = if last {
            "terminated;reason=noresource"
        } else {
            "active;expires=60"
        };
        let body = format!("{frag}\r\n");
        let req = message::request(
            "NOTIFY",
            &leg.request_uri,
            &[
                ("Via", message::via_header(sent_by)),
                ("From", leg.from.clone()),
                ("To", leg.to.clone()),
                ("Call-ID", leg.call_id.clone()),
                ("CSeq", format!("{} NOTIFY", leg.cseq)),
                ("Event", format!("refer;id={refer_cseq}")),
                ("Subscription-State", state.to_string()),
                ("Contact", format!("<sip:commos@{}>", self.media_ip)),
            ],
            Some(("message/sipfrag;version=2.0", &body)),
        );
        if !send_request_reliable_on(&sock, req.as_bytes(), leg.addr).await {
            tracing::debug!(addr = %leg.addr, "refer NOTIFY unconfirmed");
        }
    }

    /// Place the transfer target's call: an internal registration (via the extension route table
    /// first) or an external number through the outbound trunk. Returns the target's own relay,
    /// its dialog leg and the codec it answered with.
    async fn dial_transfer_target(
        &self,
        refer: &ReferTo,
        offer: &codec::AudioMedia,
        caller_id: CallerId<'_>,
    ) -> Result<(rtp::Bridge, CalleeLeg, codec::Codec), Fail> {
        let tenant = self.default_tenant;
        let user = refer.user.as_deref().ok_or((400, "Bad Request"))?;
        // A distinct id so the new leg's Call-ID/From-tag never collide with the live call's.
        let leg_id = Uuid::now_v7();
        let routed = self.resolve_route(&refer.uri).await;
        let reg = routed
            .as_deref()
            .and_then(|d| find_registered(&self.registrations, tenant, d))
            .or_else(|| find_registered(&self.registrations, tenant, user));
        if let Some(reg) = reg {
            // A do-not-disturb extension is never rung, transfer or not.
            let ext = user_part(&reg.aor).unwrap_or(user);
            if self.is_dnd(ext).await {
                tracing::info!(ext, "transfer target is in DND; refused");
                return Err((486, "Busy Here"));
            }
            return match self
                .try_bridge(&reg, leg_id, None, offer, None, caller_id, None)
                .await
            {
                BridgeOutcome::Answered(b, leg, codec, _te, _crypto) => Ok((b, leg, codec)),
                BridgeOutcome::Declined(486) => Err((486, "Busy Here")),
                BridgeOutcome::Declined(600) => Err((600, "Busy Everywhere")),
                BridgeOutcome::Declined(_) => Err((603, "Decline")),
                BridgeOutcome::NoAnswer => Err((480, "Temporarily Unavailable")),
            };
        }
        if let Some((gw, num)) = self.select_outbound_gateway(user).await {
            // External target: outbound policy (international gating, caps) + own-DID loop guard.
            self.vet_external(&gw, &num).await?;
            tracing::warn!(target = %num, "SIP REFER: external transfer destination");
            return self
                .try_trunk(&gw, &num, leg_id, None, offer, None)
                .await
                .map(|(b, leg, codec, _te, _crypto)| (b, leg, codec))
                .ok_or((503, "Service Unavailable"));
        }
        Err((404, "Not Found"))
    }

    /// Blind transfer: call the target, then re-point the transferor's relay leg at it.
    async fn blind_transfer(&self, p: &Parties, refer: &ReferTo) -> Result<Vec<CalleeLeg>, Fail> {
        let offer = codec::AudioMedia::parse(&p.answer_sdp);
        // Present the transferee (the party staying on the call) as the caller to the target.
        let x_uri = extract_uri(&p.transferee.to);
        let x_display = header_display_name(&p.transferee.to);
        let caller_id = CallerId {
            number: x_uri.as_deref().and_then(user_part),
            display: x_display.as_deref(),
        };
        let (n_bridge, c_leg, c_codec) = self.dial_transfer_target(refer, &offer, caller_id).await?;
        let want = offer.preferred_audio().unwrap_or_else(default_codec);
        if !c_codec.name.eq_ignore_ascii_case(&want.name) {
            // The relay is byte-transparent: the target must speak the call's codec.
            self.send_bye_to_leg(&c_leg).await;
            n_bridge.abort();
            return Err((488, "Not Acceptable Here"));
        }
        let sock = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => s,
            Err(_) => {
                self.send_bye_to_leg(&c_leg).await;
                n_bridge.abort();
                return Err((500, "Server Internal Error"));
            }
        };
        let peer = loopback(n_bridge.leg_a_port);
        let mut n_bridge = Some(n_bridge);

        let committed = {
            let mut dialogs = self.dialogs.lock().expect("dialogs mutex");
            match dialogs.remove(&p.primary) {
                Some(mut d) if matches!(d.media, Media::Bridge(_)) => {
                    let mut leg_a_port = 0;
                    if let Media::Bridge(b) = &mut d.media {
                        b.retarget(
                            leg_of(p.transferor_is_caller),
                            sock,
                            Some(peer),
                            n_bridge.take(),
                        );
                        b.set_hold(rtp::HoldState::Active);
                        if p.transferor_is_caller {
                            // The surviving callee becomes the dialog's primary (leg A) party.
                            b.swap_legs();
                        }
                        leg_a_port = b.leg_a_port;
                    }
                    d.callee = Some(c_leg.clone());
                    let mut aliases = self.bye_aliases.lock().expect("aliases mutex");
                    let key = if p.transferor_is_caller {
                        d.caller = Some(p.transferee.clone());
                        d.answer_sdp = self.rebuilt_answer(&p.answer_sdp, leg_a_port);
                        aliases.remove(&p.transferee.call_id);
                        p.transferee.call_id.clone()
                    } else {
                        aliases.remove(&p.transferor.call_id);
                        p.primary.clone()
                    };
                    aliases.insert(c_leg.call_id.clone(), key.clone());
                    drop(aliases);
                    dialogs.insert(key, d);
                    true
                }
                other => {
                    if let Some(d) = other {
                        dialogs.insert(p.primary.clone(), d);
                    }
                    false
                }
            }
        };
        if !committed {
            // The call ended (or changed) while the target was ringing.
            self.send_bye_to_leg(&c_leg).await;
            if let Some(n) = n_bridge {
                n.abort();
            }
            return Err((481, "Call/Transaction Does Not Exist"));
        }
        Ok(Vec::new())
    }

    /// Attended transfer: join the transferee of this call with the far party of the transferor's
    /// other (consultation) call, identified by `Replaces`. Returns the transferor's consultation
    /// leg, which must also be hung up.
    async fn attended_transfer(&self, p: &Parties, rep: &Replaces) -> Result<Vec<CalleeLeg>, Fail> {
        let s1 = UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|_| (500, "Server Internal Error"))?;
        let s2 = UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|_| (500, "Server Internal Error"))?;
        let (a1, a2) = (loopback(sock_port(&s1)), loopback(sock_port(&s2)));

        let (d2_call, d2_capture, t2) = {
            let mut dialogs = self.dialogs.lock().expect("dialogs mutex");
            // Locate the consultation dialog by Call-ID (primary key, or a callee-leg alias).
            let (primary2, t2_is_caller) = if dialogs.contains_key(&rep.call_id) {
                (rep.call_id.clone(), true)
            } else {
                let alias = self
                    .bye_aliases
                    .lock()
                    .expect("aliases mutex")
                    .get(&rep.call_id)
                    .cloned();
                match alias {
                    Some(a) => (a, false),
                    None => return Err((481, "Call/Transaction Does Not Exist")),
                }
            };
            if primary2 == p.primary {
                return Err((400, "Bad Request"));
            }
            // Validate both dialogs (read-only) before touching either.
            let (t2, y) = {
                let (Some(d1), Some(d2)) = (dialogs.get(&p.primary), dialogs.get(&primary2)) else {
                    return Err((481, "Call/Transaction Does Not Exist"));
                };
                let (Some(c2), Some(e2)) = (d2.caller.as_ref(), d2.callee.as_ref()) else {
                    return Err((481, "Call/Transaction Does Not Exist"));
                };
                let (t2, y) = if t2_is_caller { (c2, e2) } else { (e2, c2) };
                if !matches!(d1.media, Media::Bridge(_)) || !matches!(d2.media, Media::Bridge(_)) {
                    return Err((488, "Not Acceptable Here"));
                }
                // The consultation call must be the transferor's own (same phone, matching tags).
                let tags = [message::header_tag(&t2.from), message::header_tag(&t2.to)];
                let has = |t: &str| tags.iter().flatten().any(|x| x == t);
                if t2.addr.ip() != p.transferor.addr.ip()
                    || !has(&rep.to_tag)
                    || !has(&rep.from_tag)
                {
                    return Err((481, "Call/Transaction Does Not Exist"));
                }
                // Byte-transparent relay: both far ends must share the call's codec.
                let c1 = codec::AudioMedia::parse(&d1.answer_sdp).preferred_audio();
                let c2 = codec::AudioMedia::parse(&d2.answer_sdp).preferred_audio();
                match (c1, c2) {
                    (Some(a), Some(b)) if a.name.eq_ignore_ascii_case(&b.name) => {}
                    _ => return Err((488, "Not Acceptable Here")),
                }
                (t2.clone(), y.clone())
            };

            let mut d1 = dialogs.remove(&p.primary).expect("checked above");
            let mut d2 = dialogs.remove(&primary2).expect("checked above");

            // Cross-link: each relay's transferor leg now forwards into the other's (and the
            // transferor's phone can no longer re-latch either), so the far ends hear each other.
            let leg1 = leg_of(p.transferor_is_caller);
            let leg2 = leg_of(t2_is_caller);
            let d2_media = std::mem::replace(&mut d2.media, Media::Echo(tokio::spawn(async {})));
            if let (Media::Bridge(mut b2), Media::Bridge(b1)) = (d2_media, &mut d1.media) {
                b2.retarget(leg2, s2, Some(a1), None);
                b2.set_hold(rtp::HoldState::Active);
                b1.retarget(leg1, s1, Some(a2), Some(b2));
                b1.set_hold(rtp::HoldState::Active);
                if p.transferor_is_caller {
                    b1.swap_legs();
                }
                if p.transferor_is_caller {
                    d1.answer_sdp = self.rebuilt_answer(&p.answer_sdp, b1.leg_a_port);
                }
            }
            // Old callee-leg aliases go away; the surviving far party is re-keyed under the
            // transferee (primary) Call-ID.
            {
                let mut aliases = self.bye_aliases.lock().expect("aliases mutex");
                for c in [d1.callee.as_ref(), d2.callee.as_ref()].into_iter().flatten() {
                    aliases.remove(&c.call_id);
                }
                aliases.insert(y.call_id.clone(), p.transferee.call_id.clone());
            }
            d1.caller = Some(p.transferee.clone());
            d1.callee = Some(y);
            let (d2_call, d2_capture) = (d2.call_id, d2.capture.take());
            dialogs.insert(p.transferee.call_id.clone(), d1);
            (d2_call, d2_capture, t2)
        };

        // The consultation Call ends (its CDR); any audio it captured is kept.
        if let Some(cap) = d2_capture {
            let bytes = std::mem::take(&mut *cap.lock().expect("capture mutex"));
            if !bytes.is_empty() {
                self.spawn_save_recording(d2_call, bytes);
            }
        }
        if let Err(e) = self
            .routing
            .hangup(self.default_tenant, d2_call, Some("transferred".to_string()))
            .await
        {
            tracing::debug!(error = %e, "ending consultation call failed");
        }
        Ok(vec![t2])
    }

    /// The SDP a party now on relay leg A should be answered with on a replayed (re-)INVITE: the
    /// call's codec/DTMF as before, but the relay's *current* leg-A port.
    fn rebuilt_answer(&self, old_sdp: &str, leg_a_port: u16) -> String {
        let media = codec::AudioMedia::parse(old_sdp);
        let codec = media.preferred_audio().unwrap_or_else(default_codec);
        let te = media
            .telephone_event_pt()
            .unwrap_or(dtmf::TELEPHONE_EVENT_PT);
        self.build_sdp(leg_a_port, &codec, te, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::ivr::IvrService;
    use crate::control::objects::ObjectService;
    use crate::control::policy::PolicyLimits;
    use crate::control::recordings::RecordingService;
    use crate::control::voicemail::VoicemailService;
    use crate::media::LoopbackMedia;
    use crate::objectstore::{LocalObjectStore, ObjectStore};
    use crate::relay::RelaySignal;
    use crate::store::MemStore;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
    use tokio::time::timeout;

    const T: Duration = Duration::from_secs(8);

    fn test_server() -> (Arc<SipServer>, Arc<dyn Store>) {
        let store: Arc<dyn Store> = Arc::new(MemStore::new());
        let signal = RelaySignal::new();
        let (tx, _rx) = unbounded_channel();
        let routing = Routing::new(
            store.clone(),
            Arc::new(LoopbackMedia::new(tx)),
            signal.clone(),
            PolicyLimits::default(),
            "1",
        );
        let root = std::env::temp_dir().join(format!("commos-refer-{}", Uuid::now_v7()));
        let blob: Arc<dyn ObjectStore> = Arc::new(LocalObjectStore::new(root));
        let objects = ObjectService::new(blob, store.clone(), signal.clone());
        let server = SipServer::new(
            RegistrationRegistry::new(),
            routing,
            "127.0.0.1".parse().unwrap(),
            Uuid::now_v7(),
            store.clone(),
            false,
            "commos",
            false,
            RecordingService::new(objects.clone(), store.clone(), signal.clone()),
            false,
            OnDecline::Announce,
            1,
            VoicemailService::new(objects.clone(), store.clone(), signal.clone()),
            IvrService::new(store.clone(), signal.clone()),
            objects,
            "1",
            false,
            false,
            "/nonexistent",
            "/nonexistent",
            Arc::new(crate::sip::moh::MohSource::load("/nonexistent")),
            false,
        );
        (Arc::new(server), store)
    }

    /// A scripted SIP phone on loopback: auto-answers INVITEs (200 + SDP at its RTP port) and
    /// every other request (200 OK), and forwards everything it receives to `rx`.
    struct Phone {
        sip: Arc<UdpSocket>,
        rtp: Arc<UdpSocket>,
        rx: UnboundedReceiver<SipMessage>,
        tag: &'static str,
    }

    impl Phone {
        async fn new(tag: &'static str) -> Phone {
            let sip = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
            let rtp = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
            let rtp_port = rtp.local_addr().unwrap().port();
            let sip_port = sip.local_addr().unwrap().port();
            let (tx, rx) = unbounded_channel();
            let s = sip.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65_535];
                while let Ok((n, from)) = s.recv_from(&mut buf).await {
                    let Ok(m) = message::parse(&buf[..n]) else { continue };
                    if m.is_request() {
                        match m.method() {
                            Some("ACK") => {}
                            Some("INVITE") => {
                                let sdp = format!(
                                    "v=0\r\no=p 0 0 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
                                     m=audio {rtp_port} RTP/AVP 0 101\r\na=rtpmap:0 PCMU/8000\r\n\
                                     a=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n"
                                );
                                let mut out = String::from("SIP/2.0 200 OK\r\n");
                                for v in m.header_all("Via") {
                                    out.push_str(&format!("Via: {v}\r\n"));
                                }
                                out.push_str(&format!("From: {}\r\n", m.header("From").unwrap()));
                                out.push_str(&format!("To: {};tag={tag}\r\n", m.header("To").unwrap()));
                                out.push_str(&format!(
                                    "Call-ID: {}\r\nCSeq: {}\r\n",
                                    m.call_id().unwrap(),
                                    m.cseq().unwrap()
                                ));
                                out.push_str(&format!("Contact: <sip:{tag}@127.0.0.1:{sip_port}>\r\n"));
                                out.push_str(&format!(
                                    "Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}",
                                    sdp.len()
                                ));
                                let _ = s.send_to(out.as_bytes(), from).await;
                            }
                            _ => {
                                let _ = s
                                    .send_to(message::response(&m, 200, "OK").as_bytes(), from)
                                    .await;
                            }
                        }
                    }
                    let _ = tx.send(m);
                }
            });
            Phone { sip, rtp, rx, tag }
        }

        fn sip_addr(&self) -> SocketAddr {
            self.sip.local_addr().unwrap()
        }
        fn rtp_port(&self) -> u16 {
            self.rtp.local_addr().unwrap().port()
        }
        fn responder(&self) -> Responder {
            Responder::Udp {
                socket: self.sip.clone(),
                dst: self.sip_addr(),
            }
        }
        fn register(&self, server: &SipServer, user: &str) {
            server.registrations.register(
                server.default_tenant,
                format!("sip:{user}@127.0.0.1"),
                format!("sip:{user}@{}", self.sip_addr()),
                None,
                3600,
            );
        }
        async fn next(&mut self) -> SipMessage {
            timeout(T, self.rx.recv())
                .await
                .expect("phone timed out waiting for a message")
                .unwrap()
        }
        /// Skip responses until the next *request* (ACKs included).
        async fn next_request(&mut self) -> SipMessage {
            loop {
                let m = self.next().await;
                if m.is_request() && m.method() != Some("ACK") {
                    return m;
                }
            }
        }
        async fn next_response_status(&mut self) -> u16 {
            loop {
                let m = self.next().await;
                if let Some(s) = m.status() {
                    return s;
                }
            }
        }
    }

    fn sdp_port(body: &[u8]) -> u16 {
        String::from_utf8_lossy(body)
            .lines()
            .find(|l| l.starts_with("m=audio "))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|p| p.parse().ok())
            .expect("SDP m=audio port")
    }

    /// `caller` INVITEs `user`; returns the caller's 200 OK.
    async fn place_call(
        server: &Arc<SipServer>,
        caller: &mut Phone,
        user: &str,
        call_id: &str,
    ) -> SipMessage {
        let sdp = format!(
            "v=0\r\no=p 0 0 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
             m=audio {} RTP/AVP 0 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\n",
            caller.rtp_port()
        );
        let invite = message::request(
            "INVITE",
            &format!("sip:{user}@127.0.0.1"),
            &[
                ("Via", format!("SIP/2.0/UDP {};branch=z9hG4bKx{call_id}", caller.sip_addr())),
                ("From", format!("<sip:{}@127.0.0.1>;tag={}", caller.tag, caller.tag)),
                ("To", format!("<sip:{user}@127.0.0.1>")),
                ("Call-ID", call_id.to_string()),
                ("CSeq", "1 INVITE".to_string()),
                ("Contact", format!("<sip:{}@{}>", caller.tag, caller.sip_addr())),
            ],
            Some(("application/sdp", &sdp)),
        );
        let (s, r) = (server.clone(), caller.responder());
        let h = tokio::spawn(async move { s.handle(&r, invite.as_bytes()).await });
        loop {
            let m = caller.next().await;
            if m.status() == Some(200) {
                h.await.unwrap().unwrap();
                return m;
            }
        }
    }

    /// An in-dialog REFER built from `own_from` (the REFER sender's own From, with its tag) and
    /// `remote_to` (the other side's header, with tag).
    fn refer_req(sender: &Phone, own_from: &str, remote_to: &str, call_id: &str, refer_to: &str) -> String {
        message::request(
            "REFER",
            "sip:commos@127.0.0.1",
            &[
                (
                    "Via",
                    format!("SIP/2.0/UDP {};branch=z9hG4bKr{}", sender.sip_addr(), Uuid::now_v7()),
                ),
                ("From", own_from.to_string()),
                ("To", remote_to.to_string()),
                ("Call-ID", call_id.to_string()),
                ("CSeq", "7 REFER".to_string()),
                ("Contact", format!("<sip:{}@{}>", sender.tag, sender.sip_addr())),
                ("Refer-To", refer_to.to_string()),
                ("Referred-By", format!("<sip:{}@127.0.0.1>", sender.tag)),
            ],
            None,
        )
    }

    fn bye_req(p: &Phone, call_id: &str, from: &str, to: &str, cseq: u32) -> String {
        message::request(
            "BYE",
            "sip:commos@127.0.0.1",
            &[
                ("Via", format!("SIP/2.0/UDP {};branch=z9hG4bKb{}", p.sip_addr(), Uuid::now_v7())),
                ("From", from.to_string()),
                ("To", to.to_string()),
                ("Call-ID", call_id.to_string()),
                ("CSeq", format!("{cseq} BYE")),
            ],
            None,
        )
    }

    fn rtp_packet(marker: u8) -> Vec<u8> {
        let mut p = vec![0x80, 0x00, 0, 1, 0, 0, 0, 160, 0, 0, 0, 1];
        p.extend_from_slice(&[marker; 160]);
        p
    }

    /// Send RTP from `from` to `to_port` until `listener` hears it (a relay latches on the first
    /// packet it sees, so both ends keep talking while the path settles).
    async fn rtp_hears(from: &Phone, to_port: u16, marker: u8, listener: &Phone) -> bool {
        let tx = from.rtp.clone();
        let sender = tokio::spawn(async move {
            for _ in 0..250 {
                let _ = tx.send_to(&rtp_packet(marker), ("127.0.0.1", to_port)).await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let mut buf = [0u8; 2048];
        let ok = timeout(Duration::from_secs(4), async {
            loop {
                if let Ok((n, _)) = listener.rtp.recv_from(&mut buf).await {
                    if n > 12 && buf[12] == marker {
                        return;
                    }
                }
            }
        })
        .await
        .is_ok();
        sender.abort();
        ok
    }

    /// Both directions of media flow between `x` (sending to `x_target`) and `y` (to `y_target`).
    async fn media_flows(x: &Phone, x_target: u16, y: &Phone, y_target: u16) -> bool {
        let (a, b) = tokio::join!(rtp_hears(x, x_target, 0xA1, y), rtp_hears(y, y_target, 0xC3, x));
        a && b
    }

    async fn outbox_has(store: &Arc<dyn Store>, needle: &str) -> bool {
        store
            .peek_outbox(100)
            .await
            .unwrap()
            .iter()
            .any(|r| r.event.to_string().contains(needle))
    }

    /// Callee B transfers the call (blind) to C: A ends up bridged with C, B is BYEd, and B sees
    /// `202` then `NOTIFY` 100 Trying then 200 OK.
    #[tokio::test]
    async fn blind_transfer_from_callee_rebridges_caller_to_target() {
        let (server, store) = test_server();
        let mut a = Phone::new("alice").await;
        let mut b = Phone::new("bob").await;
        let mut c = Phone::new("carol").await;
        b.register(&server, "200");
        c.register(&server, "300");

        let ok = place_call(&server, &mut a, "200", "call-a-b").await;
        let a_leg_port = sdp_port(ok.body());
        let invite_b = b.next_request().await;

        let req = refer_req(
            &b,
            &format!("{};tag=bob", invite_b.header("To").unwrap()),
            invite_b.header("From").unwrap(),
            invite_b.call_id().unwrap(),
            "<sip:300@127.0.0.1>",
        );
        let (s, r) = (server.clone(), b.responder());
        let h = tokio::spawn(async move { s.handle(&r, req.as_bytes()).await });

        assert_eq!(b.next_response_status().await, 202);
        let n1 = b.next_request().await;
        assert_eq!(n1.method(), Some("NOTIFY"));
        assert_eq!(n1.header("Event"), Some("refer;id=7"));
        assert_eq!(n1.header("Content-Type"), Some("message/sipfrag;version=2.0"));
        assert!(String::from_utf8_lossy(n1.body()).starts_with("SIP/2.0 100 Trying"));
        assert!(n1.header("Via").unwrap().contains("127.0.0.1:"));
        let n2 = b.next_request().await;
        assert_eq!(n2.method(), Some("NOTIFY"));
        assert!(String::from_utf8_lossy(n2.body()).starts_with("SIP/2.0 200 OK"));
        assert!(n2.header("Subscription-State").unwrap().starts_with("terminated"));
        assert!(n2.cseq_parts().unwrap().0 > n1.cseq_parts().unwrap().0);
        let bye = b.next_request().await;
        assert_eq!(bye.method(), Some("BYE"));
        assert_eq!(bye.call_id(), invite_b.call_id());
        assert!(
            bye.cseq_parts().unwrap().0 > n2.cseq_parts().unwrap().0,
            "BYE CSeq must follow the NOTIFYs"
        );
        h.await.unwrap().unwrap();

        // C was invited, with Alice presented as the caller.
        let invite_c = c.next_request().await;
        assert_eq!(invite_c.method(), Some("INVITE"));
        assert!(invite_c.header("From").unwrap().contains("alice"), "{:?}", invite_c.header("From"));
        let c_leg_port = sdp_port(invite_c.body());

        // Media: A <-> C through the re-targeted relay (B is out).
        assert!(media_flows(&a, a_leg_port, &c, c_leg_port).await, "A and C must hear each other");
        assert!(outbox_has(&store, "CallTransferred").await);

        // Hangup bookkeeping follows the new pair: C hanging up BYEs A.
        let c_bye = bye_req(
            &c,
            invite_c.call_id().unwrap(),
            &format!("{};tag=carol", invite_c.header("To").unwrap()),
            invite_c.header("From").unwrap(),
            2,
        );
        server.handle(&c.responder(), c_bye.as_bytes()).await.unwrap();
        let m = a.next_request().await;
        assert_eq!(m.method(), Some("BYE"));
        assert_eq!(m.call_id(), Some("call-a-b"));
        assert!(server.dialogs.lock().unwrap().is_empty());
    }

    /// The *caller* A transfers (blind) the call to C: callee B stays up and is bridged to C, and
    /// the dialog is re-keyed so B's later BYE tears down C.
    #[tokio::test]
    async fn blind_transfer_from_caller_rebridges_callee_to_target() {
        let (server, _store) = test_server();
        let mut a = Phone::new("alice").await;
        let mut b = Phone::new("bob").await;
        let mut c = Phone::new("carol").await;
        a.register(&server, "alice");
        b.register(&server, "200");
        c.register(&server, "300");

        let ok = place_call(&server, &mut a, "200", "call-a-b2").await;
        let invite_b = b.next_request().await;
        let b_leg_port = sdp_port(invite_b.body());

        let req = refer_req(
            &a,
            "<sip:alice@127.0.0.1>;tag=alice",
            ok.header("To").unwrap(),
            "call-a-b2",
            "<sip:300@127.0.0.1;user=phone>",
        );
        let (s, r) = (server.clone(), a.responder());
        let h = tokio::spawn(async move { s.handle(&r, req.as_bytes()).await });
        assert_eq!(a.next_response_status().await, 202);
        let mut saw = Vec::new();
        let bye = loop {
            let m = a.next_request().await;
            if m.method() == Some("BYE") {
                break m;
            }
            saw.push(String::from_utf8_lossy(m.body()).trim().to_string());
        };
        assert_eq!(bye.call_id(), Some("call-a-b2"));
        assert!(
            saw.iter().any(|b| b.starts_with("SIP/2.0 100")) && saw.iter().any(|b| b.starts_with("SIP/2.0 200")),
            "{saw:?}"
        );
        h.await.unwrap().unwrap();

        let invite_c = c.next_request().await;
        assert!(
            invite_c.header("From").unwrap().contains("200"),
            "Bob presented to C: {:?}",
            invite_c.header("From")
        );
        let c_leg_port = sdp_port(invite_c.body());
        // B keeps sending to the port it was given (now relay leg A); C reaches it via the chain.
        assert!(media_flows(&b, b_leg_port, &c, c_leg_port).await, "B and C must hear each other");

        // B hangs up (its Call-ID is now the primary key): C gets the BYE.
        let b_bye = bye_req(
            &b,
            invite_b.call_id().unwrap(),
            &format!("{};tag=bob", invite_b.header("To").unwrap()),
            invite_b.header("From").unwrap(),
            9,
        );
        server.handle(&b.responder(), b_bye.as_bytes()).await.unwrap();
        assert_eq!(c.next_request().await.method(), Some("BYE"));
        assert!(server.dialogs.lock().unwrap().is_empty());
    }

    /// An unreachable target fails the transfer: the transferor gets the final status in a NOTIFY,
    /// is NOT hung up, the call survives, and a later BYE uses a CSeq above the NOTIFYs.
    #[tokio::test]
    async fn failed_transfer_notifies_error_and_keeps_call() {
        let (server, _store) = test_server();
        let mut a = Phone::new("alice").await;
        let mut b = Phone::new("bob").await;
        b.register(&server, "200");
        let _ = place_call(&server, &mut a, "200", "call-fail").await;
        let invite_b = b.next_request().await;

        let req = refer_req(
            &b,
            &format!("{};tag=bob", invite_b.header("To").unwrap()),
            invite_b.header("From").unwrap(),
            invite_b.call_id().unwrap(),
            "<sip:999@127.0.0.1>",
        );
        server.handle(&b.responder(), req.as_bytes()).await.unwrap();
        assert_eq!(b.next_response_status().await, 202);
        let mut last_cseq = 0;
        let mut bodies = Vec::new();
        while bodies.len() < 2 {
            let m = b.next_request().await;
            assert_eq!(m.method(), Some("NOTIFY"), "the transferor must not be hung up");
            last_cseq = m.cseq_parts().unwrap().0;
            bodies.push(String::from_utf8_lossy(m.body()).trim().to_string());
        }
        assert!(bodies[0].starts_with("SIP/2.0 100"));
        assert!(bodies[1].starts_with("SIP/2.0 404"), "{bodies:?}");
        assert_eq!(server.dialogs.lock().unwrap().len(), 1, "original call must survive");

        // The caller now hangs up: B is BYEd with a CSeq above the NOTIFYs.
        let a_bye = bye_req(
            &a,
            "call-fail",
            "<sip:alice@127.0.0.1>;tag=alice",
            "<sip:200@127.0.0.1>",
            2,
        );
        server.handle(&a.responder(), a_bye.as_bytes()).await.unwrap();
        let bye = b.next_request().await;
        assert_eq!(bye.method(), Some("BYE"));
        assert!(bye.cseq_parts().unwrap().0 > last_cseq);
    }

    /// A REFER from a leg that is not a registered extension (e.g. the carrier side of a call) is
    /// refused with 403 and the call is untouched.
    #[tokio::test]
    async fn refer_from_a_non_extension_leg_is_403() {
        let (server, _store) = test_server();
        let mut a = Phone::new("alice").await; // NOT registered: stands in for a trunk-origin caller
        let mut b = Phone::new("bob").await;
        let mut c = Phone::new("carol").await;
        b.register(&server, "200");
        c.register(&server, "300");
        let ok = place_call(&server, &mut a, "200", "call-trunk").await;
        let _ = b.next_request().await;
        let req = refer_req(
            &a,
            "<sip:alice@127.0.0.1>;tag=alice",
            ok.header("To").unwrap(),
            "call-trunk",
            "<sip:300@127.0.0.1>",
        );
        server.handle(&a.responder(), req.as_bytes()).await.unwrap();
        assert_eq!(a.next_response_status().await, 403);
        assert_eq!(server.dialogs.lock().unwrap().len(), 1, "call must be untouched");
    }

    /// Transferring to a do-not-disturb extension fails with a 486 NOTIFY; the extension never rings.
    #[tokio::test]
    async fn refer_to_a_dnd_extension_is_486_and_never_rings() {
        use crate::store::Tx;
        let (server, store) = test_server();
        let mut a = Phone::new("alice").await;
        let mut b = Phone::new("bob").await;
        let mut c = Phone::new("carol").await;
        b.register(&server, "200");
        c.register(&server, "300");
        let mut ext = commos_core::entities::extension::Extension::new(
            server.default_tenant,
            "300",
            Uuid::now_v7(),
        );
        ext.dnd = true;
        store.commit(Tx { extensions: vec![ext], ..Default::default() }).await.unwrap();

        let _ = place_call(&server, &mut a, "200", "call-dnd").await;
        let invite_b = b.next_request().await;
        let req = refer_req(
            &b,
            &format!("{};tag=bob", invite_b.header("To").unwrap()),
            invite_b.header("From").unwrap(),
            invite_b.call_id().unwrap(),
            "<sip:300@127.0.0.1>",
        );
        server.handle(&b.responder(), req.as_bytes()).await.unwrap();
        assert_eq!(b.next_response_status().await, 202);
        let mut bodies = Vec::new();
        while bodies.len() < 2 {
            let m = b.next_request().await;
            bodies.push(String::from_utf8_lossy(m.body()).trim().to_string());
        }
        assert!(bodies[1].starts_with("SIP/2.0 486"), "{bodies:?}");
        assert!(
            timeout(Duration::from_millis(300), c.rx.recv()).await.is_err(),
            "the DND extension must not be rung"
        );
        assert_eq!(server.dialogs.lock().unwrap().len(), 1, "original call survives");
    }

    /// An external REFER target goes through outbound policy: international is denied by default
    /// (403 NOTIFY), and a target that is one of our own DIDs is refused as a loop (482).
    #[tokio::test]
    async fn refer_to_external_target_is_policy_checked() {
        use crate::store::Tx;
        use commos_core::entities::{did::Did, gateway::Gateway};
        let (server, store) = test_server();
        let mut a = Phone::new("alice").await;
        let mut b = Phone::new("bob").await;
        b.register(&server, "200");
        let carrier = Uuid::now_v7();
        let gw = Gateway::new_sip(server.default_tenant, carrier, "127.0.0.1:9");
        let did = Did::new(server.default_tenant, "+14155550123", carrier, "sip:200@x");
        store
            .commit(Tx { gateways: vec![gw], dids: vec![did], ..Default::default() })
            .await
            .unwrap();
        let _ = place_call(&server, &mut a, "200", "call-ext").await;
        let invite_b = b.next_request().await;

        for (target, want) in [
            ("<sip:+442071838750@127.0.0.1>", "SIP/2.0 403"),
            ("<sip:+14155550123@127.0.0.1>", "SIP/2.0 482"),
        ] {
            let req = refer_req(
                &b,
                &format!("{};tag=bob", invite_b.header("To").unwrap()),
                invite_b.header("From").unwrap(),
                invite_b.call_id().unwrap(),
                target,
            );
            server.handle(&b.responder(), req.as_bytes()).await.unwrap();
            assert_eq!(b.next_response_status().await, 202);
            let mut bodies = Vec::new();
            while bodies.len() < 2 {
                let m = b.next_request().await;
                bodies.push(String::from_utf8_lossy(m.body()).trim().to_string());
            }
            assert!(bodies[1].starts_with(want), "{target}: {bodies:?}");
            assert_eq!(server.dialogs.lock().unwrap().len(), 1, "call survives a refused transfer");
        }
    }

    /// A call to a DID that routes to a DND extension must not ring it: DND is keyed on the
    /// resolved target (300), not the request-URI (the DID).
    #[tokio::test]
    async fn did_routed_call_to_a_dnd_extension_does_not_ring() {
        use crate::store::Tx;
        use commos_core::entities::{did::Did, extension::Extension};
        let (server, store) = test_server();
        let mut a = Phone::new("alice").await;
        let mut c = Phone::new("carol").await;
        c.register(&server, "300");
        let mut ext = Extension::new(server.default_tenant, "300", Uuid::now_v7());
        ext.dnd = true;
        let did = Did::new(server.default_tenant, "+14155550123", Uuid::now_v7(), "sip:300@x");
        store
            .commit(Tx { extensions: vec![ext], dids: vec![did], ..Default::default() })
            .await
            .unwrap();
        let sdp = format!(
            "v=0\r\no=p 0 0 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
             m=audio {} RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n",
            a.rtp_port()
        );
        let invite = message::request(
            "INVITE",
            "sip:+14155550123@127.0.0.1",
            &[
                ("Via", format!("SIP/2.0/UDP {};branch=z9hG4bKdid", a.sip_addr())),
                ("From", "<sip:alice@127.0.0.1>;tag=alice".to_string()),
                ("To", "<sip:+14155550123@127.0.0.1>".to_string()),
                ("Call-ID", "did-dnd".to_string()),
                ("CSeq", "1 INVITE".to_string()),
                ("Contact", format!("<sip:alice@{}>", a.sip_addr())),
            ],
            Some(("application/sdp", &sdp)),
        );
        let (s, r) = (server.clone(), a.responder());
        tokio::spawn(async move { s.handle(&r, invite.as_bytes()).await });
        loop {
            let m = a.next().await;
            match m.status() {
                Some(486) => break,
                Some(200) => panic!("DND extension was answered via its DID"),
                _ => {}
            }
        }
        assert!(
            timeout(Duration::from_millis(300), c.rx.recv()).await.is_err(),
            "the DND extension must not be rung"
        );
    }

    /// `*72` to an external number is policy-vetted: international is refused by default and an
    /// own-DID target is refused as a loop, while a national number passes.
    #[tokio::test]
    async fn star72_external_forward_is_policy_vetted() {
        use super::featurecodes::FeatureCode;
        use crate::store::Tx;
        use commos_core::entities::{did::Did, gateway::Gateway};
        let (server, store) = test_server();
        let carrier = Uuid::now_v7();
        store
            .commit(Tx {
                gateways: vec![Gateway::new_sip(server.default_tenant, carrier, "127.0.0.1:9")],
                dids: vec![Did::new(server.default_tenant, "+14155550123", carrier, "sip:200@x")],
                ..Default::default()
            })
            .await
            .unwrap();
        let fwd = |n: &str| FeatureCode::ForwardOn(n.to_string());
        assert!(server.forward_allowed(&fwd("+14155550199")).await, "national passes");
        assert!(server.forward_allowed(&fwd("300")).await, "internal passes");
        assert!(!server.forward_allowed(&fwd("+442071838750")).await, "international denied");
        assert!(!server.forward_allowed(&fwd("+14155550123")).await, "own DID is a loop");
        assert!(server.forward_allowed(&FeatureCode::DndOn).await);
    }

    /// A REFER for an unknown dialog is refused.
    #[tokio::test]
    async fn refer_for_unknown_dialog_is_481() {
        let (server, _store) = test_server();
        let mut p = Phone::new("eve").await;
        let req = refer_req(&p, "<sip:eve@h>;tag=e", "<sip:x@h>;tag=x", "nope", "<sip:300@h>");
        server.handle(&p.responder(), req.as_bytes()).await.unwrap();
        assert_eq!(p.next_response_status().await, 481);
    }

    /// Attended transfer: B (callee of call 1 from A) calls C (call 2), then REFERs A to C with
    /// `Replaces` of call 2. A ends up bridged with C; both of B's legs are BYEd.
    #[tokio::test]
    async fn attended_transfer_joins_the_two_far_parties() {
        let (server, store) = test_server();
        let mut a = Phone::new("alice").await;
        let mut b = Phone::new("bob").await;
        let mut c = Phone::new("carol").await;
        b.register(&server, "200");
        c.register(&server, "300");

        let ok1 = place_call(&server, &mut a, "200", "call-1").await;
        let a_leg_port = sdp_port(ok1.body());
        let invite_b = b.next_request().await;

        // Call 2: B -> C.
        let ok2 = place_call(&server, &mut b, "300", "call-2").await;
        let invite_c = c.next_request().await;
        let c_leg_port = sdp_port(invite_c.body());
        let to_tag = message::header_tag(ok2.header("To").unwrap()).unwrap();

        let refer_to = format!("<sip:300@127.0.0.1?Replaces=call-2%3Bto-tag%3D{to_tag}%3Bfrom-tag%3Dbob>");
        let req = refer_req(
            &b,
            &format!("{};tag=bob", invite_b.header("To").unwrap()),
            invite_b.header("From").unwrap(),
            invite_b.call_id().unwrap(),
            &refer_to,
        );
        let (s, r) = (server.clone(), b.responder());
        let h = tokio::spawn(async move { s.handle(&r, req.as_bytes()).await });

        let mut notifies = Vec::new();
        let mut byes = Vec::new();
        while byes.len() < 2 {
            let m = b.next_request().await;
            match m.method() {
                Some("NOTIFY") => notifies.push(String::from_utf8_lossy(m.body()).trim().to_string()),
                Some("BYE") => byes.push(m.call_id().unwrap().to_string()),
                _ => {}
            }
        }
        h.await.unwrap().unwrap();
        assert!(notifies.iter().any(|n| n.starts_with("SIP/2.0 200")), "{notifies:?}");
        assert!(byes.contains(&"call-2".to_string()), "both transferor legs hung up: {byes:?}");
        assert!(byes.contains(&invite_b.call_id().unwrap().to_string()));

        assert!(media_flows(&a, a_leg_port, &c, c_leg_port).await, "A and C must hear each other");
        assert!(outbox_has(&store, "ATTENDED").await);
        assert_eq!(server.dialogs.lock().unwrap().len(), 1);

        // C hangs up -> A is BYEd (aliases re-keyed to the surviving pair).
        let c_bye = bye_req(
            &c,
            invite_c.call_id().unwrap(),
            &format!("{};tag=carol", invite_c.header("To").unwrap()),
            invite_c.header("From").unwrap(),
            2,
        );
        server.handle(&c.responder(), c_bye.as_bytes()).await.unwrap();
        assert_eq!(a.next_request().await.method(), Some("BYE"));
    }

    /// `Replaces` naming a dialog that does not exist is rejected (no media change).
    #[tokio::test]
    async fn attended_transfer_with_unknown_replaces_fails() {
        let (server, _store) = test_server();
        let mut a = Phone::new("alice").await;
        let mut b = Phone::new("bob").await;
        b.register(&server, "200");
        let _ = place_call(&server, &mut a, "200", "call-x").await;
        let invite_b = b.next_request().await;
        let req = refer_req(
            &b,
            &format!("{};tag=bob", invite_b.header("To").unwrap()),
            invite_b.header("From").unwrap(),
            invite_b.call_id().unwrap(),
            "<sip:300@h?Replaces=ghost%3Bto-tag%3Da%3Bfrom-tag%3Db>",
        );
        server.handle(&b.responder(), req.as_bytes()).await.unwrap();
        let mut bodies = Vec::new();
        while bodies.len() < 2 {
            let m = b.next_request().await;
            bodies.push(String::from_utf8_lossy(m.body()).trim().to_string());
        }
        assert!(bodies[1].starts_with("SIP/2.0 481"), "{bodies:?}");
        assert_eq!(server.dialogs.lock().unwrap().len(), 1);
    }
}
