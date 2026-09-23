//! Call-queue answer path and the queue-wait driver (greeting + MoH + agent ring).

use super::*;

impl SipServer {
    /// Answer a caller who reached a `queue:<uuid>` target and hand them to the queue-wait
    /// treatment loop: they are answered immediately (never left ringing) and then hear a
    /// greeting, music-on-hold, and periodic announcements while CommOS repeatedly tries to
    /// place them with an available registered member, overflowing after the queue's
    /// `max_wait_ms`. Mirrors [`Self::answer_with_ivr`]: bind a caller RTP socket, spawn the
    /// driver, answer `200`, and track it as a [`Media::Ivr`] session so a caller BYE stops it.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn answer_with_queue(
        &self,
        resp: &Responder,
        msg: &SipMessage,
        call_id: Uuid,
        call_id_hdr: &str,
        queue_id: Uuid,
        g711: g711::G711,
        te_pt: u8,
    ) -> std::io::Result<()> {
        let tenant = self.default_tenant;
        let queue = match self.store.get_queue(tenant, queue_id).await {
            Ok(Some(q)) => q,
            _ => {
                tracing::warn!(%queue_id, "queue not found; falling back to echo");
                return self.answer_with_echo(resp, msg, call_id, call_id_hdr).await;
            }
        };

        // Preload prompts (transcoded to the negotiated codec) so the spawned driver — which has
        // no `&self` — can voice them. A missing prompt file falls back to a short beep.
        let greeting = self
            .load_prompt("queue-greeting", g711)
            .await
            .unwrap_or_else(|| g711::beep(200, g711));
        let hold_prompt = self
            .load_prompt("queue-thankyou", g711)
            .await
            .unwrap_or_else(|| g711::beep(120, g711));

        let sock = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "could not bind queue RTP socket; falling back to echo");
                return self.answer_with_echo(resp, msg, call_id, call_id_hdr).await;
            }
        };
        let rtp_port = sock.local_addr().map(|a| a.port()).unwrap_or(0);

        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let display = self.call_display_name(call_id).await;
        let wait = crate::sip::queuewait::WaitConfig::from_max_wait_ms(queue.max_wait_ms);
        let task = tokio::spawn(Self::queue_wait_driver(
            sock,
            g711,
            te_pt,
            self.media_ip,
            call_id,
            self.moh.clone(),
            self.music_on_hold,
            self.registrations.clone(),
            queue.members.clone(),
            tenant,
            wait,
            queue.overflow_ref.clone(),
            self.no_answer_timeout,
            greeting,
            hold_prompt,
            display,
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
        tracing::info!(%call_id, %queue_id, rtp_port, members = queue.members.len(),
            "SIP INVITE answered (queue wait)");
        resp.send(ok.as_bytes()).await
    }

    /// The queue-wait treatment loop (spawned; no `&self`). Latches the caller's RTP, plays the
    /// greeting, then loops: try to place the caller with the next registered member (via
    /// [`ivr_transfer`], which rings them and splices on answer); if none answers, play
    /// music-on-hold for a poll interval with a periodic announcement; overflow once
    /// `max_wait` elapses. Ends when the caller hangs up (`stop`), a member connects, or
    /// overflow completes.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn queue_wait_driver(
        sock: UdpSocket,
        codec: g711::G711,
        te_pt: u8,
        media_ip: IpAddr,
        call_id: Uuid,
        moh: Arc<crate::sip::moh::MohSource>,
        music_on_hold: bool,
        registrations: RegistrationRegistry,
        members: Vec<String>,
        tenant: Uuid,
        wait: crate::sip::queuewait::WaitConfig,
        overflow_ref: Option<String>,
        no_answer_timeout: Duration,
        greeting: Vec<u8>,
        hold_prompt: Vec<u8>,
        display: Option<String>,
        mut stop_rx: tokio::sync::watch::Receiver<bool>,
    ) {
        let pt = codec.payload_type();
        // Latch the caller's RTP source (symmetric RTP) from their first packet, so we know
        // where to stream hold music. Give up if they never send media.
        let mut buf = vec![0u8; MAX_DATAGRAM];
        let peer = {
            let latch =
                tokio::time::timeout(Duration::from_secs(10), sock.recv_from(&mut buf)).await;
            match latch {
                Ok(Ok((_, from))) => from,
                _ => {
                    tracing::warn!(%call_id, "queue: caller sent no RTP; ending wait");
                    return;
                }
            }
        };

        // Greeting.
        ivr::play(&sock, peer, pt, &greeting).await;

        let start = tokio::time::Instant::now();
        let mut announced = 0u32;
        let mut cursor = 0usize;
        loop {
            if *stop_rx.borrow() {
                break; // caller hung up
            }
            if wait.overflow_due(start.elapsed()) {
                tracing::info!(%call_id, "queue: max wait reached; overflowing");
                ivr::play(&sock, peer, pt, &hold_prompt).await;
                if let Some(dest) = overflow_ref.as_deref() {
                    if let Some(reg) = find_registered(&registrations, tenant, dest) {
                        let _ = ivr_transfer(
                            &sock,
                            peer,
                            &reg,
                            media_ip,
                            call_id,
                            codec,
                            te_pt,
                            no_answer_timeout,
                            display.clone(),
                            &mut stop_rx,
                        )
                        .await;
                    }
                }
                break;
            }

            // Try to place the caller with the next registered member.
            if let Some(i) = crate::sip::queuewait::next_registered(
                &members,
                |m| find_registered(&registrations, tenant, m).is_some(),
                cursor,
            ) {
                cursor = i + 1;
                if let Some(reg) = find_registered(&registrations, tenant, &members[i]) {
                    // ivr_transfer rings the member (ring-back to the caller) and, on answer,
                    // relays until hangup. `true` → the caller was connected and the call is done.
                    if ivr_transfer(
                        &sock,
                        peer,
                        &reg,
                        media_ip,
                        call_id,
                        codec,
                        te_pt,
                        no_answer_timeout,
                        display.clone(),
                        &mut stop_rx,
                    )
                    .await
                    {
                        return;
                    }
                    // Member did not answer → keep the caller waiting.
                    if *stop_rx.borrow() {
                        break;
                    }
                }
            }

            // Periodic "still holding" announcement.
            if wait.announce_due(start.elapsed(), announced) {
                ivr::play(&sock, peer, pt, &hold_prompt).await;
                announced += 1;
            }

            // Hold music for one poll interval (or until the caller hangs up). When hold music
            // is disabled, just wait quietly.
            if music_on_hold {
                let _ = tokio::time::timeout(
                    wait.poll_every,
                    moh.stream_until(&sock, peer, pt, codec, stop_rx.clone()),
                )
                .await;
            } else {
                tokio::select! {
                    _ = tokio::time::sleep(wait.poll_every) => {}
                    _ = stop_rx.changed() => {}
                }
            }
        }
        tracing::info!(%call_id, waited_s = start.elapsed().as_secs(), "queue wait ended");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The queue-wait driver latches the caller, plays treatment audio, and — with no member to
    /// answer — overflows and exits cleanly once `max_wait` elapses (never hangs on hold music).
    #[tokio::test]
    async fn queue_wait_driver_overflows_and_exits_with_no_members() {
        use crate::control::registrations::RegistrationRegistry;

        let driver_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let driver_addr = driver_sock.local_addr().unwrap();
        let caller = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // The caller sends one RTP-sized packet so the driver latches its address.
        caller.send_to(&[0u8; 172], driver_addr).await.unwrap();

        let (_stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let moh = Arc::new(crate::sip::moh::MohSource::synth());
        let wait = crate::sip::queuewait::WaitConfig {
            max_wait: Some(Duration::from_millis(300)),
            announce_every: Duration::from_secs(30),
            poll_every: Duration::from_millis(50),
        };
        let beep = g711::beep(20, g711::G711::Ulaw);
        let driver = tokio::spawn(SipServer::queue_wait_driver(
            driver_sock,
            g711::G711::Ulaw,
            dtmf::TELEPHONE_EVENT_PT,
            "127.0.0.1".parse().unwrap(),
            Uuid::now_v7(),
            moh,
            true,
            RegistrationRegistry::new(),
            Vec::new(), // no members → nobody to place the caller with
            Uuid::now_v7(),
            wait,
            None, // no overflow target
            Duration::from_secs(1),
            beep.clone(),
            beep,
            None,
            stop_rx,
        ));

        // The caller receives treatment audio (greeting / hold music).
        let mut buf = [0u8; 2048];
        assert!(
            tokio::time::timeout(Duration::from_secs(2), caller.recv_from(&mut buf))
                .await
                .is_ok(),
            "caller should hear queue treatment audio"
        );
        // The driver overflows shortly after max_wait and finishes (does not hang on MoH).
        tokio::time::timeout(Duration::from_secs(3), driver)
            .await
            .expect("driver should finish after overflow")
            .unwrap();
    }
}
