//! Dial-in feature codes handled by the PBX itself (no dialplan entry needed):
//!
//! | code           | effect                                                     |
//! |----------------|------------------------------------------------------------|
//! | `*78`          | do-not-disturb **on** for the calling extension            |
//! | `*79`          | do-not-disturb **off**                                     |
//! | `*72<number>`  | forward-always **on**, to `<number>` (extension or PSTN)   |
//! | `*73`          | forward-always **off**                                     |
//!
//! The call is answered, a short confirmation tone (rising double beep = done, low tone =
//! refused) is played, and the PBX hangs up. State is persisted so it survives restarts and shows
//! in the admin UI: DND is the `Extension.dnd` flag; forward-always is a [`Forwarding`] rule in
//! [`ForwardMode::Always`].

use commos_core::entities::forwarding::{ForwardMode, Forwarding};

use super::*;

/// A parsed feature code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum FeatureCode {
    DndOn,
    DndOff,
    /// Forward-always to the dialled digits (`+` allowed as the first character). The target may
    /// be empty (`*72` alone) — applying it then fails, so the caller hears the refusal tone.
    ForwardOn(String),
    ForwardOff,
}

/// Parse the dialled user part. `*` may arrive percent-encoded (`%2A`) from some phones; a
/// trailing `#` (some users terminate `*72<number>#`) is ignored.
pub(super) fn parse_feature_code(dialed: &str) -> Option<FeatureCode> {
    let d = dialed.replace("%2A", "*").replace("%2a", "*");
    let d = d.trim().trim_end_matches('#');
    match d {
        "*78" => Some(FeatureCode::DndOn),
        "*79" => Some(FeatureCode::DndOff),
        "*73" => Some(FeatureCode::ForwardOff),
        _ => {
            let rest = d.strip_prefix("*72")?;
            let rest: String = rest
                .chars()
                .filter(|c| !matches!(c, ' ' | '-' | '(' | ')' | '.'))
                .collect();
            Some(FeatureCode::ForwardOn(rest))
        }
    }
}

/// The `Forwarding.targets` entry for a forward-always destination: a short number is an
/// internal extension (`sip:<n>`); anything PSTN-length or `+`-prefixed is external (the trunk
/// path normalises it with the gateway's dial profile at call time).
pub(super) fn forward_target(number: &str) -> Option<String> {
    let digits = number.strip_prefix('+').unwrap_or(number);
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if number.starts_with('+') || digits.len() >= 7 {
        Some(format!("external:{number}"))
    } else {
        Some(format!("sip:{number}"))
    }
}

/// Apply `code` for extension `number`, persisting to the store. Returns `false` when the code is
/// refused (the caller is not a known extension, or the forward destination is invalid).
pub(super) async fn apply_feature_code(
    store: &Arc<dyn Store>,
    tenant: Uuid,
    number: &str,
    code: &FeatureCode,
) -> bool {
    use crate::store::Tx;

    // Only a real extension may set DND / forwarding for itself.
    let Some(mut ext) = find_extension(store, tenant, number).await else {
        return false;
    };
    match code {
        FeatureCode::DndOn | FeatureCode::DndOff => {
            let on = matches!(code, FeatureCode::DndOn);
            if ext.dnd != on {
                ext.dnd = on;
                ext.base.touch();
                if store
                    .commit(Tx { extensions: vec![ext], ..Default::default() })
                    .await
                    .is_err()
                {
                    return false;
                }
            }
            true
        }
        FeatureCode::ForwardOn(raw) => {
            let Some(target) = forward_target(raw) else {
                return false;
            };
            // Forwarding to yourself would loop.
            if raw.trim_start_matches('+') == number {
                return false;
            }
            let (mut rule, existing) = match find_always_rule(store, tenant, number).await {
                Some(r) => (r, true),
                None => (Forwarding::create(tenant, number, ForwardMode::Always), false),
            };
            rule.enabled = true;
            rule.targets = vec![target];
            if existing {
                rule.base.touch();
            }
            store
                .commit(Tx { forwardings: vec![rule], ..Default::default() })
                .await
                .is_ok()
        }
        FeatureCode::ForwardOff => {
            if let Some(mut rule) = find_always_rule(store, tenant, number).await {
                if rule.enabled {
                    rule.enabled = false;
                    rule.base.touch();
                    return store
                        .commit(Tx { forwardings: vec![rule], ..Default::default() })
                        .await
                        .is_ok();
                }
            }
            true // already off
        }
    }
}

pub(super) async fn find_extension(
    store: &Arc<dyn Store>,
    tenant: Uuid,
    number: &str,
) -> Option<commos_core::entities::extension::Extension> {
    let mut cursor = None;
    loop {
        let page = store.list_extensions(tenant, 200, cursor).await.ok()?;
        if let Some(e) = page.items.iter().find(|e| e.number == number) {
            return Some(e.clone());
        }
        cursor = Some(page.next_cursor?);
    }
}

async fn find_always_rule(store: &Arc<dyn Store>, tenant: Uuid, number: &str) -> Option<Forwarding> {
    let mut cursor = None;
    loop {
        let page = store.list_forwardings(tenant, 200, cursor).await.ok()?;
        if let Some(f) = page
            .items
            .iter()
            .find(|f| f.number == number && f.mode == ForwardMode::Always)
        {
            return Some(f.clone());
        }
        cursor = Some(page.next_cursor?);
    }
}

/// The confirmation audio: a rising double beep when `ok`, a single low tone when refused,
/// followed by a short silent tail so the last tone isn't clipped by the hangup.
pub(super) fn confirmation_tone(ok: bool, codec: g711::G711) -> Vec<u8> {
    let mut out = Vec::new();
    if ok {
        out.extend(g711::tone(880.0, 120, 8000, codec));
        out.extend(std::iter::repeat_n(codec.silence(), 8 * 60));
        out.extend(g711::tone(1175.0, 180, 8000, codec));
    } else {
        out.extend(g711::tone(330.0, 600, 8000, codec));
    }
    out.extend(std::iter::repeat_n(codec.silence(), 8 * 200));
    out
}

impl SipServer {
    /// Answer a feature-code call, apply it, play the confirmation tone and hang up.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn answer_with_feature_code(
        &self,
        resp: &Responder,
        msg: &SipMessage,
        call_id: Uuid,
        call_id_hdr: &str,
        from_ref: &str,
        code: FeatureCode,
        g711: g711::G711,
        te_pt: u8,
        src: SocketAddr,
    ) -> std::io::Result<()> {
        let caller = user_part(from_ref).unwrap_or("");
        let ok = apply_feature_code(&self.store, self.default_tenant, caller, &code).await;
        tracing::info!(%call_id, %caller, ?code, ok, "feature code");

        let g711_codec = codec::Codec {
            pt: g711.payload_type(),
            name: g711.sdp_name().to_string(),
            clock: 8000,
        };
        let sock = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "could not bind RTP for feature code; answering without media");
                self.mark_answered(call_id).await;
                let sdp = self.build_sdp(0, &g711_codec, te_pt, None);
                let ok = self.build_invite_ok(msg, &sdp, call_id);
                return resp.send(ok.as_bytes()).await;
            }
        };
        let rtp_port = sock.local_addr().map(|a| a.port()).unwrap_or(0);
        let tone = confirmation_tone(ok, g711);
        let caller_leg = self.caller_leg(msg, call_id, src);
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(Self::feature_code_driver(
            sock,
            g711,
            te_pt,
            tone,
            self.media_ip,
            self.dialogs.clone(),
            call_id_hdr.to_string(),
            caller_leg,
            self.routing.clone(),
            self.default_tenant,
            call_id,
            stop_rx,
        ));
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
        resp.send(ok.as_bytes()).await
    }

    /// Play the confirmation tone (latching onto the caller's RTP), then release the caller. If
    /// the caller hangs up first, `on_bye` has already torn the dialog down and signalled `stop`.
    #[allow(clippy::too_many_arguments)]
    async fn feature_code_driver(
        sock: UdpSocket,
        codec: g711::G711,
        te_pt: u8,
        tone: Vec<u8>,
        media_ip: IpAddr,
        dialogs: Arc<Mutex<HashMap<String, Dialog>>>,
        call_id_hdr: String,
        caller_leg: CalleeLeg,
        routing: Routing,
        tenant: Uuid,
        call_id: Uuid,
        mut stop_rx: tokio::sync::watch::Receiver<bool>,
    ) {
        let (_info_tx, mut info_rx) = tokio::sync::mpsc::unbounded_channel::<char>();
        let mut peer: Option<SocketAddr> = None;
        // Tone length (8 bytes/ms) plus time for the phone's first RTP packet to arrive.
        let window = Duration::from_millis(tone.len() as u64 / 8 + 1500);
        tokio::select! {
            _ = ivr::play_and_collect(&sock, &tone, codec.payload_type(), te_pt, window, &mut info_rx, &mut peer) => {}
            _ = stop_rx.changed() => return,
        }
        if *stop_rx.borrow() {
            return;
        }
        Self::hangup_caller(media_ip, &caller_leg, &dialogs, &call_id_hdr, &routing, tenant, call_id)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{MemStore, Tx};
    use commos_core::entities::extension::Extension;

    #[test]
    fn parses_each_code() {
        assert_eq!(parse_feature_code("*78"), Some(FeatureCode::DndOn));
        assert_eq!(parse_feature_code("*79"), Some(FeatureCode::DndOff));
        assert_eq!(parse_feature_code("*73"), Some(FeatureCode::ForwardOff));
        assert_eq!(
            parse_feature_code("*721002"),
            Some(FeatureCode::ForwardOn("1002".into()))
        );
        assert_eq!(
            parse_feature_code("*72+44 7700 900123#"),
            Some(FeatureCode::ForwardOn("+447700900123".into()))
        );
        assert_eq!(parse_feature_code("*72"), Some(FeatureCode::ForwardOn(String::new())));
        // Percent-encoded star.
        assert_eq!(parse_feature_code("%2A78"), Some(FeatureCode::DndOn));
    }

    #[test]
    fn does_not_swallow_other_numbers() {
        for d in ["*97", "*98", "*781", "*7", "78", "1002", "07700900123", "*790"] {
            assert_eq!(parse_feature_code(d), None, "{d}");
        }
    }

    #[test]
    fn forward_target_distinguishes_extension_from_pstn() {
        assert_eq!(forward_target("1002").as_deref(), Some("sip:1002"));
        assert_eq!(forward_target("07700900123").as_deref(), Some("external:07700900123"));
        assert_eq!(forward_target("+447700900123").as_deref(), Some("external:+447700900123"));
        assert_eq!(forward_target(""), None);
        assert_eq!(forward_target("12x4"), None);
    }

    fn setup() -> (Arc<dyn Store>, Uuid, Extension) {
        let store: Arc<dyn Store> = Arc::new(MemStore::new());
        let t = Uuid::now_v7();
        let ext = Extension::new(t, "1001", Uuid::now_v7());
        (store, t, ext)
    }

    async fn seed(store: &Arc<dyn Store>, ext: &Extension) {
        store
            .commit(Tx { extensions: vec![ext.clone()], ..Default::default() })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn dnd_toggles_and_persists() {
        let (store, t, ext) = setup();
        seed(&store, &ext).await;
        assert!(apply_feature_code(&store, t, "1001", &FeatureCode::DndOn).await);
        assert!(find_extension(&store, t, "1001").await.unwrap().dnd);
        // Idempotent.
        assert!(apply_feature_code(&store, t, "1001", &FeatureCode::DndOn).await);
        assert!(apply_feature_code(&store, t, "1001", &FeatureCode::DndOff).await);
        assert!(!find_extension(&store, t, "1001").await.unwrap().dnd);
    }

    #[tokio::test]
    async fn unknown_caller_is_refused() {
        let (store, t, _) = setup();
        assert!(!apply_feature_code(&store, t, "9999", &FeatureCode::DndOn).await);
        assert!(
            !apply_feature_code(&store, t, "9999", &FeatureCode::ForwardOn("1002".into())).await
        );
    }

    #[tokio::test]
    async fn forward_on_creates_then_updates_one_always_rule_and_off_disables() {
        let (store, t, ext) = setup();
        seed(&store, &ext).await;
        let on = |n: &str| FeatureCode::ForwardOn(n.into());
        assert!(apply_feature_code(&store, t, "1001", &on("1002")).await);
        assert!(apply_feature_code(&store, t, "1001", &on("07700900123")).await);

        let page = store.list_forwardings(t, 50, None).await.unwrap();
        assert_eq!(page.items.len(), 1, "re-setting updates the same rule");
        let rule = &page.items[0];
        assert_eq!(rule.mode, ForwardMode::Always);
        assert!(rule.is_active());
        assert_eq!(rule.targets, vec!["external:07700900123"]);

        assert!(apply_feature_code(&store, t, "1001", &FeatureCode::ForwardOff).await);
        let rule = &store.list_forwardings(t, 50, None).await.unwrap().items[0];
        assert!(!rule.is_active());
        // Off when already off / never set is fine.
        assert!(apply_feature_code(&store, t, "1001", &FeatureCode::ForwardOff).await);
    }

    #[tokio::test]
    async fn forward_rejects_empty_garbage_and_self() {
        let (store, t, ext) = setup();
        seed(&store, &ext).await;
        for bad in ["", "abc", "1001"] {
            assert!(
                !apply_feature_code(&store, t, "1001", &FeatureCode::ForwardOn(bad.into())).await,
                "{bad:?}"
            );
        }
        assert!(store.list_forwardings(t, 50, None).await.unwrap().items.is_empty());
    }

    #[test]
    fn confirmation_tones_differ_and_are_nonempty() {
        let ok = confirmation_tone(true, g711::G711::Ulaw);
        let bad = confirmation_tone(false, g711::G711::Ulaw);
        assert!(!ok.is_empty() && !bad.is_empty());
        assert_ne!(ok, bad);
    }
}
