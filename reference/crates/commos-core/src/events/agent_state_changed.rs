//! `AgentStateChanged` event — Rust projection of
//! `contracts/json-schema/events/AgentStateChanged.schema.json`.

use serde::{Deserialize, Serialize};

use crate::common::Uuid;
use crate::event::EventPayload;

/// Payload of the `AgentStateChanged` canonical event (Volume 5). Produced by Routing when
/// a contact-centre agent changes live availability (e.g. `AVAILABLE` → `BUSY`). Agent
/// live-state is ephemeral runtime state (like SIP registrations), but every *observable*
/// transition still surfaces as a canonical event so downstream consumers (wallboards,
/// reporting) stay in sync.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentStateChanged {
    pub agent_user_id: Uuid,
    pub state: String,
}

impl EventPayload for AgentStateChanged {
    const TYPE: &'static str = "AgentStateChanged";
    // Routing (ACD) is the emitting subsystem for agent state transitions.
    const SOURCE: &'static str = "/routing";

    fn subject(&self) -> String {
        // The event is about the agent (their Identity user id).
        self.agent_user_id.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Correlation, Envelope};

    #[test]
    fn envelope_carries_type_source_subject() {
        let agent_user_id = Uuid::now_v7();
        let ctx = Correlation::root(Uuid::now_v7());
        let env = Envelope::new(
            AgentStateChanged {
                agent_user_id,
                state: "AVAILABLE".into(),
            },
            &ctx,
            "idem-1",
        );
        assert_eq!(env.event_type, "AgentStateChanged");
        assert_eq!(env.source, "/routing");
        assert_eq!(env.subject, agent_user_id.to_string());
        let json = env.to_json();
        assert_eq!(json["data"]["agent_user_id"], agent_user_id.to_string());
        assert_eq!(json["data"]["state"], "AVAILABLE");
    }
}
