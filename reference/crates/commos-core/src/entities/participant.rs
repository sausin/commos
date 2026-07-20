//! `Participant` entity — Rust projection of
//! `contracts/json-schema/entities/Participant.schema.json`.
//!
//! A Participant is a single member of a real-time session (a VideoRoom or a Call) in the
//! video/presence workloads (Volume 2; `workloads.md`). It is a *peer* workload entity on
//! the same substrate (CMOS-02-DOM-100). `session_ref` binds the participant to its session
//! and `role` selects its capabilities within it.
//!
//! Projected faithfully into core; for the create+read MVP the platform represents a
//! VideoRoom's members inline via `VideoRoom.participants[]` (session refs), so this richer
//! entity is not yet given its own store table / API surface.

use serde::{Deserialize, Serialize};

use crate::common::{EntityBase, Timestamp, Uuid};

/// Role of a Participant within its session (`Participant.schema.json` `role`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ParticipantRole {
    Host,
    Guest,
    Agent,
    Observer,
}

/// The Participant entity. `EntityBase` is flattened so the wire shape is
/// `allOf: [EntityBase] + Participant properties`, matching the schema. `session_ref` and
/// `role` are required; `identity_id`, `joined_at` and `left_at` are optional.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Participant {
    #[serde(flatten)]
    pub base: EntityBase,
    /// The session (VideoRoom / Call) this participant belongs to.
    pub session_ref: String,
    /// Resolved identity, if attributed (CMOS-02-DOM-113).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity_id: Option<Uuid>,
    pub role: ParticipantRole,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub joined_at: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left_at: Option<Timestamp>,
}

impl Participant {
    /// Admit a Participant to a session in the given role (Volume 5: `ParticipantJoined`).
    /// `joined_at` is stamped to now.
    pub fn join(tenant_id: Uuid, session_ref: impl Into<String>, role: ParticipantRole) -> Self {
        Participant {
            base: EntityBase::new(tenant_id),
            session_ref: session_ref.into(),
            identity_id: None,
            role,
            joined_at: Some(Timestamp::now()),
            left_at: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_starts_v0() {
        let t = Uuid::now_v7();
        let p = Participant::join(t, "sip:100", ParticipantRole::Host);
        assert_eq!(p.role, ParticipantRole::Host);
        assert_eq!(p.base.version, 0);
        assert_eq!(p.base.tenant_id, t);
    }

    #[test]
    fn serialised_field_names_match_schema() {
        let identity = Uuid::now_v7();
        let mut p = Participant::join(Uuid::now_v7(), "sip:100", ParticipantRole::Guest);
        p.identity_id = Some(identity);
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["session_ref"], "sip:100");
        assert_eq!(json["role"], "GUEST");
        assert_eq!(json["identity_id"], identity.to_string());
        assert!(json.get("joined_at").is_some());
        assert!(json.get("left_at").is_none());
        assert!(json.get("id").is_some());
        // Round-trips.
        let back: Participant = serde_json::from_value(json).unwrap();
        assert_eq!(back.session_ref, "sip:100");
        assert_eq!(back.role, ParticipantRole::Guest);
    }
}
