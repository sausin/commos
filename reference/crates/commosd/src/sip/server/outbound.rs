//! Outbound dialling helpers: per-gateway dial profiles, trunk caller ID, and the emergency
//! (911/112/999/…) route that bypasses every guardrail.

use super::*;

/// The effective dial profile of `gw` under the deployment's `default_cc`.
pub(super) fn gateway_profile(gw: &Gateway, default_cc: &str) -> dialplan::DialProfile {
    dialplan::DialProfile::with_overrides(
        default_cc,
        gw.country_code.as_deref(),
        gw.national_prefix.as_deref(),
        gw.international_prefix.as_deref(),
        gw.strip_prefix.as_deref(),
    )
}

/// The `From` and optional `P-Asserted-Identity` header values for a trunk leg. With a configured
/// `outbound_caller_id` (E.164) both present that number; otherwise the plain service `From` and
/// no PAI. The number is sanitised to `+`/digits so it cannot inject header content.
pub(super) fn trunk_identity(
    media_ip: IpAddr,
    outbound_caller_id: Option<&str>,
    tag: &str,
) -> (String, Option<String>) {
    let cid: String = outbound_caller_id
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '+')
        .collect();
    let valid = cid.trim_start_matches('+').len() >= 4;
    if !valid {
        return (format!("<sip:commos@{media_ip}>;tag={tag}"), None);
    }
    (
        format!("<sip:{cid}@{media_ip}>;tag={tag}"),
        Some(format!("<sip:{cid}@{media_ip}>")),
    )
}

/// If `dialed` (a user part) is an emergency number — as dialled, or after an outside-line prefix
/// configured on any gateway (`9-911`) — return the bare emergency digits to send to the carrier.
pub(super) fn emergency_digits(dialed: &str, strip_prefixes: &[String]) -> Option<String> {
    use crate::control::policy::is_emergency_user;
    let clean: String = dialed
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '(' | ')' | '.'))
        .collect();
    if is_emergency_user(&clean) {
        return Some(clean);
    }
    for sp in strip_prefixes.iter().filter(|s| !s.is_empty()) {
        if let Some(rest) = clean.strip_prefix(sp.as_str()) {
            if is_emergency_user(rest) {
                return Some(rest.to_string());
            }
        }
    }
    None
}

impl SipServer {
    /// All SIP gateways with an address: online ones first (the "first available trunk"), then
    /// offline ones — an emergency call tries everything rather than fail on stale health.
    async fn emergency_gateways(&self) -> Vec<Gateway> {
        let mut online = Vec::new();
        let mut offline = Vec::new();
        let mut cursor = None;
        loop {
            let Ok(page) = self
                .store
                .list_gateways(self.default_tenant, 200, cursor)
                .await
            else {
                break;
            };
            for g in page.items {
                if g.kind == GatewayKind::Sip && g.address.as_deref().is_some_and(|a| !a.is_empty())
                {
                    if g.health == GatewayHealth::Online {
                        online.push(g);
                    } else {
                        offline.push(g);
                    }
                }
            }
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        online.extend(offline);
        online
    }

    /// Detect an emergency call for the dialled user part (see [`emergency_digits`]).
    pub(super) async fn emergency_number(&self, dialed: &str) -> Option<String> {
        // Cheap pre-check: emergency numbers are at most a prefix + 3 digits.
        if dialed.len() > 12 || dialed.is_empty() {
            return None;
        }
        let strips: Vec<String> = self
            .emergency_gateways()
            .await
            .iter()
            .filter_map(|g| g.strip_prefix.clone())
            .collect();
        emergency_digits(dialed, &strips)
    }

    /// Route an emergency call out the first available trunk, **bypassing** caps, schedules,
    /// international gating and gateway health, and never diverting to voicemail/echo: if no
    /// trunk answers the caller gets `503` so the phone fails visibly instead of sitting in a
    /// mailbox.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn route_emergency(
        &self,
        resp: &Responder,
        msg: &SipMessage,
        call_id: Uuid,
        call_id_hdr: &str,
        src: SocketAddr,
        number: &str,
        capture: Option<rtp::Capture>,
        offer: &codec::AudioMedia,
        caller_crypto: Option<&sdes::CryptoAttr>,
    ) -> std::io::Result<()> {
        tracing::warn!(%call_id, %number, "EMERGENCY call: routing via first available trunk");
        let _ = resp.send(self.build_ringing(msg, call_id).as_bytes()).await;
        for gw in self.emergency_gateways().await {
            if let Some(won) = self
                .try_trunk(&gw, number, call_id, capture.clone(), offer, caller_crypto)
                .await
            {
                return self
                    .answer_bridged(resp, msg, call_id, call_id_hdr, src, capture, won, "emergency")
                    .await;
            }
        }
        tracing::error!(%call_id, %number, "EMERGENCY call: no trunk could place the call");
        self.reply(resp, msg, 503, "Service Unavailable").await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip() -> IpAddr {
        "10.0.0.5".parse().unwrap()
    }

    #[test]
    fn trunk_identity_without_cid_is_the_service_identity() {
        assert_eq!(
            trunk_identity(ip(), None, "t1"),
            ("<sip:commos@10.0.0.5>;tag=t1".to_string(), None)
        );
        assert_eq!(trunk_identity(ip(), Some(""), "t1").1, None);
        assert_eq!(trunk_identity(ip(), Some("12"), "t1").1, None, "implausibly short");
    }

    #[test]
    fn trunk_identity_with_cid_sets_from_and_pai() {
        let (from, pai) = trunk_identity(ip(), Some("+442071838750"), "t1");
        assert_eq!(from, "<sip:+442071838750@10.0.0.5>;tag=t1");
        assert_eq!(pai.as_deref(), Some("<sip:+442071838750@10.0.0.5>"));
    }

    #[test]
    fn trunk_identity_sanitises_the_number() {
        let (from, pai) = trunk_identity(ip(), Some("+44 20\r\nX: y>@evil"), "t");
        assert!(!from.contains('\r') && !from.contains(' ') && !from.contains("evil"));
        assert_eq!(from, "<sip:+4420@10.0.0.5>;tag=t");
        assert_eq!(pai.as_deref(), Some("<sip:+4420@10.0.0.5>"));
    }

    #[test]
    fn emergency_numbers_are_detected_with_and_without_outside_line_prefix() {
        let none: Vec<String> = vec![];
        let nine = vec!["9".to_string()];
        assert_eq!(emergency_digits("911", &none).as_deref(), Some("911"));
        assert_eq!(emergency_digits("112", &none).as_deref(), Some("112"));
        assert_eq!(emergency_digits("999", &none).as_deref(), Some("999"));
        assert_eq!(emergency_digits("9911", &none), None, "no strip prefix configured");
        assert_eq!(emergency_digits("9911", &nine).as_deref(), Some("911"));
        assert_eq!(emergency_digits("9-112", &nine).as_deref(), Some("112"));
        // Not emergencies.
        assert_eq!(emergency_digits("9112345", &nine), None);
        assert_eq!(emergency_digits("1001", &nine), None);
        assert_eq!(emergency_digits("07700900123", &nine), None);
    }

    #[test]
    fn gateway_profile_applies_overrides() {
        let mut gw = Gateway::new_sip(Uuid::now_v7(), Uuid::now_v7(), "gw:5060");
        gw.country_code = Some("44".into());
        gw.strip_prefix = Some("9".into());
        let p = gateway_profile(&gw, "1");
        assert_eq!(p.country_code, "44");
        assert_eq!(p.national_prefix, "0");
        assert_eq!(p.international_prefix, "00");
        assert_eq!(p.strip_prefix, "9");
        assert_eq!(
            dialplan::normalize_dialed("9 07700 900123", &p).as_deref(),
            Some("+447700900123")
        );
        // No overrides → deployment defaults.
        let plain = Gateway::new_sip(Uuid::now_v7(), Uuid::now_v7(), "gw:5060");
        assert_eq!(gateway_profile(&plain, "+1").international_prefix, "011");
    }
}
