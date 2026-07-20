//! `MessageSent` event — Rust projection of
//! `contracts/json-schema/events/MessageSent.schema.json`.

use serde::{Deserialize, Serialize};

use crate::common::Uuid;
use crate::event::EventPayload;

/// Payload of the `MessageSent` canonical event (Volume 5). Produced by the messaging
/// subsystem when a Message is accepted for transport (`workloads.md` §2.3: `MessageSent`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessageSent {
    pub message_id: Uuid,
    pub channel_id: Uuid,
    pub sender_ref: String,
}

impl EventPayload for MessageSent {
    const TYPE: &'static str = "MessageSent";
    // The messaging subsystem is the emitting source (peer of Routing for voice).
    const SOURCE: &'static str = "/communications";

    fn subject(&self) -> String {
        // The event is about the Message.
        self.message_id.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Correlation, Envelope};

    #[test]
    fn envelope_carries_type_source_subject() {
        let message_id = Uuid::now_v7();
        let channel_id = Uuid::now_v7();
        let ctx = Correlation::root(Uuid::now_v7());
        let env = Envelope::new(
            MessageSent {
                message_id,
                channel_id,
                sender_ref: "sip:100".into(),
            },
            &ctx,
            "idem-1",
        );
        assert_eq!(env.event_type, "MessageSent");
        assert_eq!(env.source, "/communications");
        assert_eq!(env.subject, message_id.to_string());
        let json = env.to_json();
        assert_eq!(json["data"]["message_id"], message_id.to_string());
        assert_eq!(json["data"]["channel_id"], channel_id.to_string());
        assert_eq!(json["data"]["sender_ref"], "sip:100");
    }
}
