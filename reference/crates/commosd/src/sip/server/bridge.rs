//! The media plane: echo, two-leg bridging, ring-group fork, trunk bridge, recording.

use super::*;

impl SipServer {
    /// Answer an INVITE with a plain RTP echo path (the IVR fallback). One UDP socket
    /// reflecting RTP back to the caller.
    pub(super) async fn answer_with_echo(
        &self,
        resp: &Responder,
        msg: &SipMessage,
        call_id: Uuid,
        call_id_hdr: &str,
    ) -> std::io::Result<()> {
        let (rtp_port, task) = match rtp::bind_echo(None, None).await {
            Ok((port, task)) => (port, Some(task)),
            Err(e) => {
                tracing::warn!(error = %e, "could not bind RTP; answering without media");
                (0, None)
            }
        };
        // Reflect the caller's preferred codec (the echo path relays bytes verbatim).
        let offer = codec::AudioMedia::parse(&String::from_utf8_lossy(msg.body()));
        let audio = offer.preferred_audio().unwrap_or_else(default_codec);
        let te_pt = offer
            .telephone_event_pt()
            .unwrap_or(dtmf::TELEPHONE_EVENT_PT);
        let sdp = self.build_sdp(rtp_port, &audio, te_pt, None);
        if let Some(task) = task {
            if !call_id_hdr.is_empty() {
                self.dialogs.lock().expect("dialogs mutex").insert(
                    call_id_hdr.to_string(),
                    Dialog {
                        call_id,
                        media: Media::Echo(task),
                        callee: None,
                        caller: None,
                        capture: None,
                        voicemail: None,
                        info_tx: None,
                        answer_sdp: sdp.clone(),
                    },
                );
            } else {
                task.abort();
            }
        }
        self.mark_answered(call_id).await;
        let ok = self.build_invite_ok(msg, &sdp, call_id);
        resp.send(ok.as_bytes()).await
    }

    /// Execute a resolved [`DialPlan`](crate::control::ringplan::DialPlan)'s ring stages,
    /// reusing the single-leg bridge/trunk primitives, and return the first leg that answers
    /// (`None` → nobody answered, so the caller applies the plan's `FinalAction`).
    ///
    /// Within a stage, all registered internal members are rung **simultaneously** (a true
    /// ring-all fork — parallel INVITEs, first 2xx wins, losers `CANCEL`led via
    /// [`Self::fork_bridge`]); any external (trunk) members in the same stage are then tried in
    /// order. Stages themselves run in order, so a hunt group / follow-me chain (one contact
    /// per stage) still rings its members one at a time. This makes `RING_ALL` truly
    /// simultaneous while keeping `SEQUENTIAL`/`ROUND_ROBIN`/`RANDOM`/follow-me exact.
    #[allow(clippy::type_complexity)]
    pub(super) async fn execute_ring_plan(
        &self,
        plan: &crate::control::ringplan::DialPlan,
        call_id: Uuid,
        capture: Option<rtp::Capture>,
        offer: &codec::AudioMedia,
        caller_crypto: Option<&sdes::CryptoAttr>,
        caller_id: CallerId<'_>,
    ) -> Option<(
        rtp::Bridge,
        CalleeLeg,
        codec::Codec,
        u8,
        Option<sdes::CryptoAttr>,
    )> {
        for stage in &plan.stages {
            // Split the stage into registered internal endpoints (rung simultaneously) and
            // external trunk targets (rung in order after).
            let mut regs = Vec::new();
            let mut externals = Vec::new();
            for contact in &stage.contacts {
                match find_registered(&self.registrations, self.default_tenant, contact) {
                    Some(reg) => regs.push(reg),
                    None => externals.push(
                        contact
                            .strip_prefix("external:")
                            .unwrap_or(contact)
                            .to_string(),
                    ),
                }
            }
            if !regs.is_empty() {
                if let Some(won) = self
                    .fork_bridge(
                        &regs,
                        call_id,
                        capture.clone(),
                        offer,
                        caller_crypto,
                        caller_id,
                    )
                    .await
                {
                    return Some(won);
                }
            }
            for e164 in &externals {
                if let Some((gw, num)) = self.select_outbound_gateway(e164).await {
                    if let Some(won) = self
                        .try_trunk(&gw, &num, call_id, capture.clone(), offer, caller_crypto)
                        .await
                    {
                        return Some(won);
                    }
                }
            }
        }
        None
    }

    /// Ring every registered member of `regs` **simultaneously** and return the first that
    /// answers, `CANCEL`ling the rest.
    ///
    /// A single member is just a plain [`Self::try_bridge`] (no fork machinery). For several,
    /// one `try_bridge` future per member is driven concurrently on this task (no `spawn`, so
    /// the borrowed caller state needs no `'static`); [`std::future::poll_fn`] races them. The
    /// first `Some` wins; the losers are then signalled to cancel and drained to completion so
    /// each sends its SIP `CANCEL`. A member that answered in the same instant the winner did
    /// (a fork glare) is torn down with a `BYE`, so no caller is left connected to a ghost leg.
    #[allow(clippy::type_complexity)]
    pub(super) async fn fork_bridge(
        &self,
        regs: &[Registration],
        call_id: Uuid,
        capture: Option<rtp::Capture>,
        offer: &codec::AudioMedia,
        caller_crypto: Option<&sdes::CryptoAttr>,
        caller_id: CallerId<'_>,
    ) -> Option<(
        rtp::Bridge,
        CalleeLeg,
        codec::Codec,
        u8,
        Option<sdes::CryptoAttr>,
    )> {
        if regs.len() == 1 {
            // Ring groups fold a decline into "no member answered" (a single member declining
            // shouldn't speak for the whole group); only the direct-call path acts on Declined.
            return match self
                .try_bridge(
                    &regs[0],
                    call_id,
                    capture,
                    offer,
                    caller_crypto,
                    caller_id,
                    None,
                )
                .await
            {
                BridgeOutcome::Answered(bridge, leg, codec, te, crypto) => {
                    Some((bridge, leg, codec, te, crypto))
                }
                BridgeOutcome::Declined(_) | BridgeOutcome::NoAnswer => None,
            };
        }

        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        #[allow(clippy::type_complexity)]
        let mut futs: Vec<
            std::pin::Pin<Box<dyn std::future::Future<Output = BridgeOutcome> + Send + '_>>,
        > = regs
            .iter()
            .map(|reg| {
                let rx = cancel_rx.clone();
                let cap = capture.clone();
                Box::pin(self.try_bridge(
                    reg,
                    call_id,
                    cap,
                    offer,
                    caller_crypto,
                    caller_id,
                    Some(rx),
                ))
                    as std::pin::Pin<Box<dyn std::future::Future<Output = BridgeOutcome> + Send>>
            })
            .collect();

        // Race all legs on this task. First `Answered` wins; a leg that declines or fails drops
        // out and the rest keep ringing.
        let winner = std::future::poll_fn(|cx| {
            let mut i = 0;
            while i < futs.len() {
                match futs[i].as_mut().poll(cx) {
                    std::task::Poll::Ready(BridgeOutcome::Answered(
                        bridge,
                        leg,
                        codec,
                        te,
                        crypto,
                    )) => {
                        drop(futs.swap_remove(i)); // drop the completed winner so it is not re-polled
                        return std::task::Poll::Ready(Some((bridge, leg, codec, te, crypto)));
                    }
                    std::task::Poll::Ready(_) => {
                        drop(futs.swap_remove(i)); // this member is out; a new one now sits at i
                    }
                    std::task::Poll::Pending => i += 1,
                }
            }
            if futs.is_empty() {
                std::task::Poll::Ready(None)
            } else {
                std::task::Poll::Pending
            }
        })
        .await;

        // Tell the losers to give up, then drive each to completion so it actually sends its
        // CANCEL. Any leg that had already answered (glare) is torn down with a BYE.
        let _ = cancel_tx.send(true);
        for f in futs.drain(..) {
            if let BridgeOutcome::Answered(bridge, leg, _, _, _) = f.await {
                bridge.abort();
                self.send_bye_to_leg(&leg).await;
            }
        }
        winner
    }

    /// Build the music-on-hold loop for a bridge whose legs negotiated `codec`, transcoded to
    /// that codec's G.711 flavour and ready for the relay to packetise while a leg is on hold.
    /// `None` when hold music is disabled or the negotiated codec isn't G.711.
    pub(super) fn moh_loop_for(&self, codec: &codec::Codec) -> Option<rtp::MohLoop> {
        if !self.music_on_hold {
            return None;
        }
        let g = g711::G711::from_name(&codec.name)?;
        Some(rtp::MohLoop {
            audio: self.moh.for_codec(g),
            payload_type: codec.pt,
        })
    }

    /// Best-effort outbound (UAC) INVITE to a registered callee, bridged to the caller.
    ///
    /// Binds a two-leg [`rtp::Bridge`], sends an INVITE offering leg B to the callee's
    /// contact over a **dedicated** UDP socket (so it never contends with the main ingress
    /// loop), waits (skipping 1xx) for a 2xx up to the configured no-answer timeout, ACKs it, and
    /// returns the live bridge plus the callee-leg dialog state. Returns `None` — after
    /// aborting the bridge — on any failure (unresolvable contact, no answer, rejection).
    ///
    /// The relay latches onto each side's RTP source address from its first packet, so the
    /// callee's advertised SDP address is not required for media to flow.
    ///
    /// When `cancel` is supplied (the simultaneous ring-all fork), a change on that receiver
    /// while the callee is still ringing makes this leg send a SIP `CANCEL` for its INVITE
    /// transaction and return `None` — so the losing members' phones stop ringing the instant
    /// another member answers. A `None` cancel keeps the exact original single-leg behaviour.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn try_bridge(
        &self,
        callee: &Registration,
        call_id: Uuid,
        capture: Option<rtp::Capture>,
        offer: &codec::AudioMedia,
        caller_crypto: Option<&sdes::CryptoAttr>,
        caller_id: CallerId<'_>,
        cancel: Option<tokio::sync::watch::Receiver<bool>>,
    ) -> BridgeOutcome {
        let pending = match rtp::bind_bridge_sockets().await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "could not bind RTP bridge");
                return BridgeOutcome::NoAnswer;
            }
        };

        // Leg A (caller): when the caller offered SRTP, key our side of it and remember the
        // `a=crypto` to answer the caller with. When the caller leg is encrypted, extend SRTP to
        // the callee (leg B) too by offering it a fresh key — end-to-end, both legs encrypted.
        let (leg_a_crypto, srtp_a) = match caller_crypto {
            Some(c) => {
                let (attr, session) = srtp_answer(c);
                (Some(attr), Some(session))
            }
            None => (None, None),
        };
        let legb_offer = leg_a_crypto.as_ref().map(|_| srtp::random_key_salt());
        let legb_crypto = legb_offer.map(|ks| sdes::CryptoAttr {
            tag: 1,
            key_salt: ks,
        });

        let addr = match resolve_contact_addr(&callee.contact).await {
            Some(a) => a,
            None => {
                tracing::warn!(contact = %callee.contact, "callee contact is unresolvable");
                return BridgeOutcome::NoAnswer;
            }
        };

        let sock = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "could not bind outbound SIP socket");
                return BridgeOutcome::NoAnswer;
            }
        };

        // Our reachable sent-by for the outbound leg: `media_ip` at the ephemeral port we send from
        // and await the response on. The callee returns its 100/180/200 here (see `via_header`);
        // without a reachable Via the answer is lost and the call wrongly diverts to voicemail.
        let sent_by = SocketAddr::new(
            self.media_ip,
            sock.local_addr().map(|a| a.port()).unwrap_or(0),
        );

        // Outbound-leg dialog identifiers, derived from the CommOS Call id.
        let leg_call_id = format!("{}@commos", call_id.to_string().replace('-', ""));
        let from_tag: String = call_id
            .to_string()
            .chars()
            .filter(|c| *c != '-')
            .take(16)
            .collect();
        // Present the *caller's* identity to the callee (its number + display name) so the callee's
        // phone shows who is calling. When the caller is anonymous, fall back to the operator's
        // configurable display name (or the bare "commos") via `caller_from_header`.
        let display = match caller_id.display {
            Some(_) => caller_id.display.map(str::to_string),
            None => self.call_display_name(call_id).await,
        };
        let from_hdr = caller_from_header(
            self.media_ip,
            caller_id.number,
            display.as_deref(),
            &from_tag,
        );
        let contact_hdr = format!("<sip:commos@{}>", self.media_ip);
        let cseq_num: u32 = 1;

        // Offer the caller's full codec list to the callee on leg B, so the two ends converge on
        // a shared codec CommOS relays untouched (transparent pass-through, no transcoding), plus
        // an SDES key when the caller leg is encrypted.
        let sdp = reoffer_sdp(
            self.media_ip,
            pending.leg_b_port,
            offer,
            legb_crypto.as_ref(),
        );
        // Build the top Via once and reuse it for the INVITE and (if cancelled) the CANCEL, so
        // the CANCEL matches the INVITE client transaction by branch (RFC 3261 §9.1). The ACK
        // for a 2xx is a fresh transaction and keeps its own branch.
        let via = message::via_header(sent_by);
        let invite = message::request(
            "INVITE",
            &callee.contact,
            &[
                ("Via", via.clone()),
                ("From", from_hdr.clone()),
                ("To", format!("<{}>", callee.aor)),
                ("Call-ID", leg_call_id.clone()),
                ("CSeq", format!("{cseq_num} INVITE")),
                ("Contact", contact_hdr),
            ],
            Some(("application/sdp", &sdp)),
        );

        // Send the INVITE and wait for the callee's final answer, retransmitting until it responds
        // (RFC 3261 client transaction); a non-2xx (or no answer) falls back to voicemail/echo.
        // In the fork case, a cancel signal instead sends a CANCEL for this transaction.
        let resp = match cancel {
            None => {
                send_invite_await_final(&sock, invite.as_bytes(), addr, self.no_answer_timeout)
                    .await
            }
            Some(rx) => {
                let cancel_req = message::request(
                    "CANCEL",
                    &callee.contact,
                    &[
                        ("Via", via.clone()),
                        ("From", from_hdr.clone()),
                        ("To", format!("<{}>", callee.aor)),
                        ("Call-ID", leg_call_id.clone()),
                        ("CSeq", format!("{cseq_num} CANCEL")),
                    ],
                    None,
                )
                .into_bytes();
                send_invite_await_final_cancellable(
                    &sock,
                    invite.as_bytes(),
                    addr,
                    self.no_answer_timeout,
                    rx,
                    cancel_req,
                )
                .await
            }
        };
        let resp = match resp {
            Some(r) if (200..300).contains(&r.status().unwrap_or(0)) => r,
            // Active rejection: the callee pressed Decline/Reject (486 Busy Here / 600 Busy
            // Everywhere / 603 Decline). Carry the code so the caller-facing path can react.
            Some(r) if matches!(r.status(), Some(486) | Some(600) | Some(603)) => {
                return BridgeOutcome::Declined(r.status().unwrap_or(603));
            }
            // No answer within the ring timeout, a cancelled fork leg, or any other non-2xx.
            _ => return BridgeOutcome::NoAnswer,
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
                ("Via", message::via_header(sent_by)),
                ("From", from_hdr.clone()),
                ("To", callee_to.clone()),
                ("Call-ID", leg_call_id.clone()),
                ("CSeq", format!("{cseq_num} ACK")),
            ],
            None,
        );
        let _ = sock.send_to(ack.as_bytes(), addr).await;

        // The callee's chosen codec (from its 200 SDP) is what we answer the *caller* with, so
        // both legs use it and the relay is byte-transparent. Fall back to the caller's preferred
        // codec if the callee's answer is unparseable.
        let callee_body = String::from_utf8_lossy(resp.body());
        let callee_answer = codec::AudioMedia::parse(&callee_body);
        let sel_codec = callee_answer
            .preferred_audio()
            .or_else(|| offer.preferred_audio())
            .unwrap_or_else(default_codec);
        let sel_te = callee_answer
            .telephone_event_pt()
            .or_else(|| offer.telephone_event_pt())
            .unwrap_or(dtmf::TELEPHONE_EVENT_PT);

        // Leg B (callee): if we offered SRTP and the callee answered with its own SDES key, key the
        // callee side too. Otherwise leg B is plaintext (a callee that declined SRTP).
        let srtp_b = legb_offer.as_ref().and_then(|offered| {
            sdes::CryptoAttr::from_sdp(&callee_body).map(|k| srtp_offered(offered, &k))
        });
        if leg_a_crypto.is_some() {
            tracing::info!(%call_id, leg_b_encrypted = srtp_b.is_some(),
                "SRTP bridge: caller leg encrypted; callee leg {}",
                if srtp_b.is_some() { "encrypted" } else { "plaintext (callee declined)" });
        }
        let bridge = pending.start(capture, srtp_a, srtp_b, self.moh_loop_for(&sel_codec));

        let leg = CalleeLeg {
            addr,
            request_uri: callee_target,
            from: from_hdr,
            to: callee_to,
            call_id: leg_call_id,
            cseq: cseq_num,
        };
        BridgeOutcome::Answered(bridge, leg, sel_codec, sel_te, leg_a_crypto)
    }

    /// Place an **outbound** call to the PSTN/SIP carrier via `gateway`, bridged to the caller.
    ///
    /// Binds a two-leg [`rtp::Bridge`], sends an INVITE to the gateway for `sip:<e164>@<gateway>`
    /// offering leg B, and — if the carrier challenges with `401`/`407` — retries once with a
    /// digest `Authorization`/`Proxy-Authorization` computed from the carrier's [`Trunk`] auth.
    /// On a 2xx it ACKs and returns the live bridge + callee-leg state (so a BYE tears the trunk
    /// leg down). Returns `None` (after aborting the bridge) on any failure.
    ///
    /// TODO(B2BUA): the outbound leg is best-effort like [`Self::try_bridge`]; full transaction
    /// state / retransmission and codec negotiation with the carrier are future work.
    pub(super) async fn try_trunk(
        &self,
        gateway: &Gateway,
        e164: &str,
        call_id: Uuid,
        capture: Option<rtp::Capture>,
        offer: &codec::AudioMedia,
        caller_crypto: Option<&sdes::CryptoAttr>,
    ) -> Option<(
        rtp::Bridge,
        CalleeLeg,
        codec::Codec,
        u8,
        Option<sdes::CryptoAttr>,
    )> {
        let gw_address = gateway.address.as_deref()?;
        let addr = match resolve_contact_addr(gw_address).await {
            Some(a) => a,
            None => {
                tracing::warn!(gateway = %gw_address, "outbound trunk: gateway address unresolvable");
                return None;
            }
        };
        let pending = rtp::bind_bridge_sockets().await.ok()?;

        // Leg A (caller) SRTP. The carrier (leg B) is offered SRTP only when `trunk_srtp` is on:
        // a carrier that can't answer RTP/SAVP would reject the call, so by default the trunk leg
        // stays plaintext (the caller's access leg is still encrypted) and the call always connects.
        let (leg_a_crypto, srtp_a) = match caller_crypto {
            Some(c) => {
                let (attr, session) = srtp_answer(c);
                (Some(attr), Some(session))
            }
            None => (None, None),
        };
        let legb_offer = (self.trunk_srtp && leg_a_crypto.is_some()).then(srtp::random_key_salt);
        let legb_crypto = legb_offer.map(|ks| sdes::CryptoAttr {
            tag: 1,
            key_salt: ks,
        });

        let sock = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "outbound trunk: could not bind SIP socket");
                return None;
            }
        };

        // Our reachable sent-by (see `try_bridge`): the carrier returns its responses here.
        let sent_by = SocketAddr::new(
            self.media_ip,
            sock.local_addr().map(|a| a.port()).unwrap_or(0),
        );

        // Request-URI toward the carrier, and outbound-leg dialog identifiers.
        let request_uri = format!("sip:{e164}@{gw_address}");
        let leg_call_id = format!("{}@commos-trunk", call_id.to_string().replace('-', ""));
        let from_tag: String = call_id
            .to_string()
            .chars()
            .filter(|c| *c != '-')
            .take(16)
            .collect();
        let from_hdr = format!("<sip:commos@{}>;tag={from_tag}", self.media_ip);
        let contact_hdr = format!("<sip:commos@{}>", self.media_ip);
        let cnonce = from_tag.clone();
        // Offer the caller's codec list to the carrier (transparent pass-through), plus an SDES
        // key when the caller leg is encrypted.
        let sdp = reoffer_sdp(
            self.media_ip,
            pending.leg_b_port,
            offer,
            legb_crypto.as_ref(),
        );
        let creds = self.trunk_credentials(gateway.carrier_id).await;

        // Send the INVITE (retransmitting until the carrier responds), retrying once with digest
        // auth if it challenges.
        let mut auth: Option<(&str, String)> = None;
        let mut resp = None;
        for cseq in 1u32..=2 {
            let via = message::via_header(sent_by);
            let mut headers = vec![
                ("Via", via),
                ("From", from_hdr.clone()),
                ("To", format!("<{request_uri}>")),
                ("Call-ID", leg_call_id.clone()),
                ("CSeq", format!("{cseq} INVITE")),
                ("Contact", contact_hdr.clone()),
            ];
            if let Some((name, value)) = &auth {
                headers.push((name, value.clone()));
            }
            let invite = message::request(
                "INVITE",
                &request_uri,
                &headers,
                Some(("application/sdp", &sdp)),
            );
            let msg = match send_invite_await_final(
                &sock,
                invite.as_bytes(),
                addr,
                self.no_answer_timeout,
            )
            .await
            {
                Some(m) => m,
                None => return None,
            };
            let status = msg.status().unwrap_or(0);
            if (200..300).contains(&status) {
                resp = Some(msg);
                break;
            }
            // A challenge on the first attempt → compute digest auth and retry.
            if (status == 401 || status == 407) && auth.is_none() {
                let (chdr, ahdr) = if status == 407 {
                    ("Proxy-Authenticate", "Proxy-Authorization")
                } else {
                    ("WWW-Authenticate", "Authorization")
                };
                match (
                    msg.header(chdr)
                        .and_then(crate::sip::digest::parse_challenge),
                    &creds,
                ) {
                    (Some(challenge), Some((user, pass))) => {
                        let value = crate::sip::digest::authorization_value(
                            user,
                            pass,
                            "INVITE",
                            &request_uri,
                            &challenge,
                            &cnonce,
                        );
                        auth = Some((ahdr, value));
                        continue;
                    }
                    _ => {
                        tracing::warn!(gateway = %gw_address, status, "outbound trunk: auth required but no usable trunk credentials");
                        return None;
                    }
                }
            }
            tracing::info!(gateway = %gw_address, status, "outbound trunk: carrier rejected the call");
            return None;
        }
        let resp = resp?;

        let callee_to = resp
            .header("To")
            .map(str::to_string)
            .unwrap_or_else(|| format!("<{request_uri}>"));
        let callee_target = resp
            .header("Contact")
            .and_then(extract_uri)
            .unwrap_or_else(|| request_uri.clone());
        let ack_cseq = if auth.is_some() { 2 } else { 1 };
        let mut ack_headers = vec![
            ("Via", message::via_header(sent_by)),
            ("From", from_hdr.clone()),
            ("To", callee_to.clone()),
            ("Call-ID", leg_call_id.clone()),
            ("CSeq", format!("{ack_cseq} ACK")),
        ];
        if let Some((name, value)) = &auth {
            ack_headers.push((name, value.clone()));
        }
        let ack = message::request("ACK", &callee_target, &ack_headers, None);
        let _ = sock.send_to(ack.as_bytes(), addr).await;
        // The carrier's chosen codec is what we answer the caller with (transparent relay).
        let carrier_body = String::from_utf8_lossy(resp.body());
        let carrier_answer = codec::AudioMedia::parse(&carrier_body);
        let sel_codec = carrier_answer
            .preferred_audio()
            .or_else(|| offer.preferred_audio())
            .unwrap_or_else(default_codec);
        let sel_te = carrier_answer
            .telephone_event_pt()
            .or_else(|| offer.telephone_event_pt())
            .unwrap_or(dtmf::TELEPHONE_EVENT_PT);

        // Leg B (carrier) SRTP, if offered and the carrier answered with its own SDES key.
        let srtp_b = legb_offer.as_ref().and_then(|offered| {
            sdes::CryptoAttr::from_sdp(&carrier_body).map(|k| srtp_offered(offered, &k))
        });
        tracing::info!(%call_id, gateway = %gw_address, %e164, leg_b_port = pending.leg_b_port,
            codec = %sel_codec.name, srtp = leg_a_crypto.is_some(), carrier_srtp = srtp_b.is_some(),
            "outbound trunk: call placed to carrier");
        let bridge = pending.start(capture, srtp_a, srtp_b, self.moh_loop_for(&sel_codec));

        let leg = CalleeLeg {
            addr,
            request_uri: callee_target,
            from: from_hdr,
            to: callee_to,
            call_id: leg_call_id,
            cseq: ack_cseq,
        };
        Some((bridge, leg, sel_codec, sel_te, leg_a_crypto))
    }

    /// Mid-dialog BYE toward one leg of a bridged call, sent as a reliable non-INVITE
    /// transaction: retransmitted until the endpoint confirms with a final response (or the
    /// transaction times out), so a lost BYE still tears that leg down. Used for both directions —
    /// the callee leg (caller hung up) and the caller leg (callee hung up).
    ///
    /// TODO(B2BUA): this reconstructs the BYE from captured identifiers only; full RFC 3261
    /// mid-dialog correctness (route sets, contact refresh) is still out of scope.
    pub(super) async fn send_bye_to_leg(&self, leg: &CalleeLeg) {
        bye_leg(self.media_ip, leg).await;
    }

    /// Persist captured caller audio as a call [`Recording`], fire-and-forget off the BYE path.
    pub(super) fn spawn_save_recording(&self, call_id: Uuid, bytes: Vec<u8>) {
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
}
