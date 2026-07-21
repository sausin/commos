//! `UserActivated` event — Rust projection of
//! `contracts/json-schema/events/UserActivated.schema.json`.

use serde::{Deserialize, Serialize};

use crate::common::Uuid;
use crate::event::EventPayload;

/// Payload of the `UserActivated` canonical event (Volume 5). Produced by the Identity
/// subsystem when a User transitions into the `ACTIVE` state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserActivated {
    pub user_id: Uuid,
}

impl EventPayload for UserActivated {
    const TYPE: &'static str = "UserActivated";
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
        let env = Envelope::new(UserActivated { user_id }, &ctx, "idem-1");
        assert_eq!(env.event_type, "UserActivated");
        assert_eq!(env.source, "/identity");
        assert_eq!(env.subject, user_id.to_string());
        let json = env.to_json();
        assert_eq!(json["data"]["user_id"], user_id.to_string());
    }
}
