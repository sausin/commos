//! Do-not-disturb and busy diversion, plus the shared "a bridged leg answered" bookkeeping.

use std::collections::HashSet;

use super::*;

impl SipServer {
    /// Extension numbers currently in do-not-disturb.
    pub(super) async fn dnd_numbers(&self) -> HashSet<String> {
        self.extension_numbers_where(|e| e.dnd).await
    }

    /// Extension numbers with voicemail switched off (`Extension::voicemail == false`).
    pub(super) async fn voicemail_off_numbers(&self) -> HashSet<String> {
        self.extension_numbers_where(|e| !e.voicemail).await
    }

    /// Whether the dialled extension takes voicemail: the global master switch AND the
    /// extension's own flag.
    pub(super) async fn voicemail_on_for(&self, dialled: &str) -> bool {
        self.voicemail_enabled
            && (dialled.is_empty() || !self.voicemail_off_numbers().await.contains(dialled))
    }

    async fn extension_numbers_where(&self, pred: impl Fn(&commos_core::entities::extension::Extension) -> bool) -> HashSet<String> {
        let mut out = HashSet::new();
        let mut cursor = None;
        loop {
            let Ok(page) = self
                .store
                .list_extensions(self.default_tenant, 200, cursor)
                .await
            else {
                break;
            };
            out.extend(page.items.into_iter().filter(|e| pred(e)).map(|e| e.number));
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        out
    }

    /// Record a bridged leg that answered: register the dialog, index the callee Call-ID for a
    /// callee-side BYE, mark the call answered and `200 OK` the caller with the negotiated SDP.
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub(super) async fn answer_bridged(
        &self,
        resp: &Responder,
        msg: &SipMessage,
        call_id: Uuid,
        call_id_hdr: &str,
        src: SocketAddr,
        capture: Option<rtp::Capture>,
        won: (
            rtp::Bridge,
            CalleeLeg,
            codec::Codec,
            u8,
            Option<sdes::CryptoAttr>,
        ),
        what: &str,
    ) -> std::io::Result<()> {
        let (bridge, callee_leg, sel_codec, sel_te, leg_a_crypto) = won;
        let leg_a_port = bridge.leg_a_port;
        let sdp = self.build_sdp(leg_a_port, &sel_codec, sel_te, leg_a_crypto.as_ref());
        if !call_id_hdr.is_empty() {
            let callee_call_id = callee_leg.call_id.clone();
            self.dialogs.lock().expect("dialogs mutex").insert(
                call_id_hdr.to_string(),
                Dialog {
                    call_id,
                    media: Media::Bridge(bridge),
                    callee: Some(callee_leg),
                    caller: Some(self.caller_leg(msg, call_id, src)),
                    capture,
                    voicemail: None,
                    info_tx: None,
                    answer_sdp: sdp.clone(),
                },
            );
            self.bye_aliases
                .lock()
                .expect("aliases mutex")
                .insert(callee_call_id, call_id_hdr.to_string());
        } else {
            bridge.abort();
        }
        self.mark_answered(call_id).await;
        let ok = self.build_invite_ok(msg, &sdp, call_id);
        tracing::info!(%call_id, leg_a_port, "SIP INVITE bridged ({what})");
        resp.send(ok.as_bytes()).await
    }

    /// The callee answered **busy** (`486`): if the dialled extension has an active `BUSY`
    /// forwarding rule, walk its targets and bridge the first that answers. `Some(result)` when
    /// the caller was answered that way; `None` to fall back to the normal decline handling
    /// (no rule, or no target answered).
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn divert_busy(
        &self,
        resp: &Responder,
        msg: &SipMessage,
        call_id: Uuid,
        call_id_hdr: &str,
        src: SocketAddr,
        dialled: &str,
        capture: Option<rtp::Capture>,
        offer: &codec::AudioMedia,
        caller_crypto: Option<&sdes::CryptoAttr>,
        caller_id: CallerId<'_>,
    ) -> Option<std::io::Result<()>> {
        if dialled.is_empty() {
            return None;
        }
        let opts = crate::control::ringplan::PlanOpts {
            default_ring_seconds: self.no_answer_timeout.as_secs().max(1) as u32,
            voicemail_enabled: self.voicemail_on_for(dialled).await,
        };
        let plan = crate::control::ringresolve::resolve_busy_plan(
            &self.store,
            self.default_tenant,
            dialled,
            opts,
        )
        .await?;
        tracing::info!(%call_id, %dialled, "callee busy (486); diverting via BUSY forwarding rule");
        let won = self
            .execute_ring_plan(&plan, call_id, capture.clone(), offer, caller_crypto, caller_id)
            .await?;
        Some(
            self.answer_bridged(resp, msg, call_id, call_id_hdr, src, capture, won, "busy forward")
                .await,
        )
    }
}
