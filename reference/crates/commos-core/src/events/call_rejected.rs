//! `CallRejected` event — Rust projection of
//! `contracts/json-schema/events/CallRejected.schema.json`.

use serde::{Deserialize, Serialize};

use crate::event::EventPayload;
use crate::common::Uuid;

/// Payload of the `CallRejected` canonical event (Volume 5). Produced by SIP when the
/// callee rejects the Call (`components.md`: SIP produces Call signalling events).
/// `cause` is an optional free-form reject reason.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallRejected {
    pub call_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
}

impl EventPayload for CallRejected {
    const TYPE: &'static str = "CallRejected";
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
    fn envelope_serialises_optional_cause() {
        let call_id = Uuid::now_v7();
        let ctx = Correlation::root(Uuid::now_v7());
        let env = Envelope::new(
            CallRejected { call_id, cause: Some("BUSY_HERE".into()) },
            &ctx,
            "idem-1",
        );
        assert_eq!(env.event_type, "CallRejected");
        assert_eq!(env.source, "/sip");
        let json = env.to_json();
        assert_eq!(json["data"]["call_id"], call_id.to_string());
        assert_eq!(json["data"]["cause"], "BUSY_HERE");
    }
}
