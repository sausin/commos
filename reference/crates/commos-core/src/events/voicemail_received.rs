//! `VoicemailReceived` event — Rust projection of
//! `contracts/json-schema/events/VoicemailReceived.schema.json`.

use serde::{Deserialize, Serialize};

use crate::common::Uuid;
use crate::event::EventPayload;

/// Payload of the `VoicemailReceived` canonical event (Volume 5). Emitted when a caller has
/// left a voicemail — the audio is captured and stored as an Object and a [`Voicemail`] links
/// it to the mailbox.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VoicemailReceived {
    pub voicemail_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
    pub object_id: Uuid,
}

impl EventPayload for VoicemailReceived {
    const TYPE: &'static str = "VoicemailReceived";
    // The media plane captures and stores the voicemail audio (as with recordings).
    const SOURCE: &'static str = "/media";

    fn subject(&self) -> String {
        self.voicemail_id.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Correlation, Envelope};

    #[test]
    fn envelope_carries_type_source_subject() {
        let voicemail_id = Uuid::now_v7();
        let ctx = Correlation::root(Uuid::now_v7());
        let env = Envelope::new(
            VoicemailReceived {
                voicemail_id,
                user_id: Some(Uuid::now_v7()),
                object_id: Uuid::now_v7(),
            },
            &ctx,
            "idem-vm",
        );
        assert_eq!(env.event_type, "VoicemailReceived");
        assert_eq!(env.source, "/media");
        assert_eq!(env.subject, voicemail_id.to_string());
        assert_eq!(env.to_json()["data"]["voicemail_id"], voicemail_id.to_string());
    }

    #[test]
    fn omits_user_id_when_absent() {
        let ctx = Correlation::root(Uuid::now_v7());
        let env = Envelope::new(
            VoicemailReceived { voicemail_id: Uuid::now_v7(), user_id: None, object_id: Uuid::now_v7() },
            &ctx,
            "idem-vm2",
        );
        assert!(env.to_json()["data"].get("user_id").is_none());
    }
}
