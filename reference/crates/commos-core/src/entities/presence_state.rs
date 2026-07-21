//! `PresenceState` entity — Rust projection of
//! `contracts/json-schema/entities/PresenceState.schema.json`.
//!
//! A PresenceState is the current availability of a user in the presence workload
//! (Volume 2 Presence Workload; `workloads.md`; Presence component in
//! `spec/003-architecture/components.md`). It is a *peer* workload entity to `Call` and
//! `Channel` on the same substrate (CMOS-02-DOM-100), proving the substrate carries
//! real-time presence beyond voice/messaging. `user_id` names the subject; `status`
//! is the current availability signal.

use serde::{Deserialize, Serialize};

use crate::common::{EntityBase, Timestamp, Uuid};

/// Availability signal of a user (`PresenceState.schema.json` `status`; Volume 2 Presence
/// Workload). `ON_CALL` is set by the voice workload while a Call is up, demonstrating
/// cross-workload composition on one substrate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PresenceStatus {
    Available,
    Busy,
    Away,
    Dnd,
    Offline,
    OnCall,
}

/// The PresenceState entity. `EntityBase` is flattened so the wire shape is
/// `allOf: [EntityBase] + PresenceState properties`, matching the schema. `user_id` and
/// `status` are required; `since` and `device_id` are optional.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PresenceState {
    #[serde(flatten)]
    pub base: EntityBase,
    /// The user this presence is about (the subject; CMOS-02-DOM-113).
    pub user_id: Uuid,
    pub status: PresenceStatus,
    /// When the current status took effect.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<Timestamp>,
    /// The device that reported the presence, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<Uuid>,
}

impl PresenceState {
    /// Record a user's current presence at version 0 (Volume 5: `PresenceChanged`). `since`
    /// is stamped to now, matching the moment this status took effect.
    pub fn set(tenant_id: Uuid, user_id: Uuid, status: PresenceStatus) -> Self {
        PresenceState {
            base: EntityBase::new(tenant_id),
            user_id,
            status,
            since: Some(Timestamp::now()),
            device_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_starts_v0() {
        let t = Uuid::now_v7();
        let user = Uuid::now_v7();
        let p = PresenceState::set(t, user, PresenceStatus::Available);
        assert_eq!(p.status, PresenceStatus::Available);
        assert_eq!(p.user_id, user);
        assert_eq!(p.base.version, 0);
        assert_eq!(p.base.tenant_id, t);
    }

    #[test]
    fn serialised_field_names_match_schema() {
        let user = Uuid::now_v7();
        let device = Uuid::now_v7();
        let mut p = PresenceState::set(Uuid::now_v7(), user, PresenceStatus::OnCall);
        p.device_id = Some(device);
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["user_id"], user.to_string());
        assert_eq!(json["status"], "ON_CALL");
        assert_eq!(json["device_id"], device.to_string());
        assert!(json.get("since").is_some());
        assert!(json.get("id").is_some());
        // Round-trips.
        let back: PresenceState = serde_json::from_value(json).unwrap();
        assert_eq!(back.user_id, user);
        assert_eq!(back.status, PresenceStatus::OnCall);
    }

    #[test]
    fn status_variants_serialise_faithfully() {
        let cases = [
            (PresenceStatus::Available, "AVAILABLE"),
            (PresenceStatus::Busy, "BUSY"),
            (PresenceStatus::Away, "AWAY"),
            (PresenceStatus::Dnd, "DND"),
            (PresenceStatus::Offline, "OFFLINE"),
            (PresenceStatus::OnCall, "ON_CALL"),
        ];
        for (status, wire) in cases {
            assert_eq!(serde_json::to_value(status).unwrap(), wire);
        }
    }
}
