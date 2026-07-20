//! `ParticipantJoined` event — Rust projection of
//! `contracts/json-schema/events/ParticipantJoined.schema.json`.

use serde::{Deserialize, Serialize};

use crate::common::Uuid;
use crate::event::EventPayload;

/// Payload of the `ParticipantJoined` canonical event (Volume 5). Produced by the media
/// subsystem when a Participant joins a session (`workloads.md`: Video Workload).
/// `session_ref` is required; `identity_id` is the optional resolved identity.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ParticipantJoined {
    pub session_ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity_id: Option<Uuid>,
}

impl EventPayload for ParticipantJoined {
    const TYPE: &'static str = "ParticipantJoined";
    // The media subsystem is the emitting source (Media Plane, `components.md`).
    const SOURCE: &'static str = "/media";

    fn subject(&self) -> String {
        // The event is about the joining session participant.
        self.session_ref.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Correlation, Envelope};

    #[test]
    fn envelope_carries_type_source_subject() {
        let ctx = Correlation::root(Uuid::now_v7());
        let identity_id = Uuid::now_v7();
        let env = Envelope::new(
            ParticipantJoined {
                session_ref: "sip:100".into(),
                identity_id: Some(identity_id),
            },
            &ctx,
            "idem-1",
        );
        assert_eq!(env.event_type, "ParticipantJoined");
        assert_eq!(env.source, "/media");
        assert_eq!(env.subject, "sip:100");
        let json = env.to_json();
        assert_eq!(json["data"]["session_ref"], "sip:100");
        assert_eq!(json["data"]["identity_id"], identity_id.to_string());
    }
}
