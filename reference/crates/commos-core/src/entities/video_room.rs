//! `VideoRoom` entity — Rust projection of
//! `contracts/json-schema/entities/VideoRoom.schema.json`.
//!
//! A VideoRoom is the real-time video workload's durable session surface (Volume 2 Video
//! Workload; `workloads.md`). It is a *peer* workload entity to `Call` and `Channel` on the
//! same substrate (CMOS-02-DOM-100), proving the substrate carries real-time media beyond
//! voice/messaging. `mode` selects the media topology binding only (SFU vs. P2P) and MUST
//! NOT change the entity contract.

use serde::{Deserialize, Serialize};

use crate::common::{EntityBase, Uuid};

/// Media topology of a VideoRoom (`VideoRoom.schema.json` `mode`). `SFU` routes through a
/// selective-forwarding unit (Media Plane §Conferencing); `P2P` is a direct mesh.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoMode {
    #[serde(rename = "SFU")]
    Sfu,
    #[serde(rename = "P2P")]
    P2p,
}

/// VideoRoom lifecycle state (`VideoRoom.schema.json` `state`; Volume 2 Video Workload).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum VideoRoomState {
    Active,
    Ended,
}

impl VideoRoomState {
    /// `ENDED` is terminal — a torn-down room is a sink (CMOS-02-DOM-003).
    pub fn is_terminal(self) -> bool {
        matches!(self, VideoRoomState::Ended)
    }

    /// Whether `self -> next` is legal (`ACTIVE → ENDED`, and `ENDED` is a sink).
    pub fn can_transition_to(self, next: VideoRoomState) -> bool {
        matches!((self, next), (VideoRoomState::Active, VideoRoomState::Ended))
    }
}

/// The VideoRoom entity. `EntityBase` is flattened so the wire shape is
/// `allOf: [EntityBase] + VideoRoom properties`, matching the schema. Only `mode` and
/// `state` are required; `name` and `participants` are optional.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VideoRoom {
    #[serde(flatten)]
    pub base: EntityBase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub mode: VideoMode,
    pub state: VideoRoomState,
    /// Participant session references (resolve to Identities/Devices via the attribution
    /// chain, CMOS-02-DOM-113).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub participants: Vec<String>,
}

impl VideoRoom {
    /// Start a new VideoRoom in the `ACTIVE` state (Volume 5: `VideoRoomStarted`).
    pub fn start(tenant_id: Uuid, mode: VideoMode) -> Self {
        VideoRoom {
            base: EntityBase::new(tenant_id),
            name: None,
            mode,
            state: VideoRoomState::Active,
            participants: Vec::new(),
        }
    }

    /// End the VideoRoom, bumping the entity version (CMOS-02-DOM-005). Returns `false`
    /// if the room is already ended (the transition is illegal).
    pub fn end(&mut self) -> bool {
        if !self.state.can_transition_to(VideoRoomState::Ended) {
            return false;
        }
        self.state = VideoRoomState::Ended;
        self.base.touch();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_starts_active_v0() {
        let t = Uuid::now_v7();
        let room = VideoRoom::start(t, VideoMode::Sfu);
        assert_eq!(room.state, VideoRoomState::Active);
        assert_eq!(room.base.version, 0);
        assert_eq!(room.base.tenant_id, t);
    }

    #[test]
    fn serialised_field_names_match_schema() {
        let mut room = VideoRoom::start(Uuid::now_v7(), VideoMode::P2p);
        room.name = Some("standup".into());
        room.participants = vec!["sip:100".into()];
        let json = serde_json::to_value(&room).unwrap();
        // Flattened EntityBase + VideoRoom properties, faithful casing.
        assert_eq!(json["mode"], "P2P");
        assert_eq!(json["state"], "ACTIVE");
        assert_eq!(json["name"], "standup");
        assert_eq!(json["participants"][0], "sip:100");
        assert!(json.get("id").is_some());
        assert!(json.get("tenant_id").is_some());
        // Round-trips.
        let back: VideoRoom = serde_json::from_value(json).unwrap();
        assert_eq!(back.mode, VideoMode::P2p);
        assert_eq!(back.state, VideoRoomState::Active);
    }

    #[test]
    fn end_is_terminal() {
        let mut room = VideoRoom::start(Uuid::now_v7(), VideoMode::Sfu);
        assert!(room.end());
        assert_eq!(room.base.version, 1);
        assert!(room.state.is_terminal());
        // Ending again is illegal.
        assert!(!room.end());
    }
}
