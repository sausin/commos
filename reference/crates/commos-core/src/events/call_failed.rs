//! `CallFailed` event — Rust projection of
//! `contracts/json-schema/events/CallFailed.schema.json`.

use serde::{Deserialize, Serialize};

use crate::event::EventPayload;
use crate::common::Uuid;

/// Payload of the `CallFailed` canonical event (Volume 5). Produced by SIP when a Call
/// fails (`components.md`: SIP produces Call signalling events). `cause` is required — a
/// failure always carries a reason.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallFailed {
    pub call_id: Uuid,
    pub cause: String,
}

impl EventPayload for CallFailed {
    const TYPE: &'static str = "CallFailed";
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
    fn envelope_serialises_required_cause() {
        let call_id = Uuid::now_v7();
        let ctx = Correlation::root(Uuid::now_v7());
        let env = Envelope::new(
            CallFailed { call_id, cause: "NETWORK_UNREACHABLE".into() },
            &ctx,
            "idem-1",
        );
        assert_eq!(env.event_type, "CallFailed");
        assert_eq!(env.source, "/sip");
        let json = env.to_json();
        assert_eq!(json["data"]["call_id"], call_id.to_string());
        assert_eq!(json["data"]["cause"], "NETWORK_UNREACHABLE");
    }
}
