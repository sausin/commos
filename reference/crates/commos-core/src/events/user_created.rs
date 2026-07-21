//! `UserCreated` event — Rust projection of
//! `contracts/json-schema/events/UserCreated.schema.json`.

use serde::{Deserialize, Serialize};

use crate::common::Uuid;
use crate::entities::user::UserState;
use crate::event::EventPayload;

/// Payload of the `UserCreated` canonical event (Volume 5). Produced by the Identity
/// subsystem when a User is created.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserCreated {
    pub user_id: Uuid,
    pub display_name: String,
    pub state: UserState,
}

impl EventPayload for UserCreated {
    const TYPE: &'static str = "UserCreated";
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
            UserCreated {
                user_id,
                display_name: "Ada Lovelace".into(),
                state: UserState::Active,
            },
            &ctx,
            "idem-1",
        );
        assert_eq!(env.event_type, "UserCreated");
        assert_eq!(env.source, "/identity");
        assert_eq!(env.subject, user_id.to_string());
        let json = env.to_json();
        assert_eq!(json["data"]["user_id"], user_id.to_string());
    }
}
