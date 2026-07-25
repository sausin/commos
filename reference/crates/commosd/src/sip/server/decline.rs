//! Callee-decline handling: the unavailable announcement and leave-a-message path.

use super::*;

impl SipServer {
    /// The announcement + menu played to a caller whose call was actively declined
    /// (`on_decline = announce`). Composed from the sound pack — an "unavailable" preamble then a
    /// "press 1 to leave a message" instruction — each segment best-effort (a missing file is
    /// skipped) and closed with a short tone, so there is always an audible cue even with no sound
    /// pack. Reword by editing `DECLINE_PROMPTS`.
    pub(super) async fn decline_announcement(&self, codec: g711::G711) -> Vec<u8> {
        // "I'm sorry; nobody is available to take your call. Press one — please leave a message."
        const DECLINE_PROMPTS: &[&str] = &[
            "im-sorry",
            "vm-nobodyavail",
            "vm-press",
            "digits/1",
            "vm-leavemsg",
        ];
        let mut out = Vec::new();
        for name in DECLINE_PROMPTS {
            if let Some(mut audio) = self.load_prompt(name, codec).await {
                out.append(&mut audio);
            }
        }
        out.extend_from_slice(&g711::beep(200, codec));
        out
    }

    /// Answer a caller whose call the callee actively declined, then hand off to
    /// [`Self::decline_driver`] (announcement → leave-a-message-or-hang-up). Like the
    /// voicemail-deposit path the media is plaintext G.711 (SRTP for prompt-bearing media is
    /// future work). Falls back to a bare answer if the media socket can't bind, so the caller is
    /// never left ringing.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn answer_with_decline_announcement(
        &self,
        resp: &Responder,
        msg: &SipMessage,
        call_id: Uuid,
        call_id_hdr: &str,
        callee: &Registration,
        g711: g711::G711,
        te_pt: u8,
        src: SocketAddr,
    ) -> std::io::Result<()> {
        let g711_codec = codec::Codec {
            pt: g711.payload_type(),
            name: g711.sdp_name().to_string(),
            clock: 8000,
        };
        let sock = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "could not bind RTP for decline announcement; answering without media");
                self.mark_answered(call_id).await;
                let sdp = self.build_sdp(0, &g711_codec, te_pt, None);
                let ok = self.build_invite_ok(msg, &sdp, call_id);
                return resp.send(ok.as_bytes()).await;
            }
        };
        let rtp_port = sock.local_addr().map(|a| a.port()).unwrap_or(0);
        let announcement = self.decline_announcement(g711).await;
        // The mailbox the "leave a message" branch deposits to (the declining extension), with its
        // MWI target resolved now while we hold the registration.
        let notify = resolve_contact_addr(&callee.contact)
            .await
            .map(|addr| (addr, callee.contact.clone()));
        let vmbox = VoicemailBox {
            aor: callee.aor.clone(),
            notify,
        };
        let caller_leg = self.caller_leg(msg, call_id, src);
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(Self::decline_driver(
            sock,
            g711,
            te_pt,
            announcement,
            g711::beep(250, g711),
            self.default_tenant,
            call_id,
            vmbox,
            self.voicemails.clone(),
            self.media_ip,
            self.dialogs.clone(),
            call_id_hdr.to_string(),
            caller_leg,
            self.routing.clone(),
            stop_rx,
        ));
        // Answer plaintext G.711 (prompt-bearing media path, like the voicemail deposit).
        let sdp = self.build_sdp(rtp_port, &g711_codec, te_pt, None);
        if !call_id_hdr.is_empty() {
            self.dialogs.lock().expect("dialogs mutex").insert(
                call_id_hdr.to_string(),
                Dialog {
                    call_id,
                    // Media::Ivr tears down gracefully on a caller BYE (signals `stop`).
                    media: Media::Ivr {
                        task,
                        stop: stop_tx,
                    },
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
        tracing::info!(%call_id, rtp_port, codec = %g711_codec.name, "SIP INVITE answered (decline announcement)");
        resp.send(ok.as_bytes()).await
    }

    /// Drive the declined-call treatment: play the announcement, then honour a DTMF choice — `1`
    /// records a voicemail (like the deposit path), anything else (or silence) ends the call.
    /// Unlike the deposit/queue drivers this one **proactively hangs the caller up** when it
    /// finishes, since the caller reached a terminal announcement rather than a live party. If the
    /// caller hangs up first, `on_bye` tears the dialog down and signals `stop`, and this exits
    /// without a second teardown.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn decline_driver(
        sock: UdpSocket,
        codec: g711::G711,
        te_pt: u8,
        announcement: Vec<u8>,
        record_cue: Vec<u8>,
        tenant: Uuid,
        call_id: Uuid,
        vmbox: VoicemailBox,
        voicemails: VoicemailService,
        media_ip: IpAddr,
        dialogs: Arc<Mutex<HashMap<String, Dialog>>>,
        call_id_hdr: String,
        caller_leg: CalleeLeg,
        routing: Routing,
        mut stop_rx: tokio::sync::watch::Receiver<bool>,
    ) {
        const LEAVE_MESSAGE_KEY: char = '1';
        let pt = codec.payload_type();
        let (_info_tx, mut info_rx) = tokio::sync::mpsc::unbounded_channel::<char>();
        let mut peer: Option<SocketAddr> = None;
        // Play the announcement + menu, latching the caller and collecting a DTMF choice. The
        // window is the prompt length plus ~6 s to decide (G.711 is 8 bytes/ms).
        let window = Duration::from_millis((announcement.len() as u64 / 8) + 6000);
        let choice = tokio::select! {
            d = ivr::play_and_collect(&sock, &announcement, pt, te_pt, window, &mut info_rx, &mut peer) => d,
            _ = stop_rx.changed() => return, // caller hung up during the announcement → on_bye handles it
        };
        if *stop_rx.borrow() {
            return;
        }
        if choice == Some(LEAVE_MESSAGE_KEY) {
            // Leave-a-message branch: a record cue then capture until hangup/cap, saved as a
            // voicemail with MWI — the same tail as the deposit driver.
            if let Some(dst) = peer {
                ivr::play(&sock, dst, pt, &record_cue).await;
            }
            let capture: rtp::Capture = Arc::new(Mutex::new(Vec::new()));
            ivr::record_until_stop(&sock, te_pt, &capture, &mut stop_rx, IVR_VOICEMAIL_MAX).await;
            let audio = std::mem::take(&mut *capture.lock().expect("capture mutex"));
            if !audio.is_empty() {
                match voicemails.save(tenant, call_id, None, &audio).await {
                    Ok(vm) => {
                        tracing::info!(%call_id, voicemail_id = %vm.base.id, bytes = audio.len(), mailbox = %vmbox.aor,
                            "voicemail saved (after decline)");
                        if let Some((addr, contact)) = &vmbox.notify {
                            let number = user_part(&vmbox.aor).unwrap_or("");
                            let (new, old) = voicemails
                                .mailbox_summary(tenant, number)
                                .await
                                .unwrap_or((1, 0));
                            send_mwi_notify(*addr, contact, &vmbox.aor, media_ip, new, old).await;
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, %call_id, "saving voicemail failed"),
                }
            }
        }
        // Terminal announcement (or message left): proactively release the caller, unless it has
        // already hung up (then on_bye did the teardown).
        if *stop_rx.borrow() {
            return;
        }
        Self::hangup_caller(
            media_ip,
            &caller_leg,
            &dialogs,
            &call_id_hdr,
            &routing,
            tenant,
            call_id,
        )
        .await;
    }

    /// Proactively end a caller CommOS answered: remove the dialog (so a late caller BYE is a
    /// no-op and the entry doesn't leak), BYE the caller's phone, and record the hangup for the
    /// CDR. The caller-side mirror of the callee-BYE half of [`Self::on_bye`], callable from a
    /// detached driver (no `&self`). The `remove` doubles as the race guard: if a caller BYE beat
    /// us to it, the entry is already gone and we do nothing.
    pub(super) async fn hangup_caller(
        media_ip: IpAddr,
        caller_leg: &CalleeLeg,
        dialogs: &Arc<Mutex<HashMap<String, Dialog>>>,
        call_id_hdr: &str,
        routing: &Routing,
        tenant: Uuid,
        call_id: Uuid,
    ) {
        let existed = dialogs
            .lock()
            .expect("dialogs mutex")
            .remove(call_id_hdr)
            .is_some();
        if !existed {
            return; // a caller BYE raced us; on_bye already tore down + logged the hangup
        }
        bye_leg(media_ip, caller_leg).await;
        match routing
            .hangup(tenant, call_id, Some("BYE".to_string()))
            .await
        {
            Ok(_) => tracing::info!(%call_id, "decline announcement complete; caller released"),
            Err(e) => {
                tracing::debug!(error = %e, %call_id, "decline: caller hangup transition failed")
            }
        }
    }
}
