//! `CallEnded` event — Rust projection of
//! `contracts/json-schema/events/CallEnded.schema.json`.

use serde::{Deserialize, Serialize};

use crate::event::EventPayload;
use crate::common::{Timestamp, Uuid};

/// Payload of the `CallEnded` canonical event (Volume 5). Produced by SIP when a Call
/// terminates (`components.md`: SIP produces `CallEnded`; Billing consumes it).
/// `ended_at` and `duration_ms` are required; `hangup_cause` is optional.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallEnded {
    pub call_id: Uuid,
    pub ended_at: Timestamp,
    /// Call duration in milliseconds (schema `minimum: 0`).
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hangup_cause: Option<String>,
}

impl EventPayload for CallEnded {
    const TYPE: &'static str = "CallEnded";
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
    fn envelope_serialises_duration_and_ended_at() {
        let call_id = Uuid::now_v7();
        let ended_at = Timestamp::now();
        let ctx = Correlation::root(Uuid::now_v7());
        let env = Envelope::new(
            CallEnded { call_id, ended_at, duration_ms: 42_000, hangup_cause: Some("NORMAL_CLEARING".into()) },
            &ctx,
            "idem-1",
        );
        assert_eq!(env.event_type, "CallEnded");
        assert_eq!(env.source, "/sip");
        let json = env.to_json();
        assert_eq!(json["data"]["duration_ms"], 42_000);
        assert_eq!(json["data"]["ended_at"], ended_at.to_string());
        assert_eq!(json["data"]["hangup_cause"], "NORMAL_CLEARING");
    }
}
