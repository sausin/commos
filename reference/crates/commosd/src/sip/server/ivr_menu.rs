//! IVR menu playout: prompt loading, the menu driver, and the answer path.

use super::*;

impl SipServer {
    /// Load an IVR's prompt audio: the recorded prompt Object when set, else a short tone
    /// synthesised in the negotiated `codec` so the caller hears that the menu is live. (A
    /// recorded prompt Object is assumed to already be in the negotiated codec.)
    pub(super) async fn load_ivr_prompt(
        &self,
        tenant: Uuid,
        ivr: &Ivr,
        codec: g711::G711,
    ) -> Vec<u8> {
        if let Some(obj_id) = ivr.prompt_object_id {
            match self.objects.get_bytes(tenant, obj_id).await {
                Ok((_obj, bytes)) => return bytes,
                Err(e) => {
                    tracing::warn!(error = %e, %obj_id, "IVR prompt object missing; using a tone")
                }
            }
        }
        g711::beep(400, codec)
    }

    /// Answer an INVITE that routes to an IVR: bind an RTP socket, answer `200 OK` with SDP, and
    /// spawn the menu session (play prompt + collect DTMF → resolve destination). Falls back to
    /// the echo path if the IVR is missing or its media socket can't bind.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn answer_with_ivr(
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
    pub(super) async fn ivr_driver(
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
            ivr::IvrOutcome::Selected { destination, .. }
                if destination.starts_with("voicemail") =>
            {
                if let Some(peer) = result.peer {
                    ivr::play(
                        &sock,
                        peer,
                        cfg.codec.payload_type(),
                        &g711::beep(250, cfg.codec),
                    )
                    .await;
                }
                let capture: rtp::Capture = Arc::new(Mutex::new(Vec::new()));
                ivr::record_until_stop(&sock, cfg.te_pt, &capture, &mut stop_rx, IVR_VOICEMAIL_MAX)
                    .await;
                let audio = std::mem::take(&mut *capture.lock().expect("capture mutex"));
                if audio.is_empty() {
                    tracing::info!(%call_id, "IVR voicemail: nothing recorded");
                    return;
                }
                match voicemails.save(tenant, call_id, None, &audio).await {
                    Ok(vm) => {
                        tracing::info!(%call_id, voicemail_id = %vm.base.id, bytes = audio.len(),
                        "IVR voicemail saved")
                    }
                    Err(e) => tracing::warn!(error = %e, %call_id, "saving IVR voicemail failed"),
                }
            }
            ivr::IvrOutcome::Selected { destination, .. } => {
                // A dial target: bridge the caller to a live registered extension, mid-call.
                if let (Some(peer), Some(callee)) = (
                    result.peer,
                    find_registered(&registrations, tenant, &destination),
                ) {
                    if ivr_transfer(
                        &sock,
                        peer,
                        &callee,
                        media_ip,
                        call_id,
                        cfg.codec,
                        cfg.te_pt,
                        no_answer_timeout,
                        display.clone(),
                        &mut stop_rx,
                    )
                    .await
                    {
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
    pub(super) async fn load_prompt(&self, name: &str, codec: g711::G711) -> Option<Vec<u8>> {
        let path = format!("{}/en/{}.ulaw", self.sounds_dir, name);
        match tokio::fs::read(&path).await {
            Ok(bytes) if !bytes.is_empty() => Some(g711::transcode_ulaw(&bytes, codec)),
            _ => None,
        }
    }
}
