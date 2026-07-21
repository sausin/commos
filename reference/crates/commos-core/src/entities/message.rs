//! `Message` entity — Rust projection of
//! `contracts/json-schema/entities/Message.schema.json`.
//!
//! A Message is a single utterance in the messaging workload (Volume 2 §2 Messaging
//! Workload; `workloads.md`). It MUST reference its owning Channel and MAY reference a
//! Thread whose `channel_id` equals the Message's (CMOS-02-DOM-110). `attachments[]` MUST
//! contain **Object** identifiers only — never raw bytes (CMOS-02-DOM-111). A messaging
//! peer of the voice `Call` on the same substrate.

use serde::{Deserialize, Serialize};

use crate::common::{EntityBase, Uuid};

/// Message delivery state (`Message.schema.json` `state`; Volume 2 §2.3 Message).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MessageState {
    Sent,
    Delivered,
    Read,
    Failed,
}

impl MessageState {
    /// Terminal delivery states (Volume 2 §2.3: `READ` and `FAILED` are sinks).
    pub fn is_terminal(self) -> bool {
        matches!(self, MessageState::Read | MessageState::Failed)
    }

    /// Whether `self -> next` is legal (Volume 2 §2.3 / CMOS-02-DOM-112:
    /// `SENT → DELIVERED → READ`, with `SENT`/`DELIVERED → FAILED`).
    pub fn can_transition_to(self, next: MessageState) -> bool {
        use MessageState::*;
        match self {
            Sent => matches!(next, Delivered | Failed),
            Delivered => matches!(next, Read | Failed),
            // Terminal states are sinks.
            Read | Failed => false,
        }
    }
}

/// The Message entity. `EntityBase` is flattened so the wire shape is
/// `allOf: [EntityBase] + Message properties`, matching the schema. `channel_id`,
/// `sender_ref` and `state` are required; `thread_id`, `body` and `attachments` are
/// optional.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    #[serde(flatten)]
    pub base: EntityBase,
    /// Owning Channel (CMOS-02-DOM-110).
    pub channel_id: Uuid,
    /// Owning Thread; a Channel-level message MAY omit it (CMOS-02-DOM-110).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<Uuid>,
    /// Sender reference; resolves through the attribution chain (CMOS-02-DOM-113).
    pub sender_ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// **Object** identifiers only — never raw bytes (CMOS-02-DOM-111).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Uuid>,
    pub state: MessageState,
}

impl Message {
    /// Accept a Message for transport in the `SENT` state (Volume 2 §2.3: `MessageSent`).
    pub fn send(tenant_id: Uuid, channel_id: Uuid, sender_ref: impl Into<String>) -> Self {
        Message {
            base: EntityBase::new(tenant_id),
            channel_id,
            thread_id: None,
            sender_ref: sender_ref.into(),
            body: None,
            attachments: Vec::new(),
            state: MessageState::Sent,
        }
    }

    /// Advance the delivery state, enforcing the §2.3 state machine and bumping the
    /// version (CMOS-02-DOM-005). Returns `false` on an illegal transition.
    pub fn transition(&mut self, to: MessageState) -> bool {
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
    fn send_starts_sent_v0() {
        let t = Uuid::now_v7();
        let m = Message::send(t, Uuid::now_v7(), "sip:100");
        assert_eq!(m.state, MessageState::Sent);
        assert_eq!(m.base.version, 0);
    }

    #[test]
    fn serialised_field_names_match_schema() {
        let channel_id = Uuid::now_v7();
        let thread_id = Uuid::now_v7();
        let object_id = Uuid::now_v7();
        let mut m = Message::send(Uuid::now_v7(), channel_id, "sip:100");
        m.thread_id = Some(thread_id);
        m.body = Some("hi".into());
        m.attachments = vec![object_id];
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["channel_id"], channel_id.to_string());
        assert_eq!(json["thread_id"], thread_id.to_string());
        assert_eq!(json["sender_ref"], "sip:100");
        assert_eq!(json["body"], "hi");
        assert_eq!(json["attachments"][0], object_id.to_string());
        assert_eq!(json["state"], "SENT");
        // Round-trips.
        let back: Message = serde_json::from_value(json).unwrap();
        assert_eq!(back.channel_id, channel_id);
        assert_eq!(back.thread_id, Some(thread_id));
        assert_eq!(back.state, MessageState::Sent);
    }

    #[test]
    fn delivery_state_machine_is_enforced() {
        let mut m = Message::send(Uuid::now_v7(), Uuid::now_v7(), "sip:100");
        assert!(m.transition(MessageState::Delivered));
        assert!(m.transition(MessageState::Read));
        assert_eq!(m.base.version, 2);
        assert!(m.state.is_terminal());
        // A READ message cannot fail.
        assert!(!m.transition(MessageState::Failed));
    }

    #[test]
    fn sent_can_fail_directly() {
        let mut m = Message::send(Uuid::now_v7(), Uuid::now_v7(), "sip:100");
        assert!(m.transition(MessageState::Failed));
        assert!(m.state.is_terminal());
        // Cannot resurrect a failed message.
        assert!(!m.transition(MessageState::Delivered));
    }
}
