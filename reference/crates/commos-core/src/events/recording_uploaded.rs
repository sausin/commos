//! `RecordingUploaded` event — Rust projection of
//! `contracts/json-schema/events/RecordingUploaded.schema.json`.

use serde::{Deserialize, Serialize};

use crate::common::Uuid;
use crate::event::EventPayload;

/// Payload of the `RecordingUploaded` canonical event (Volume 5). Emitted when a call's audio
/// has been captured and stored as an Object.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordingUploaded {
    pub recording_id: Uuid,
    pub call_id: Uuid,
    pub object_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

impl EventPayload for RecordingUploaded {
    const TYPE: &'static str = "RecordingUploaded";
    // The media plane captures and stores the recording.
    const SOURCE: &'static str = "/media";

    fn subject(&self) -> String {
        self.recording_id.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Correlation, Envelope};

    #[test]
    fn envelope_carries_type_source_subject() {
        let recording_id = Uuid::now_v7();
        let ctx = Correlation::root(Uuid::now_v7());
        let env = Envelope::new(
            RecordingUploaded {
                recording_id,
                call_id: Uuid::now_v7(),
                object_id: Uuid::now_v7(),
                object_uri: Some("local://t/o".into()),
                bytes: Some(64000),
            },
            &ctx,
            "idem-rec",
        );
        assert_eq!(env.event_type, "RecordingUploaded");
        assert_eq!(env.source, "/media");
        assert_eq!(env.subject, recording_id.to_string());
        assert_eq!(env.to_json()["data"]["recording_id"], recording_id.to_string());
    }
}
