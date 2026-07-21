//! `Webhook` entity — Rust projection of
//! `contracts/json-schema/entities/Webhook.schema.json`.
//!
//! A Webhook is an outbound HTTP subscription: it names a `url` to deliver to and the set of
//! canonical event `event_types` that trigger a delivery. Like `Queue`/`Route` it is
//! *configuration*, not an occurrence — it carries no lifecycle state machine and has no
//! creation event in the frozen catalogue. Deliveries against it surface as the
//! `WebhookDelivered` / `WebhookDeliveryFailed` events (Volume 5).

use serde::{Deserialize, Serialize};

use crate::common::{EntityBase, Uuid};

/// The Webhook entity. `EntityBase` is flattened so the wire shape is
/// `allOf: [EntityBase] + Webhook properties`, matching the schema. `url`, `event_types`
/// and `active` are required; `secret_ref` is optional.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Webhook {
    #[serde(flatten)]
    pub base: EntityBase,
    /// Endpoint deliveries are POSTed to.
    pub url: String,
    /// Canonical event TYPEs this endpoint subscribes to; `["*"]` means all.
    pub event_types: Vec<String>,
    /// Reference to the HMAC secret used to sign deliveries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<String>,
    /// Whether the endpoint is currently receiving deliveries.
    pub active: bool,
}

impl Webhook {
    /// Create a new active Webhook for `url` subscribed to `event_types`, with no
    /// `secret_ref`. Callers set `secret_ref` / `active` directly on the returned value.
    pub fn new(tenant: Uuid, url: impl Into<String>, event_types: Vec<String>) -> Self {
        Webhook {
            base: EntityBase::new(tenant),
            url: url.into(),
            event_types,
            secret_ref: None,
            active: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_is_active_and_omits_secret_ref() {
        let t = Uuid::now_v7();
        let w = Webhook::new(t, "https://example.test/hook", vec!["*".into()]);
        // `active` defaults true from `new`.
        assert!(w.active);
        assert_eq!(w.base.version, 0);
        assert_eq!(w.base.tenant_id, t);
        let json = serde_json::to_value(&w).unwrap();
        // Required field names match the schema.
        assert_eq!(json["url"], "https://example.test/hook");
        assert_eq!(json["event_types"][0], "*");
        assert_eq!(json["active"], true);
        // `secret_ref` omitted when None.
        assert!(json.get("secret_ref").is_none());
        assert!(json.get("id").is_some());
        assert!(json.get("tenant_id").is_some());
    }

    #[test]
    fn secret_ref_serialises_when_set_and_round_trips() {
        let mut w = Webhook::new(
            Uuid::now_v7(),
            "https://example.test/hook",
            vec!["CallStarted".into(), "CallEnded".into()],
        );
        w.secret_ref = Some("secret:webhook-1".into());
        w.active = false;
        let json = serde_json::to_value(&w).unwrap();
        assert_eq!(json["secret_ref"], "secret:webhook-1");
        assert_eq!(json["active"], false);
        // Round-trips.
        let back: Webhook = serde_json::from_value(json).unwrap();
        assert_eq!(back.url, "https://example.test/hook");
        assert_eq!(
            back.event_types,
            vec!["CallStarted".to_string(), "CallEnded".to_string()]
        );
        assert_eq!(back.secret_ref, Some("secret:webhook-1".to_string()));
        assert!(!back.active);
    }
}
