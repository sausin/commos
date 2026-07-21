//! `WebhookDelivered` event — Rust projection of
//! `contracts/json-schema/events/WebhookDelivered.schema.json`.

use serde::{Deserialize, Serialize};

use crate::common::Uuid;
use crate::event::EventPayload;

/// Payload of the `WebhookDelivered` canonical event (Volume 5). Produced by the Webhooks
/// subsystem when a subscribed event is successfully delivered to an endpoint.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebhookDelivered {
    pub webhook_id: Uuid,
    pub delivered_event_id: Uuid,
    pub status_code: u16,
    pub attempt: u32,
    pub duration_ms: u64,
}

impl EventPayload for WebhookDelivered {
    const TYPE: &'static str = "WebhookDelivered";
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
            WebhookDelivered {
                webhook_id,
                delivered_event_id,
                status_code: 200,
                attempt: 1,
                duration_ms: 42,
            },
            &ctx,
            "idem-1",
        );
        assert_eq!(env.event_type, "WebhookDelivered");
        assert_eq!(env.source, "/webhooks");
        assert_eq!(env.subject, webhook_id.to_string());
        let json = env.to_json();
        assert_eq!(json["data"]["webhook_id"], webhook_id.to_string());
        assert_eq!(
            json["data"]["delivered_event_id"],
            delivered_event_id.to_string()
        );
        assert_eq!(json["data"]["status_code"], 200);
        assert_eq!(json["data"]["attempt"], 1);
        assert_eq!(json["data"]["duration_ms"], 42);
    }
}
