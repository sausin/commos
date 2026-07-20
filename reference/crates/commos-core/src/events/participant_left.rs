//! `ParticipantLeft` event — Rust projection of
//! `contracts/json-schema/events/ParticipantLeft.schema.json`.

use serde::{Deserialize, Serialize};

use crate::common::Uuid;
use crate::event::EventPayload;

/// Payload of the `ParticipantLeft` canonical event (Volume 5). Produced by the media
/// subsystem when a Participant leaves a session (`workloads.md`: Video Workload).
/// `session_ref` is required; `identity_id` is the optional resolved identity.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ParticipantLeft {
    pub session_ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity_id: Option<Uuid>,
}

impl EventPayload for ParticipantLeft {
    const TYPE: &'static str = "ParticipantLeft";
    // The media subsystem is the emitting source (Media Plane, `components.md`).
    const SOURCE: &'static str = "/media";

    fn subject(&self) -> String {
        // The event is about the leaving session participant.
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
        let env = Envelope::new(
            ParticipantLeft {
                session_ref: "sip:100".into(),
                identity_id: None,
            },
            &ctx,
            "idem-1",
        );
        assert_eq!(env.event_type, "ParticipantLeft");
        assert_eq!(env.source, "/media");
        assert_eq!(env.subject, "sip:100");
        let json = env.to_json();
        assert_eq!(json["data"]["session_ref"], "sip:100");
        assert!(json["data"].get("identity_id").is_none());
    }
}
