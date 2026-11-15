//! Destination resolution: extension/IVR/DID lookup and outbound gateway selection.

use super::*;

impl SipServer {
    /// Resolve the request-URI through the Extension→Route table. When the dialled number
    /// routes to a SIP endpoint (`sip:<user>@<host>`), returns that destination so the callee
    /// is found by the *route's* target; a queue/external (or unrouted) destination returns
    /// `None`, leaving the raw request-URI to be matched (or the echo path to take over).
    pub(super) async fn resolve_route(&self, request_uri: &str) -> Option<String> {
        let number = user_part(request_uri)?;
        let dest = self
            .routing
            .resolve_extension(self.default_tenant, number)
            .await?;
        if dest.starts_with("sip:") || dest.starts_with("sips:") {
            Some(dest)
        } else {
            tracing::info!(%dest, number, "extension routes to a non-SIP destination; using echo path");
            None
        }
    }

    /// Resolve an `ivr:<uuid>` routing target for the dialled number, if the extension routes
    /// to an IVR menu. Returns the IVR id, else `None` (a non-IVR destination).
    pub(super) async fn resolve_ivr_target(&self, request_uri: &str) -> Option<Uuid> {
        let number = user_part(request_uri)?;
        let dest = self
            .routing
            .resolve_extension(self.default_tenant, number)
            .await?;
        ivr_id_of(&dest)
    }

    /// Resolve an inbound **DID**: if the dialled number (in E.164) is a provisioned DID, return
    /// its `destination_ref` — where an inbound carrier call to this number is routed. `None`
    /// when the request-URI is not a known external number for this tenant.
    pub(super) async fn resolve_did(&self, request_uri: &str) -> Option<String> {
        let e164 = dialplan::normalize_e164(request_uri, &self.default_cc)?;
        let mut cursor = None;
        loop {
            let page = self
                .store
                .list_dids(self.default_tenant, 200, cursor)
                .await
                .ok()?;
            if let Some(did) = page.items.iter().find(|d| d.e164 == e164) {
                // Business hours: a DID with a schedule routes to its closed destination
                // while the schedule is closed (`control/schedule.rs::pick_destination`).
                let schedule = match did.schedule_id {
                    Some(sid) => self.store.get_schedule(self.default_tenant, sid).await.ok().flatten(),
                    None => None,
                };
                return Some(crate::control::schedule::pick_destination(
                    did,
                    schedule.as_ref(),
                    jiff::Timestamp::now(),
                ));
            }
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => return None,
            }
        }
    }

    /// Select an outbound gateway for a **dialled target**: if it is an external E.164 number and
    /// an `ONLINE` `SIP` gateway with an address exists, return `(gateway, e164)` to place the
    /// call to the carrier. `None` for an internal target or when no usable gateway is configured.
    ///
    /// The target is normalised **per gateway** under that gateway's dial profile
    /// ([`outbound::gateway_profile`]: country code, national/international/outside-line
    /// prefixes), so a user can dial `07700 900123`, `9 07700 900123`, `00…`/`011…` or `+…`.
    /// A target that is an internal extension is never sent to the PSTN.
    pub(super) async fn select_outbound_gateway(&self, target: &str) -> Option<(Gateway, String)> {
        // Internal extensions win over PSTN interpretation.
        if let Some(number) = dialed_user(target) {
            if !number.starts_with('+')
                && self
                    .routing
                    .resolve_extension(self.default_tenant, number)
                    .await
                    .is_some()
            {
                return None;
            }
        }
        let mut cursor = None;
        loop {
            let page = self
                .store
                .list_gateways(self.default_tenant, 200, cursor)
                .await
                .ok()?;
            if let Some(gw) = page.items.iter().find(|g| {
                g.kind == GatewayKind::Sip
                    && g.health == GatewayHealth::Online
                    && g.address.as_deref().is_some_and(|a| !a.is_empty())
                    && dialplan::normalize_dialed(
                        target,
                        &outbound::gateway_profile(g, &self.default_cc),
                    )
                    .is_some()
            }) {
                let e164 = dialplan::normalize_dialed(
                    target,
                    &outbound::gateway_profile(gw, &self.default_cc),
                )?;
                return Some((gw.clone(), e164));
            }
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => return None,
            }
        }
    }

    /// The digest credentials to authenticate to `carrier_id`'s carrier, from its Trunk's `auth`.
    pub(super) async fn trunk_credentials(&self, carrier_id: Uuid) -> Option<(String, String)> {
        let mut cursor = None;
        loop {
            let page = self
                .store
                .list_trunks(self.default_tenant, 200, cursor)
                .await
                .ok()?;
            if let Some(t) = page.items.iter().find(|t| t.carrier_id == carrier_id) {
                return t.credentials();
            }
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => return None,
            }
        }
    }
}

/// The user part of a dialled target (`sip:07700900123@host`, `tel:+44…`, or a bare number).
fn dialed_user(target: &str) -> Option<&str> {
    let t = target.trim().trim_start_matches('<');
    let t = t
        .strip_prefix("sips:")
        .or_else(|| t.strip_prefix("sip:"))
        .or_else(|| t.strip_prefix("tel:"))
        .unwrap_or(t);
    let user = t.split('@').next()?.split(';').next()?.trim();
    (!user.is_empty()).then_some(user)
}
