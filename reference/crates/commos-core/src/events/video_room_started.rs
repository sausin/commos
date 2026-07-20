//! `VideoRoomStarted` event — Rust projection of
//! `contracts/json-schema/events/VideoRoomStarted.schema.json`.

use serde::{Deserialize, Serialize};

use crate::common::Uuid;
use crate::entities::video_room::VideoMode;
use crate::event::EventPayload;

/// Payload of the `VideoRoomStarted` canonical event (Volume 5). Produced by the media
/// subsystem when a VideoRoom is started (`workloads.md`: Video Workload). `room_id` is
/// required; `mode` is the optional media-topology hint.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VideoRoomStarted {
    pub room_id: Uuid,
    pub mode: VideoMode,
}

impl EventPayload for VideoRoomStarted {
    const TYPE: &'static str = "VideoRoomStarted";
    // The media subsystem is the emitting source (Media Plane, `components.md`).
    const SOURCE: &'static str = "/media";

    fn subject(&self) -> String {
        // The event is about the VideoRoom.
        self.room_id.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Correlation, Envelope};

    #[test]
    fn envelope_carries_type_source_subject() {
        let room_id = Uuid::now_v7();
        let ctx = Correlation::root(Uuid::now_v7());
        let env = Envelope::new(
            VideoRoomStarted { room_id, mode: VideoMode::Sfu },
            &ctx,
            "idem-1",
        );
        assert_eq!(env.event_type, "VideoRoomStarted");
        assert_eq!(env.source, "/media");
        assert_eq!(env.subject, room_id.to_string());
        let json = env.to_json();
        assert_eq!(json["data"]["room_id"], room_id.to_string());
        assert_eq!(json["data"]["mode"], "SFU");
    }
}
