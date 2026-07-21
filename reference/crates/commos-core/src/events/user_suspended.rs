//! `UserSuspended` event — Rust projection of
//! `contracts/json-schema/events/UserSuspended.schema.json`.

use serde::{Deserialize, Serialize};

use crate::common::Uuid;
use crate::event::EventPayload;

/// Payload of the `UserSuspended` canonical event (Volume 5). Produced by the Identity
/// subsystem when a User transitions into the `SUSPENDED` state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserSuspended {
    pub user_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl EventPayload for UserSuspended {
    const TYPE: &'static str = "UserSuspended";
    // Identity is the emitting subsystem for User lifecycle events.
    const SOURCE: &'static str = "/identity";

    fn subject(&self) -> String {
        // The event is about the User.
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
            UserSuspended {
                user_id,
                reason: None,
            },
            &ctx,
            "idem-1",
        );
        assert_eq!(env.event_type, "UserSuspended");
        assert_eq!(env.source, "/identity");
        assert_eq!(env.subject, user_id.to_string());
        let json = env.to_json();
        assert_eq!(json["data"]["user_id"], user_id.to_string());
    }
}
