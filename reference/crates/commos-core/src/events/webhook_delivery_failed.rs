//! `WebhookDeliveryFailed` event — Rust projection of
//! `contracts/json-schema/events/WebhookDeliveryFailed.schema.json`.

use serde::{Deserialize, Serialize};

use crate::common::Uuid;
use crate::event::EventPayload;

/// Payload of the `WebhookDeliveryFailed` canonical event (Volume 5). Produced by the
/// Webhooks subsystem when a delivery attempt to an endpoint fails.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebhookDeliveryFailed {
    pub webhook_id: Uuid,
    pub delivered_event_id: Uuid,
    pub attempt: u32,
    pub error: String,
}

impl EventPayload for WebhookDeliveryFailed {
    const TYPE: &'static str = "WebhookDeliveryFailed";
    // Webhooks is the emitting subsystem for delivery lifecycle events.
    const SOURCE: &'static str = "/webhooks";

    fn subject(&self) -> String {
        // The event is about the Webhook.
        self.webhook_id.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Correlation, Envelope};

    #[test]
    fn envelope_carries_type_source_subject() {
        let webhook_id = Uuid::now_v7();
        let delivered_event_id = Uuid::now_v7();
        let ctx = Correlation::root(Uuid::now_v7());
        let env = Envelope::new(
            WebhookDeliveryFailed {
                webhook_id,
                delivered_event_id,
                attempt: 3,
                error: "connection refused".into(),
            },
            &ctx,
            "idem-1",
        );
        assert_eq!(env.event_type, "WebhookDeliveryFailed");
        assert_eq!(env.source, "/webhooks");
        assert_eq!(env.subject, webhook_id.to_string());
        let json = env.to_json();
        assert_eq!(json["data"]["webhook_id"], webhook_id.to_string());
        assert_eq!(
            json["data"]["delivered_event_id"],
            delivered_event_id.to_string()
        );
        assert_eq!(json["data"]["attempt"], 3);
        assert_eq!(json["data"]["error"], "connection refused");
    }
}
