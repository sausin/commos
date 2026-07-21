//! `Thread` entity — Rust projection of
//! `contracts/json-schema/entities/Thread.schema.json`.
//!
//! A Thread is an ordered conversation within a Channel (Volume 2 §2 Messaging Workload;
//! `workloads.md`). It MUST reference exactly one Channel via `channel_id`, in the same
//! tenant (CMOS-02-DOM-110). A messaging peer of the voice `Call` on the same substrate.

use serde::{Deserialize, Serialize};

use crate::common::{EntityBase, Uuid};

/// Thread lifecycle state (`Thread.schema.json` `state`; Volume 2 §2.3 Thread).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ThreadState {
    Open,
    Closed,
}

impl ThreadState {
    /// Whether `self -> next` is legal (Volume 2 §2.3 Thread state machine:
    /// `OPEN → CLOSED` and `CLOSED → OPEN`, both directions permitted). `CLOSED` is a
    /// soft-terminal state that MAY reopen on new activity.
    pub fn can_transition_to(self, next: ThreadState) -> bool {
        self != next
    }
}

/// The Thread entity. `EntityBase` is flattened so the wire shape is
/// `allOf: [EntityBase] + Thread properties`, matching the schema. `channel_id` and
/// `state` are required; `subject` is optional.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Thread {
    #[serde(flatten)]
    pub base: EntityBase,
    /// Owning Channel (same tenant, CMOS-02-DOM-110).
    pub channel_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub state: ThreadState,
}

impl Thread {
    /// Open a new Thread in a Channel (`OPEN`; Volume 2 §2.3: `ThreadOpened`).
    pub fn open(tenant_id: Uuid, channel_id: Uuid) -> Self {
        Thread {
            base: EntityBase::new(tenant_id),
            channel_id,
            subject: None,
            state: ThreadState::Open,
        }
    }

    /// Transition the Thread's state, enforcing the §2.3 state machine and bumping the
    /// version (CMOS-02-DOM-005). Returns `false` on an illegal (no-op) transition.
    pub fn transition(&mut self, to: ThreadState) -> bool {
        if !self.state.can_transition_to(to) {
            return false;
        }
        self.state = to;
        self.base.touch();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_starts_open_v0() {
        let t = Uuid::now_v7();
        let th = Thread::open(t, Uuid::now_v7());
        assert_eq!(th.state, ThreadState::Open);
        assert_eq!(th.base.version, 0);
    }

    #[test]
    fn serialised_field_names_match_schema() {
        let channel_id = Uuid::now_v7();
        let mut th = Thread::open(Uuid::now_v7(), channel_id);
        th.subject = Some("Billing question".into());
        let json = serde_json::to_value(&th).unwrap();
        assert_eq!(json["channel_id"], channel_id.to_string());
        assert_eq!(json["state"], "OPEN");
        assert_eq!(json["subject"], "Billing question");
        assert!(json.get("id").is_some());
        // Round-trips.
        let back: Thread = serde_json::from_value(json).unwrap();
        assert_eq!(back.channel_id, channel_id);
        assert_eq!(back.state, ThreadState::Open);
    }

    #[test]
    fn close_then_reopen_is_legal() {
        let mut th = Thread::open(Uuid::now_v7(), Uuid::now_v7());
        assert!(th.transition(ThreadState::Closed));
        assert!(th.transition(ThreadState::Open));
        assert_eq!(th.base.version, 2);
        // Same-state transition is a no-op / illegal.
        assert!(!th.transition(ThreadState::Open));
    }
}
