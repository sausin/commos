//! The inbound INVITE handler — the core call-routing decision for [`SipServer`].

use super::*;

impl SipServer {
    /// INVITE: create an inbound Call, report ring+answer facts, set up RTP echo, answer
    /// `200 OK` with an SDP answer.
    pub(super) async fn on_invite(
        &self,
        resp: &Responder,
        msg: &SipMessage,
    ) -> std::io::Result<()> {
        let src = resp.peer();
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

        // In-dialog INVITE: a retransmission (our 200 OK was lost) or a re-INVITE (media refresh /
        // hold) for a dialog we already answered. Re-answer 200 OK from the stored media — echoing
        // the incoming INVITE's headers/CSeq — without creating a duplicate Call or re-binding
        // media. (Full re-negotiation of a hold's media direction is future B2BUA work.)
        let existing = {
            let dialogs = self.dialogs.lock().expect("dialogs mutex");
            dialogs.get(&call_id_hdr).map(|d| {
                // A hold/resume re-INVITE on a live bridge drives music-on-hold. The party that
                // sent this re-INVITE is the caller (leg A of this dialog), so on hold the callee
                // (leg B) hears MoH; on resume the two-way relay comes back. A plain retransmit
                // (no direction change) leaves the state untouched.
                if let Media::Bridge(bridge) = &d.media {
                    match hold_direction(&String::from_utf8_lossy(msg.body())) {
                        Some(true) => bridge.set_hold(rtp::HoldState::Held(rtp::Leg::A)),
                        Some(false) => bridge.set_hold(rtp::HoldState::Active),
                        None => {}
                    }
                }
                (d.call_id, d.answer_sdp.clone())
            })
        };
        if let Some((dialog_call_id, answer_sdp)) = existing {
            tracing::info!(%dialog_call_id, %src, "SIP re-INVITE / retransmit → replaying answer");
            let ok = self.build_invite_ok(msg, &answer_sdp, dialog_call_id);
            return resp.send(ok.as_bytes()).await;
        }

        // Digest auth gate: an unauthenticated INVITE is challenged with 401 before any Call is
        // created; the phone re-sends with credentials. (REGISTER auth already limits who is
        // reachable; challenging INVITE too stops direct unauthenticated dialing.) Enforced when
        // configured, and always for a public source address.
        if self.auth_required_from(src) {
            match self.digest_ok(msg, "INVITE").await {
                Some(user) => {
                    // Caller-identity binding: the From user-part must match the authenticated
                    // user, so a device cannot originate calls (CDRs, trunk/PSTN caller-ID) under
                    // a spoofed identity.
                    let from_user = user_part(&from_ref);
                    if from_user
                        .map(|u| !u.eq_ignore_ascii_case(&user))
                        .unwrap_or(true)
                    {
                        tracing::warn!(
                            %src, authed = %user, from = %from_ref,
                            "SIP INVITE rejected: From identity does not match the authenticated user"
                        );
                        return self.reply(resp, msg, 403, "Forbidden").await;
                    }
                }
                None => {
                    tracing::info!(%src, "SIP INVITE challenged (digest auth required)");
                    return self.send_challenge(resp, msg).await;
                }
            }
        }

        // Provisional response.
        let trying = message::response(msg, 100, "Trying");
        resp.send(trying.as_bytes()).await?;

        // Create the inbound Call in the control plane. Clone `from_ref` so the caller's identity
        // remains available below (e.g. to resolve the caller's own mailbox for `*97`).
        let call = match self
            .routing
            .create_inbound_call(self.default_tenant, from_ref.clone(), to_ref.clone())
            .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "INVITE could not create Call");
                return self.reply(resp, msg, 500, "Server Internal Error").await;
            }
        };
        let call_id = call.base.id;

        // When recording is on, all RTP payload for this call accumulates in this shared buffer
        // (caller leg, header-stripped, as-is). It is threaded into whichever media path is set
        // up below and drained into a Recording on hangup.
        let capture: Option<rtp::Capture> = if self.record_calls {
            Some(Arc::new(Mutex::new(Vec::new())))
        } else {
            None
        };

        // SIP is the media plane here: report the ring now. The Call goes to RINGING (a fast BYE
        // while ringing is still a legal hang-up: Ringing → Ended). The ANSWERED fact is applied
        // later, at the moment CommOS actually answers the caller with 200 OK — for a bridged call
        // that is when the callee really picks up, so `answered_at` (and the billed duration) is
        // the true connect time, not the INVITE-receipt time.
        if let Err(e) = self
            .routing
            .apply_fact(MediaFact::Rang {
                tenant_id: self.default_tenant,
                call_id,
            })
            .await
        {
            tracing::warn!(error = %e, %call_id, "applying SIP ring fact failed");
        }

        // Route the dialled number through the Extension→Route table (control-plane routing,
        // Volume 3). A route to a SIP endpoint rewrites the effective target so the registered
        // callee is found by the route's user-part, not just a bare request-URI match; a
        // queue/external destination has no registered endpoint and falls through to echo.
        let request_uri = msg.request_uri().unwrap_or("");
        let routed_uri = self.resolve_route(request_uri).await;
        let effective_uri = routed_uri.as_deref().unwrap_or(request_uri);

        // Negotiate codecs from the caller's SDP offer (CMOS-07-SIP-041). `te_pt` is the DTMF
        // payload type; `g711` is the G.711 variant CommOS synthesises where it is itself the
        // endpoint (IVR/voicemail); `reflect` is the caller's preferred codec, answered verbatim
        // on the echo path (byte-transparent). Bridge/trunk pass the whole offer through.
        let body = String::from_utf8_lossy(msg.body()).into_owned();
        let offer = codec::AudioMedia::parse(&body);
        let te_pt = offer
            .telephone_event_pt()
            .unwrap_or(dtmf::TELEPHONE_EVENT_PT);
        let g711 = offer
            .select_g711()
            .and_then(|c| g711::G711::from_name(&c.name))
            .unwrap_or(g711::G711::Ulaw);
        let g711_codec = codec::Codec {
            pt: g711.payload_type(),
            name: g711.sdp_name().to_string(),
            clock: 8000,
        };
        let reflect = offer.preferred_audio().unwrap_or_else(default_codec);
        tracing::info!(%call_id, endpoint_codec = %g711.sdp_name(), reflect_codec = %reflect.name, te_pt, "codecs negotiated");

        // Whether the INVITE arrived over a confidential (TLS) transport. SDES SRTP keying is
        // only honoured on a secure transport — see `caller_crypto` — so the media key is never
        // exposed in cleartext SDP over plain UDP.
        let secure = resp.is_secure();
        if !secure && self.srtp_enabled && sdes::offers_savp(&body) {
            tracing::warn!(%call_id, %src,
                "caller offered SRTP/SAVP over a non-TLS transport; answering plain RTP (SDES key \
                 would otherwise be exposed in cleartext SDP). Use SIPS to enable SRTP.");
        }

        // The caller's SDES key, if it offered SRTP over a secure transport — used to key the
        // caller (leg A) side of a bridge/trunk and to answer the caller over RTP/SAVP.
        let caller_crypto = self.caller_crypto(&body, secure);

        // Inbound DID: an INVITE from a carrier to a provisioned external number is routed to its
        // `destination_ref`. The effective target is that DID destination if matched, else the
        // extension route, else the raw request-URI.
        let did_dest = self.resolve_did(request_uri).await;
        if did_dest.is_some() {
            tracing::info!(%call_id, number = %request_uri, dest = ?did_dest, "inbound DID routed");
        }
        let target: String = did_dest
            .clone()
            .unwrap_or_else(|| effective_uri.to_string());

        // Feature codes for voicemail retrieval (the "voicemail button" / dial-in). Handled here
        // in the SIP layer like `ivr:`/`voicemail` targets — no dialplan entry needed:
        //   *97 — listen to your OWN mailbox (the caller's extension),
        //   *98 — listen to ANOTHER mailbox (the extension is entered via DTMF).
        let dialed = user_part(request_uri).unwrap_or("");
        if dialed == "*97" || dialed == "*98" {
            let own_mailbox = if dialed == "*97" {
                user_part(&from_ref).map(str::to_string)
            } else {
                None
            };
            return self
                .answer_with_voicemail_retrieval(
                    resp,
                    msg,
                    call_id,
                    &call_id_hdr,
                    g711,
                    te_pt,
                    own_mailbox,
                )
                .await;
        }

        // The target (from a DID or an extension route) may name an `ivr:<id>` menu: run the IVR
        // runtime (answer with SDP, play the prompt, collect DTMF).
        if let Some(ivr_id) = match ivr_id_of(&target) {
            Some(id) => Some(id),
            None => self.resolve_ivr_target(request_uri).await,
        } {
            return self
                .answer_with_ivr(resp, msg, call_id, &call_id_hdr, ivr_id, g711, te_pt)
                .await;
        }

        // A `queue:<uuid>` target → answer immediately and hand to the queue-wait treatment loop
        // (greeting + music-on-hold + announcements while placing the caller with a member).
        if let Some(queue_id) = target
            .strip_prefix("queue:")
            .and_then(|s| Uuid::parse(s.trim()).ok())
        {
            return self
                .answer_with_queue(resp, msg, call_id, &call_id_hdr, queue_id, g711, te_pt)
                .await;
        }

        let mut voicemail_target: Option<VoicemailBox> = None;
        // A direct voicemail target (e.g. a DID → "voicemail").
        if target == "voicemail" || target.starts_with("voicemail:") {
            voicemail_target = Some(VoicemailBox {
                aor: target.clone(),
                notify: None,
            });
        }

        // Multi-destination routing: a ring group (`ringgroup:<uuid>`) or an active forwarding
        // rule for the dialled number is executed as a resolved DialPlan (fan-out / follow-me),
        // reusing the single-leg bridge/trunk primitives. This branch engages ONLY when there is
        // genuinely a group or a forwarding rule in play; the plain single-extension bridge path
        // below is left completely untouched otherwise.
        if voicemail_target.is_none() {
            let dialled = user_part(&to_ref)
                .map(|s| s.to_string())
                .unwrap_or_default();
            let is_group = target.starts_with(crate::control::ringresolve::RING_GROUP_SCHEME);
            let has_forwarding = !dialled.is_empty()
                && crate::control::ringresolve::active_forwarding(
                    &self.store,
                    self.default_tenant,
                    &dialled,
                )
                .await
                .is_some();
            if is_group || has_forwarding {
                // Tell the caller's phone we're ringing while we walk the plan.
                let _ = resp.send(self.build_ringing(msg, call_id).as_bytes()).await;
                let caller_display = msg.header("From").and_then(header_display_name);
                let caller_id = CallerId {
                    number: user_part(&from_ref),
                    display: caller_display.as_deref(),
                };
                let opts = crate::control::ringplan::PlanOpts {
                    default_ring_seconds: self.no_answer_timeout.as_secs().max(1) as u32,
                    voicemail_enabled: self.voicemail_enabled,
                };
                let rotation = self
                    .ring_rotation
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let regs = &self.registrations;
                let tenant = self.default_tenant;
                let plan = crate::control::ringresolve::resolve_plan(
                    &self.store,
                    tenant,
                    &dialled,
                    &target,
                    opts,
                    rotation,
                    |r: &str| find_registered(regs, tenant, r).is_some(),
                )
                .await;

                match self
                    .execute_ring_plan(
                        &plan,
                        call_id,
                        capture.clone(),
                        &offer,
                        caller_crypto.as_ref(),
                        caller_id,
                    )
                    .await
                {
                    Some((bridge, callee_leg, sel_codec, sel_te, leg_a_crypto)) => {
                        let leg_a_port = bridge.leg_a_port;
                        let sdp =
                            self.build_sdp(leg_a_port, &sel_codec, sel_te, leg_a_crypto.as_ref());
                        if !call_id_hdr.is_empty() {
                            let callee_call_id = callee_leg.call_id.clone();
                            self.dialogs.lock().expect("dialogs mutex").insert(
                                call_id_hdr.clone(),
                                Dialog {
                                    call_id,
                                    media: Media::Bridge(bridge),
                                    callee: Some(callee_leg),
                                    caller: Some(self.caller_leg(msg, call_id, src)),
                                    capture: capture.clone(),
                                    voicemail: None,
                                    info_tx: None,
                                    answer_sdp: sdp.clone(),
                                },
                            );
                            self.bye_aliases
                                .lock()
                                .expect("aliases mutex")
                                .insert(callee_call_id, call_id_hdr.clone());
                        } else {
                            bridge.abort();
                        }
                        self.mark_answered(call_id).await;
                        let ok = self.build_invite_ok(msg, &sdp, call_id);
                        tracing::info!(%call_id, leg_a_port, dialled = %dialled, group = is_group,
                            "SIP INVITE bridged via ring plan");
                        return resp.send(ok.as_bytes()).await;
                    }
                    None => {
                        // Nobody answered → apply the plan's final action.
                        match &plan.final_action {
                            crate::control::ringplan::FinalAction::Voicemail(num)
                                if self.voicemail_enabled =>
                            {
                                voicemail_target = Some(VoicemailBox {
                                    aor: format!("sip:{num}"),
                                    notify: None,
                                });
                            }
                            crate::control::ringplan::FinalAction::Redirect(_)
                                if self.voicemail_enabled =>
                            {
                                // A redirect (queue / another group) is not yet re-resolved here;
                                // divert the dialled number to voicemail as the safe terminus.
                                voicemail_target = Some(VoicemailBox {
                                    aor: format!("sip:{dialled}"),
                                    notify: None,
                                });
                            }
                            _ => {}
                        }
                        // Fall through: a set voicemail_target is picked up by the deposit path;
                        // otherwise the echo fallthrough applies (voicemail disabled).
                    }
                }
            }
        }

        // If the target names a REGISTERED endpoint, bridge the two legs: bind a two-leg RTP
        // relay, INVITE the callee (offering leg B), and on its 200 OK answer the caller
        // (offering leg A). A callee that never answers (or an internal extension that is
        // offline) diverts to voicemail.
        if voicemail_target.is_none() {
            if let Some(callee_reg) =
                find_registered(&self.registrations, self.default_tenant, &target)
            {
                // Tell the caller's phone the callee is ringing (early dialog) so it shows a
                // "ringing" state instead of dead air while we ring the callee for real.
                let _ = resp.send(self.build_ringing(msg, call_id).as_bytes()).await;
                // Present the caller's own identity to the callee (number + display name), so the
                // callee's phone shows who is calling instead of the bare service identity.
                let caller_display = msg.header("From").and_then(header_display_name);
                let caller_id = CallerId {
                    number: user_part(&from_ref),
                    display: caller_display.as_deref(),
                };
                match self
                    .try_bridge(
                        &callee_reg,
                        call_id,
                        capture.clone(),
                        &offer,
                        caller_crypto.as_ref(),
                        caller_id,
                        None,
                    )
                    .await
                {
                    BridgeOutcome::Answered(
                        bridge,
                        callee_leg,
                        sel_codec,
                        sel_te,
                        leg_a_crypto,
                    ) => {
                        let leg_a_port = bridge.leg_a_port;
                        // Answer the caller with the codec the callee selected (transparent relay),
                        // plus our SRTP key when the caller leg is encrypted.
                        let sdp =
                            self.build_sdp(leg_a_port, &sel_codec, sel_te, leg_a_crypto.as_ref());
                        if !call_id_hdr.is_empty() {
                            let callee_call_id = callee_leg.call_id.clone();
                            self.dialogs.lock().expect("dialogs mutex").insert(
                                call_id_hdr.clone(),
                                Dialog {
                                    call_id,
                                    media: Media::Bridge(bridge),
                                    callee: Some(callee_leg),
                                    caller: Some(self.caller_leg(msg, call_id, src)),
                                    capture: capture.clone(),
                                    voicemail: None,
                                    info_tx: None,
                                    answer_sdp: sdp.clone(),
                                },
                            );
                            // Index the callee-leg Call-ID so a callee-side BYE finds this dialog.
                            self.bye_aliases
                                .lock()
                                .expect("aliases mutex")
                                .insert(callee_call_id, call_id_hdr.clone());
                        } else {
                            bridge.abort();
                        }
                        // The callee actually picked up: this is the true connect time.
                        self.mark_answered(call_id).await;
                        let ok = self.build_invite_ok(msg, &sdp, call_id);
                        tracing::info!(%call_id, leg_a_port, callee = %callee_reg.contact,
                            codec = %sel_codec.name, srtp = leg_a_crypto.is_some(), "SIP INVITE bridged to registered callee");
                        return resp.send(ok.as_bytes()).await;
                    }
                    // The callee actively declined (Decline/Reject button). Treat it per the
                    // operator's `on_decline` policy rather than folding it into "no answer".
                    BridgeOutcome::Declined(code) => match self.on_decline {
                        OnDecline::Busy => {
                            // Relay the busy/decline status: the caller's phone shows Busy/Declined
                            // and plays busy tone; no voicemail, no answer.
                            let (status, reason) = decline_status(code);
                            tracing::info!(%call_id, code, callee = %callee_reg.aor,
                                "callee declined; relaying {status} {reason} to caller");
                            return self.reply(resp, msg, status, reason).await;
                        }
                        OnDecline::Voicemail if self.voicemail_enabled => {
                            // Legacy behaviour: a decline diverts straight to voicemail, same as a
                            // no-answer.
                            let notify = resolve_contact_addr(&callee_reg.contact)
                                .await
                                .map(|addr| (addr, callee_reg.contact.clone()));
                            tracing::info!(%call_id, code, mailbox = %callee_reg.aor,
                                "callee declined; diverting to voicemail");
                            voicemail_target = Some(VoicemailBox {
                                aor: callee_reg.aor.clone(),
                                notify,
                            });
                        }
                        OnDecline::Voicemail => {
                            tracing::warn!(%call_id, code, callee = %callee_reg.contact,
                                "callee declined but voicemail is disabled; falling back to echo");
                        }
                        OnDecline::Announce => {
                            // Answer the caller, play an "unavailable" announcement, then offer to
                            // leave a message or drop the call.
                            tracing::info!(%call_id, code, callee = %callee_reg.aor,
                                "callee declined; announcing to caller");
                            return self
                                .answer_with_decline_announcement(
                                    resp,
                                    msg,
                                    call_id,
                                    &call_id_hdr,
                                    &callee_reg,
                                    g711,
                                    te_pt,
                                    src,
                                )
                                .await;
                        }
                    },
                    BridgeOutcome::NoAnswer if self.voicemail_enabled => {
                        // Rang but never answered → take a voicemail. MWI is pushed to the
                        // callee's registered contact on hangup.
                        let notify = resolve_contact_addr(&callee_reg.contact)
                            .await
                            .map(|addr| (addr, callee_reg.contact.clone()));
                        tracing::info!(%call_id, mailbox = %callee_reg.aor,
                            "registered callee did not answer; diverting to voicemail");
                        voicemail_target = Some(VoicemailBox {
                            aor: callee_reg.aor.clone(),
                            notify,
                        });
                    }
                    BridgeOutcome::NoAnswer => {
                        tracing::warn!(%call_id, callee = %callee_reg.contact,
                            "registered callee did not answer within timeout; falling back to echo");
                    }
                }
            } else if self.voicemail_enabled && (routed_uri.is_some() || did_dest.is_some()) {
                // The target is an internal endpoint (an extension route or a DID destination) but
                // nobody is registered for it — the mailbox owner is offline. Take a voicemail; its
                // MWI is delivered on the phone's next REGISTER.
                tracing::info!(%call_id, mailbox = %target, "internal endpoint is offline; diverting to voicemail");
                voicemail_target = Some(VoicemailBox {
                    aor: target.clone(),
                    notify: None,
                });
            }
        }

        // Outbound PSTN / SIP trunk: an external E.164 destination with a configured ONLINE SIP
        // gateway is placed to the carrier and relayed (reuses the two-leg bridge). A caller
        // dialling a real phone number reaches it. Falls through to echo if the trunk fails.
        if voicemail_target.is_none() {
            if let Some((gateway, e164)) = self.select_outbound_gateway(&target).await {
                // Signal ringing to the caller while we place the outbound leg to the carrier.
                let _ = resp.send(self.build_ringing(msg, call_id).as_bytes()).await;
                if let Some((bridge, leg, sel_codec, sel_te, leg_a_crypto)) = self
                    .try_trunk(
                        &gateway,
                        &e164,
                        call_id,
                        capture.clone(),
                        &offer,
                        caller_crypto.as_ref(),
                    )
                    .await
                {
                    let leg_a_port = bridge.leg_a_port;
                    // Answer the caller with the carrier's selected codec (transparent relay),
                    // plus our SRTP key when the caller leg is encrypted.
                    let sdp = self.build_sdp(leg_a_port, &sel_codec, sel_te, leg_a_crypto.as_ref());
                    if !call_id_hdr.is_empty() {
                        let callee_call_id = leg.call_id.clone();
                        self.dialogs.lock().expect("dialogs mutex").insert(
                            call_id_hdr.clone(),
                            Dialog {
                                call_id,
                                media: Media::Bridge(bridge),
                                callee: Some(leg),
                                caller: Some(self.caller_leg(msg, call_id, src)),
                                capture: capture.clone(),
                                voicemail: None,
                                info_tx: None,
                                answer_sdp: sdp.clone(),
                            },
                        );
                        self.bye_aliases
                            .lock()
                            .expect("aliases mutex")
                            .insert(callee_call_id, call_id_hdr.clone());
                    } else {
                        bridge.abort();
                    }
                    // The carrier answered: true connect time.
                    self.mark_answered(call_id).await;
                    let ok = self.build_invite_ok(msg, &sdp, call_id);
                    tracing::info!(%call_id, %e164, gateway = ?gateway.address, codec = %sel_codec.name, srtp = leg_a_crypto.is_some(), "SIP INVITE routed outbound via trunk");
                    return resp.send(ok.as_bytes()).await;
                }
                tracing::warn!(%call_id, %e164, "outbound trunk failed; falling back to echo");
            }
        }

        // Voicemail deposit path: answer the caller, play a greeting ("please leave your message
        // after the tone") and a beep, then capture ONLY the audio after the beep — stored as a
        // Voicemail on hangup, with an MWI pushed to the mailbox. This reuses the IVR prompt
        // runtime, so — like the IVR menu path — the media is plaintext G.711 (SRTP for
        // prompt-bearing media is future work); a phone that offered SRTP is answered plain RTP.
        if let Some(vmbox) = voicemail_target {
            let sock = match UdpSocket::bind("0.0.0.0:0").await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "could not bind RTP for voicemail; answering without media");
                    // Answer anyway so the caller isn't left hanging; no capture is possible.
                    self.mark_answered(call_id).await;
                    let sdp = self.build_sdp(0, &g711_codec, te_pt, None);
                    let ok = self.build_invite_ok(msg, &sdp, call_id);
                    return resp.send(ok.as_bytes()).await;
                }
            };
            let rtp_port = sock.local_addr().map(|a| a.port()).unwrap_or(0);
            // Greeting = the recorded "leave a message after the tone" prompt (when the sound pack
            // is installed) followed by a 250 ms beep. With no prompt installed it is just the beep.
            let greeting = self.voicemail_greeting(g711).await;
            let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
            let task = tokio::spawn(Self::voicemail_deposit_driver(
                sock,
                g711,
                te_pt,
                greeting,
                self.default_tenant,
                call_id,
                vmbox,
                self.voicemails.clone(),
                self.media_ip,
                stop_rx,
            ));
            // Answer plaintext G.711 (prompt-bearing media path, like the IVR menu).
            let sdp = self.build_sdp(rtp_port, &g711_codec, te_pt, None);
            if !call_id_hdr.is_empty() {
                self.dialogs.lock().expect("dialogs mutex").insert(
                    call_id_hdr,
                    Dialog {
                        call_id,
                        // Media::Ivr tears down gracefully on BYE (signals `stop` so the deposit
                        // driver saves whatever was recorded before exiting).
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
            tracing::info!(%call_id, rtp_port, codec = %g711_codec.name, "SIP INVITE answered (voicemail deposit)");
            return resp.send(ok.as_bytes()).await;
        }

        // Echo path: non-mailbox destination (PSTN-style / +E.164), or a no-answer with
        // voicemail disabled. One UDP socket reflecting RTP back to the caller — decrypting then
        // re-encrypting each packet when the caller negotiated SRTP.
        let (echo_crypto, echo_srtp) = match self.negotiate_srtp(&body, secure) {
            Some((c, s)) => (Some(c), Some(s)),
            None => (None, None),
        };
        let (rtp_port, task) = match rtp::bind_echo(capture.clone(), echo_srtp).await {
            Ok((port, task)) => (port, Some(task)),
            Err(e) => {
                tracing::warn!(error = %e, "could not bind RTP; answering without media");
                (0, None)
            }
        };

        // Answer with the caller's preferred codec — the echo path reflects it byte-for-byte.
        let sdp = self.build_sdp(rtp_port, &reflect, te_pt, echo_crypto.as_ref());
        if let Some(task) = task {
            if !call_id_hdr.is_empty() {
                self.dialogs.lock().expect("dialogs mutex").insert(
                    call_id_hdr,
                    Dialog {
                        call_id,
                        media: Media::Echo(task),
                        callee: None,
                        caller: None,
                        capture: capture.clone(),
                        voicemail: None,
                        info_tx: None,
                        answer_sdp: sdp.clone(),
                    },
                );
            } else {
                task.abort();
            }
        }
        // CommOS answers the caller directly (echo/PSTN-style): connect time is now.
        self.mark_answered(call_id).await;
        let ok = self.build_invite_ok(msg, &sdp, call_id);
        tracing::info!(%call_id, rtp_port, codec = %reflect.name, srtp = echo_crypto.is_some(), "SIP INVITE answered (RTP echo)");
        resp.send(ok.as_bytes()).await
    }
}
