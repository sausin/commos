//! Non-INVITE SIP method handlers (REGISTER, INFO, BYE/CANCEL) for [`SipServer`].

use super::*;

impl SipServer {
    /// INFO: out-of-band DTMF (`application/dtmf-relay` / `application/dtmf`). If the datagram's
    /// dialog is a live IVR session, inject the pressed digit into it; always `200 OK`.
    pub(super) async fn on_info(&self, resp: &Responder, msg: &SipMessage) -> std::io::Result<()> {
        let body = String::from_utf8_lossy(msg.body());
        if let (Some(call_id), Some(digit)) = (msg.call_id(), dtmf::parse_info_dtmf(&body)) {
            let injected = self
                .dialogs
                .lock()
                .expect("dialogs mutex")
                .get(call_id)
                .and_then(|d| d.info_tx.as_ref().map(|tx| tx.send(digit).is_ok()))
                .unwrap_or(false);
            if injected {
                tracing::info!(%call_id, %digit, "SIP INFO DTMF injected into IVR");
            }
        }
        self.reply(resp, msg, 200, "OK").await
    }

    /// REGISTER: bind the AoR to its contact and confirm with a `200 OK`.
    pub(super) async fn on_register(
        &self,
        resp: &Responder,
        msg: &SipMessage,
    ) -> std::io::Result<()> {
        let src = resp.peer();
        // Digest auth gate (Volume 9): an unauthenticated REGISTER is challenged with 401 +
        // WWW-Authenticate; the phone re-sends with credentials we verify against its stored
        // per-device secret. Auth is enforced when configured, and always for a public source.
        let authed_user = if self.auth_required_from(src) {
            match self.digest_ok(msg, "REGISTER").await {
                Some(user) => Some(user),
                None => {
                    tracing::info!(%src, "SIP REGISTER challenged (digest auth required)");
                    return self.send_challenge(resp, msg).await;
                }
            }
        } else {
            None
        };

        let aor = match msg.register_aor() {
            Some(a) if !a.is_empty() => a,
            _ => {
                tracing::debug!(%src, "REGISTER without a usable To/From AoR");
                return self.reply(resp, msg, 400, "Bad Request").await;
            }
        };

        // Identity binding: a device authenticated as `user` may only register its own AoR. This
        // stops a device that holds one extension's credential from hijacking another extension's
        // registration (and thus its calls/voicemail) by putting a different AoR in To/From.
        if let Some(user) = &authed_user {
            let aor_user = user_part(&aor);
            if aor_user
                .map(|u| !u.eq_ignore_ascii_case(user))
                .unwrap_or(true)
            {
                tracing::warn!(
                    %src, authed = %user, aor = %aor,
                    "SIP REGISTER rejected: authenticated user does not own the registered AoR"
                );
                return self.reply(resp, msg, 403, "Forbidden").await;
            }
        }
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
        let extra = [
            ("Contact", contact_header),
            ("Expires", expires.to_string()),
        ];
        let reply = message::response_with(msg, 200, "OK", &extra);
        let sent = resp.send(reply.as_bytes()).await;

        // A returning phone should light its message-waiting lamp: push MWI after the 200 OK
        // if the mailbox has unheard voicemails. Skipped on de-register (expires=0) and when
        // voicemail is off. Fire-and-forget, so the REGISTER response is never delayed.
        if self.voicemail_enabled && expires > 0 {
            self.maybe_notify_mwi(aor, contact);
        }
        sent
    }

    /// BYE/CANCEL: hang the Call up (produces the CDR), abort its RTP, and `200 OK`.
    pub(super) async fn on_bye(&self, resp: &Responder, msg: &SipMessage) -> std::io::Result<()> {
        let method = msg.method().unwrap_or("BYE").to_string();
        if let Some(incoming_call_id) = msg.call_id() {
            // The incoming Call-ID is either a primary (caller-leg) dialog key, or — for a
            // bridged call whose callee hung up — a callee-leg alias pointing at the primary.
            let (primary, from_callee) = {
                let dialogs = self.dialogs.lock().expect("dialogs mutex");
                if dialogs.contains_key(incoming_call_id) {
                    (Some(incoming_call_id.to_string()), false)
                } else {
                    let alias = self
                        .bye_aliases
                        .lock()
                        .expect("aliases mutex")
                        .get(incoming_call_id)
                        .cloned();
                    (alias, true)
                }
            };

            if let Some(primary) = primary {
                let dialog = self.dialogs.lock().expect("dialogs mutex").remove(&primary);
                if let Some(d) = dialog {
                    if let Some(callee) = &d.callee {
                        // Drop the callee-leg alias index.
                        self.bye_aliases
                            .lock()
                            .expect("aliases mutex")
                            .remove(&callee.call_id);
                        if from_callee {
                            // The CALLEE hung up: propagate the BYE to the CALLER so its phone
                            // disconnects too (otherwise it stays "up" with dead air). The callee
                            // is already gone, so we do not echo a BYE back to it.
                            if let Some(caller) = &d.caller {
                                self.send_bye_to_leg(caller).await;
                            }
                        } else {
                            // The CALLER hung up: tear the callee leg down with a BYE.
                            self.send_bye_to_leg(callee).await;
                        }
                    }
                    d.media.abort();
                    // Drain the captured audio and persist it (as-is, no transcode), off the BYE
                    // path so the caller's 200 OK isn't delayed by the object write. A voicemail
                    // dialog stores a Voicemail and pushes MWI; otherwise, when call recording is
                    // on, it stores a Recording.
                    if let Some(cap) = &d.capture {
                        let bytes = std::mem::take(&mut *cap.lock().expect("capture mutex"));
                        if !bytes.is_empty() {
                            match d.voicemail {
                                Some(vmbox) => self.spawn_save_voicemail(d.call_id, vmbox, bytes),
                                None => self.spawn_save_recording(d.call_id, bytes),
                            }
                        }
                    }
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
        }
        self.reply(resp, msg, 200, "OK").await
    }
}
