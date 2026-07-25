//! Voicemail deposit/retrieval drivers, greeting/prompt loading, storage and MWI.

use super::*;

impl SipServer {
    /// The voicemail deposit greeting: the recorded "please leave your message after the tone"
    /// prompt (`vm-intro`) when the sound pack is installed, followed by a 250 ms beep. Only audio
    /// after the beep is captured. With no sound pack it is the beep alone.
    pub(super) async fn voicemail_greeting(&self, codec: g711::G711) -> Vec<u8> {
        let mut greeting = self
            .load_prompt("vm-intro", codec)
            .await
            .unwrap_or_default();
        greeting.extend_from_slice(&g711::beep(250, codec));
        greeting
    }

    /// Preload the retrieval prompt buffers (transcoded to `codec`) so the spawned driver — which
    /// has no `&self` handle — can voice the count/feedback. Missing files become empty buffers.
    pub(super) async fn load_retrieval_prompts(&self, codec: g711::G711) -> RetrievalPrompts {
        let mut digits: Vec<Vec<u8>> = Vec::with_capacity(10);
        for d in 0..10 {
            digits.push(
                self.load_prompt(&format!("digits/{d}"), codec)
                    .await
                    .unwrap_or_default(),
            );
        }
        RetrievalPrompts {
            youhave: self
                .load_prompt("vm-youhave", codec)
                .await
                .unwrap_or_default(),
            messages: self
                .load_prompt("vm-messages", codec)
                .await
                .unwrap_or_default(),
            message: self
                .load_prompt("vm-message", codec)
                .await
                .unwrap_or_default(),
            no_more: self
                .load_prompt("vm-nomore", codec)
                .await
                .unwrap_or_default(),
            deleted: self
                .load_prompt("vm-deleted", codec)
                .await
                .unwrap_or_default(),
            enter_mailbox: self
                .load_prompt("vm-extension", codec)
                .await
                .unwrap_or_default(),
            digits,
        }
    }

    /// Answer a `*97`/`*98` retrieval call: bind RTP, answer `200 OK` (plaintext G.711, like the
    /// IVR menu path), and spawn the retrieval session (announce the count, play each message,
    /// handle the DTMF menu). `own_mailbox` is the caller's own extension for `*97`; for `*98` it
    /// is `None` and the driver prompts for the mailbox number over DTMF. Falls back to the echo
    /// path if the media socket cannot bind.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn answer_with_voicemail_retrieval(
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
        let audio = codec::Codec {
            pt: g711.payload_type(),
            name: g711.sdp_name().to_string(),
            clock: 8000,
        };
        let sdp = self.build_sdp(rtp_port, &audio, te_pt, None);
        if !call_id_hdr.is_empty() {
            self.dialogs.lock().expect("dialogs mutex").insert(
                call_id_hdr.to_string(),
                Dialog {
                    call_id,
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
        tracing::info!(%call_id, rtp_port, "SIP INVITE answered (voicemail retrieval *97/*98)");
        resp.send(ok.as_bytes()).await
    }

    /// Play the greeting/beep, then record the caller until they hang up (graceful `stop`) or the
    /// recording cap, and store the captured audio as a Voicemail with an MWI push. Reuses the IVR
    /// media primitives so only post-tone audio is captured.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn voicemail_deposit_driver(
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

    /// Drive a `*97`/`*98` retrieval session: (optionally prompt for a mailbox number), announce
    /// the message count, then play each message and act on the DTMF menu — `7` delete, `9` save,
    /// `#`/timeout next. Playing a message marks it read (heard). On exit, push a fresh MWI so the
    /// phone's lamp reflects what remains. All playback is interruptible by hangup.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn voicemail_retrieval_driver(
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
                let (new, old) = voicemails
                    .mailbox_summary(tenant, &mailbox)
                    .await
                    .unwrap_or((0, 0));
                send_mwi_notify(addr, &reg.contact, &reg.aor, media_ip, new, old).await;
            }
        }
        tracing::info!(mailbox = %mailbox, "voicemail retrieval finished");
    }

    /// Persist captured caller audio as a [`Voicemail`] for `vmbox`, then push a message-waiting
    /// indication to the mailbox's phone if it is registered — fire-and-forget off the BYE path.
    pub(super) fn spawn_save_voicemail(&self, call_id: Uuid, vmbox: VoicemailBox, bytes: Vec<u8>) {
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
                let (new, old) = voicemails
                    .mailbox_summary(tenant, number)
                    .await
                    .unwrap_or((1, 0));
                send_mwi_notify(*addr, contact_uri, &vmbox.aor, media_ip, new, old).await;
            }
        });
    }

    /// After a device (re-)registers, light its message-waiting lamp: if the mailbox for `aor`
    /// has unheard voicemails, push an MWI NOTIFY to its fresh `contact`. Fire-and-forget so the
    /// REGISTER `200 OK` is never delayed.
    pub(super) fn maybe_notify_mwi(&self, aor: String, contact: String) {
        let voicemails = self.voicemails.clone();
        let tenant = self.default_tenant;
        let media_ip = self.media_ip;
        tokio::spawn(async move {
            let Some(number) = user_part(&aor) else {
                return;
            };
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
}
