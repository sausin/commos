//! `CallAnswered` event — Rust projection of
//! `contracts/json-schema/events/CallAnswered.schema.json`.

use serde::{Deserialize, Serialize};

use crate::event::EventPayload;
use crate::common::{Timestamp, Uuid};

/// Payload of the `CallAnswered` canonical event (Volume 5). Produced by SIP when a Call
/// is answered (`components.md`: SIP produces `CallAnswered`). `identity_id` is optional
/// (the answering Identity, when attributable); `answered_at` is required.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallAnswered {
    pub call_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity_id: Option<Uuid>,
    pub answered_at: Timestamp,
}

impl EventPayload for CallAnswered {
    const TYPE: &'static str = "CallAnswered";
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
    fn envelope_serialises_required_fields() {
        let call_id = Uuid::now_v7();
        let answered_at = Timestamp::now();
        let ctx = Correlation::root(Uuid::now_v7());
        let env = Envelope::new(
            CallAnswered { call_id, identity_id: None, answered_at },
            &ctx,
            "idem-1",
        );
        assert_eq!(env.event_type, "CallAnswered");
        assert_eq!(env.source, "/sip");
        let json = env.to_json();
        assert_eq!(json["data"]["call_id"], call_id.to_string());
        assert_eq!(json["data"]["answered_at"], answered_at.to_string());
        // Absent optional identity_id is omitted from the wire form.
        assert!(json["data"].get("identity_id").is_none());
    }
}
