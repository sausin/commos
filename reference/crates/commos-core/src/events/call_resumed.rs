//! `CallResumed` event — Rust projection of
//! `contracts/json-schema/events/CallResumed.schema.json`.

use serde::{Deserialize, Serialize};

use crate::event::EventPayload;
use crate::common::Uuid;

/// Payload of the `CallResumed` canonical event (Volume 5). Produced by SIP when a Call
/// is taken off hold (`components.md`: SIP produces `CallResumed`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallResumed {
    pub call_id: Uuid,
}

impl EventPayload for CallResumed {
    const TYPE: &'static str = "CallResumed";
    // SIP is the emitting subsystem for Call signalling events (Volume 3 components.md).
    const SOURCE: &'static str = "/sip";

    fn subject(&self) -> String {
        self.call_id.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Correlation, Envelope};

    #[test]
    fn envelope_carries_type_source_subject() {
        let call_id = Uuid::now_v7();
        let ctx = Correlation::root(Uuid::now_v7());
        let env = Envelope::new(CallResumed { call_id }, &ctx, "idem-1");
        assert_eq!(env.event_type, "CallResumed");
        assert_eq!(env.source, "/sip");
        assert_eq!(env.to_json()["data"]["call_id"], call_id.to_string());
    }
}
