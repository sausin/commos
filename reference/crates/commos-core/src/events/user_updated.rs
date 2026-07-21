//! `UserUpdated` event — Rust projection of
//! `contracts/json-schema/events/UserUpdated.schema.json`.

use serde::{Deserialize, Serialize};

use crate::common::Uuid;
use crate::event::EventPayload;

/// Payload of the `UserUpdated` canonical event (Volume 5). Produced by the Identity
/// subsystem when a User's mutable attributes change; `changed` lists the field names.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserUpdated {
    pub user_id: Uuid,
    pub changed: Vec<String>,
}

impl EventPayload for UserUpdated {
    const TYPE: &'static str = "UserUpdated";
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
            UserUpdated {
                user_id,
                changed: vec!["display_name".into()],
            },
            &ctx,
            "idem-1",
        );
        assert_eq!(env.event_type, "UserUpdated");
        assert_eq!(env.source, "/identity");
        assert_eq!(env.subject, user_id.to_string());
        let json = env.to_json();
        assert_eq!(json["data"]["user_id"], user_id.to_string());
    }
}
