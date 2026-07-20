//! `ChannelCreated` event — Rust projection of
//! `contracts/json-schema/events/ChannelCreated.schema.json`.

use serde::{Deserialize, Serialize};

use crate::common::Uuid;
use crate::entities::channel::ChannelKind;
use crate::event::EventPayload;

/// Payload of the `ChannelCreated` canonical event (Volume 5). Produced by the messaging
/// subsystem when a Channel is opened (`workloads.md` §2.3: `ChannelCreated`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChannelCreated {
    pub channel_id: Uuid,
    pub kind: ChannelKind,
}

impl EventPayload for ChannelCreated {
    const TYPE: &'static str = "ChannelCreated";
    // The messaging subsystem is the emitting source (peer of Routing for voice).
    const SOURCE: &'static str = "/communications";

    fn subject(&self) -> String {
        // The event is about the Channel.
        self.channel_id.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Correlation, Envelope};

    #[test]
    fn envelope_carries_type_source_subject() {
        let channel_id = Uuid::now_v7();
        let ctx = Correlation::root(Uuid::now_v7());
        let env = Envelope::new(
            ChannelCreated { channel_id, kind: ChannelKind::Sms },
            &ctx,
            "idem-1",
        );
        assert_eq!(env.event_type, "ChannelCreated");
        assert_eq!(env.source, "/communications");
        assert_eq!(env.subject, channel_id.to_string());
        let json = env.to_json();
        assert_eq!(json["data"]["channel_id"], channel_id.to_string());
        assert_eq!(json["data"]["kind"], "SMS");
    }
}
