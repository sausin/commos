//! `ThreadOpened` event — Rust projection of
//! `contracts/json-schema/events/ThreadOpened.schema.json`.

use serde::{Deserialize, Serialize};

use crate::common::Uuid;
use crate::event::EventPayload;

/// Payload of the `ThreadOpened` canonical event (Volume 5). Produced by the messaging
/// subsystem when a Thread is opened within a Channel (`workloads.md` §2.3: `ThreadOpened`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThreadOpened {
    pub thread_id: Uuid,
    pub channel_id: Uuid,
}

impl EventPayload for ThreadOpened {
    const TYPE: &'static str = "ThreadOpened";
    // The messaging subsystem is the emitting source (peer of Routing for voice).
    const SOURCE: &'static str = "/communications";

    fn subject(&self) -> String {
        // The event is about the Thread.
        self.thread_id.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Correlation, Envelope};

    #[test]
    fn envelope_carries_type_source_subject() {
        let thread_id = Uuid::now_v7();
        let channel_id = Uuid::now_v7();
        let ctx = Correlation::root(Uuid::now_v7());
        let env = Envelope::new(ThreadOpened { thread_id, channel_id }, &ctx, "idem-1");
        assert_eq!(env.event_type, "ThreadOpened");
        assert_eq!(env.source, "/communications");
        assert_eq!(env.subject, thread_id.to_string());
        let json = env.to_json();
        assert_eq!(json["data"]["thread_id"], thread_id.to_string());
        assert_eq!(json["data"]["channel_id"], channel_id.to_string());
    }
}
