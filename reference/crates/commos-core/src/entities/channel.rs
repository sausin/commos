//! `Channel` entity — Rust projection of
//! `contracts/json-schema/entities/Channel.schema.json`.
//!
//! A Channel is the messaging workload's durable conversation surface bound to a
//! transport kind (Volume 2 §2 Messaging Workload; `workloads.md`). It is a *peer*
//! workload entity to `Call` on the same substrate (CMOS-02-DOM-100), proving the
//! substrate is workload-general. `kind` selects the transport binding only and MUST NOT
//! change the entity contract (CMOS-02-DOM-114).

use serde::{Deserialize, Serialize};

use crate::common::{EntityBase, Uuid};

/// Transport binding of a Channel (`Channel.schema.json` `kind`; CMOS-02-DOM-114).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ChannelKind {
    Chat,
    Sms,
    Whatsapp,
    Email,
    Internal,
}

/// Channel lifecycle state (`Channel.schema.json` `state`; Volume 2 §2.3 Channel).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ChannelState {
    Active,
    Archived,
}

impl ChannelState {
    /// `ARCHIVED` is a soft-terminal state — history remains resolvable
    /// (CMOS-02-DOM-003; `workloads.md` §2.3 note).
    pub fn is_terminal(self) -> bool {
        matches!(self, ChannelState::Archived)
    }

    /// Whether `self -> next` is legal (Volume 2 §2.3 Channel state machine:
    /// `ACTIVE → ARCHIVED`, and `ARCHIVED` is a sink).
    pub fn can_transition_to(self, next: ChannelState) -> bool {
        matches!((self, next), (ChannelState::Active, ChannelState::Archived))
    }
}

/// The Channel entity. `EntityBase` is flattened so the wire shape is
/// `allOf: [EntityBase] + Channel properties`, matching the schema. Only `kind` and
/// `state` are required; `name` and `members` are optional.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Channel {
    #[serde(flatten)]
    pub base: EntityBase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub kind: ChannelKind,
    /// Member references (resolve to Devices/Identities via the attribution chain,
    /// CMOS-02-DOM-113).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
    pub state: ChannelState,
}

impl Channel {
    /// Open a new Channel in the `ACTIVE` state (Volume 2 §2.3: `ChannelCreated`).
    pub fn create(tenant_id: Uuid, kind: ChannelKind) -> Self {
        Channel {
            base: EntityBase::new(tenant_id),
            name: None,
            kind,
            members: Vec::new(),
            state: ChannelState::Active,
        }
    }

    /// Archive the Channel, bumping the entity version (CMOS-02-DOM-005). Returns `false`
    /// if the Channel is already archived (the transition is illegal).
    pub fn archive(&mut self) -> bool {
        if !self.state.can_transition_to(ChannelState::Archived) {
            return false;
        }
        self.state = ChannelState::Archived;
        self.base.touch();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_starts_active_v0() {
        let t = Uuid::now_v7();
        let ch = Channel::create(t, ChannelKind::Sms);
        assert_eq!(ch.state, ChannelState::Active);
        assert_eq!(ch.base.version, 0);
        assert_eq!(ch.base.tenant_id, t);
    }

    #[test]
    fn serialised_field_names_match_schema() {
        let mut ch = Channel::create(Uuid::now_v7(), ChannelKind::Whatsapp);
        ch.name = Some("support".into());
        ch.members = vec!["sip:100".into()];
        let json = serde_json::to_value(&ch).unwrap();
        // Flattened EntityBase + Channel properties, faithful casing.
        assert_eq!(json["kind"], "WHATSAPP");
        assert_eq!(json["state"], "ACTIVE");
        assert_eq!(json["name"], "support");
        assert_eq!(json["members"][0], "sip:100");
        assert!(json.get("id").is_some());
        assert!(json.get("tenant_id").is_some());
        // Round-trips.
        let back: Channel = serde_json::from_value(json).unwrap();
        assert_eq!(back.kind, ChannelKind::Whatsapp);
        assert_eq!(back.state, ChannelState::Active);
    }

    #[test]
    fn archive_is_soft_terminal() {
        let mut ch = Channel::create(Uuid::now_v7(), ChannelKind::Internal);
        assert!(ch.archive());
        assert_eq!(ch.base.version, 1);
        assert!(ch.state.is_terminal());
        // Archiving again is illegal.
        assert!(!ch.archive());
    }
}
