//! `PresenceChanged` event — Rust projection of
//! `contracts/json-schema/events/PresenceChanged.schema.json`.

use serde::{Deserialize, Serialize};

use crate::common::Uuid;
use crate::entities::presence_state::PresenceStatus;
use crate::event::EventPayload;

/// Payload of the `PresenceChanged` canonical event (Volume 5). Produced by the presence
/// subsystem when a user's availability changes (`workloads.md`: Presence Workload;
/// Presence component in `components.md`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PresenceChanged {
    pub user_id: Uuid,
    pub status: PresenceStatus,
}

impl EventPayload for PresenceChanged {
    const TYPE: &'static str = "PresenceChanged";
    // The presence subsystem is the emitting source (Presence component, `components.md`).
    const SOURCE: &'static str = "/presence";

    fn subject(&self) -> String {
        // The event is about the user whose presence changed.
        self.user_id.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Correlation, Envelope};

    #[test]
    fn envelope_carries_type_source_subject() {
        let user_id = Uuid::now_v7();
        let ctx = Correlation::root(Uuid::now_v7());
        let env = Envelope::new(
            PresenceChanged { user_id, status: PresenceStatus::Away },
            &ctx,
            "idem-1",
        );
        assert_eq!(env.event_type, "PresenceChanged");
        assert_eq!(env.source, "/presence");
        assert_eq!(env.subject, user_id.to_string());
        let json = env.to_json();
        assert_eq!(json["data"]["user_id"], user_id.to_string());
        assert_eq!(json["data"]["status"], "AWAY");
    }
}
