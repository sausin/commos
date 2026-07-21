//! Real-time (control plane) — creates the video/presence workloads' entities and emits
//! their canonical events (`workloads.md`: Video & Presence Workloads; Volume 5:
//! `VideoRoomStarted` / `PresenceChanged`).
//!
//! This is the real-time *peer* of [`crate::control::routing::Routing`] and
//! [`crate::control::messaging::MessagingService`] on the same substrate (CMOS-02-DOM-100):
//! the same commit-entity-with-its-event-atomically spine, proving the platform carries
//! real-time workloads beyond voice/messaging. Scope here is START/SET + READ (MVP).

use std::sync::Arc;

use commos_core::common::Uuid;
use commos_core::entities::presence_state::{PresenceState, PresenceStatus};
use commos_core::entities::video_room::{VideoMode, VideoRoom};
use commos_core::event::{Correlation, Envelope};
use commos_core::events::presence_changed::PresenceChanged;
use commos_core::events::video_room_started::VideoRoomStarted;

use crate::relay::RelaySignal;
use crate::store::{Store, StoreError, Tx};

/// The Real-time service. Stateless between requests — all state lives in the [`Store`]
/// (CMOS-03-ARCH-010), so any node can serve any request.
#[derive(Clone)]
pub struct RealtimeService {
    store: Arc<dyn Store>,
    signal: RelaySignal,
}

impl RealtimeService {
    pub fn new(store: Arc<dyn Store>, signal: RelaySignal) -> Self {
        RealtimeService { store, signal }
    }

    /// Start a VideoRoom in `ACTIVE`, emit `VideoRoomStarted`, commit both atomically.
    pub async fn start_video_room(
        &self,
        tenant: Uuid,
        name: Option<String>,
    ) -> Result<VideoRoom, StoreError> {
        // SFU is the default media topology (Media Plane §Conferencing).
        let mut room = VideoRoom::start(tenant, VideoMode::Sfu);
        room.name = name;

        // Fresh correlation chain rooted at this VideoRoom's start.
        let ctx = Correlation::root(tenant);
        let payload = VideoRoomStarted {
            room_id: room.base.id,
            mode: room.mode,
        };
        let idem = format!("{}:VideoRoomStarted", room.base.id);
        let envelope = Envelope::new(payload, &ctx, idem);

        self.store
            .commit(Tx {
                video_rooms: vec![room.clone()],
                events: vec![envelope.to_json()],
                ..Default::default()
            })
            .await?;
        self.signal.wake();

        Ok(room)
    }

    /// Record a user's current presence, emit `PresenceChanged`, commit both atomically.
    ///
    /// The subject is the schema's `user_id` (a UUIDv7), not a free-form ref. For the MVP
    /// each call inserts a fresh PresenceState row at version 0; a fuller upsert-by-subject
    /// (one live row per user that versions forward) is a later refinement.
    pub async fn set_presence(
        &self,
        tenant: Uuid,
        user_id: Uuid,
        status: PresenceStatus,
    ) -> Result<PresenceState, StoreError> {
        let presence = PresenceState::set(tenant, user_id, status);

        let ctx = Correlation::root(tenant);
        let payload = PresenceChanged {
            user_id: presence.user_id,
            status: presence.status,
        };
        let idem = format!("{}:PresenceChanged", presence.base.id);
        let envelope = Envelope::new(payload, &ctx, idem);

        self.store
            .commit(Tx {
                presence: vec![presence.clone()],
                events: vec![envelope.to_json()],
                ..Default::default()
            })
            .await?;
        self.signal.wake();

        Ok(presence)
    }
}
