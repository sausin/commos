//! `Call` entity — Rust projection of `contracts/json-schema/entities/Call.schema.json`.
//!
//! A Call is one workload instance: a signalling + media session (Volume 2). The state
//! set and transitions are the frozen contract; the state machine is enforced in
//! [`Call::transition`] so an implementation cannot move a Call into an illegal state.

use serde::{Deserialize, Serialize};

use crate::common::{EntityBase, Timestamp, Uuid};

/// Call direction (`Call.schema.json` `direction`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Direction {
    Inbound,
    Outbound,
    Internal,
}

/// Call lifecycle state (`Call.schema.json` `state`; Volume 2 state machine).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CallState {
    Initiated,
    Ringing,
    Answered,
    Held,
    Ended,
    Failed,
    NoAnswer,
    Busy,
    Rejected,
}

impl CallState {
    /// Terminal states have no successor (Volume 2: history is append-only, a Call ends once).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            CallState::Ended
                | CallState::Failed
                | CallState::NoAnswer
                | CallState::Busy
                | CallState::Rejected
        )
    }

    /// Whether `self -> next` is a legal transition (Volume 2 Call state machine).
    pub fn can_transition_to(self, next: CallState) -> bool {
        use CallState::*;
        match self {
            Initiated => matches!(next, Ringing | Answered | Failed | Rejected | Busy | NoAnswer),
            Ringing => matches!(next, Answered | NoAnswer | Busy | Rejected | Failed | Ended),
            Answered => matches!(next, Held | Ended | Failed),
            Held => matches!(next, Answered | Ended | Failed),
            // Terminal states are sinks.
            _ => false,
        }
    }
}

/// Media kind (`Call.schema.json` `media[].kind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum MediaKind {
    Audio,
    Video,
    Application,
}

/// Media direction (`Call.schema.json` `media[].direction`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum MediaDirection {
    Sendrecv,
    Sendonly,
    Recvonly,
    Inactive,
}

/// One negotiated media line on the Call.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MediaLine {
    pub kind: MediaKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    pub direction: MediaDirection,
}

/// The Call entity. `EntityBase` is flattened so the wire shape is
/// `allOf: [EntityBase] + Call properties`, matching the schema.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Call {
    #[serde(flatten)]
    pub base: EntityBase,
    pub direction: Direction,
    pub from_ref: String,
    pub to_ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity_id: Option<Uuid>,
    /// Groups the Call's events into one causal chain (mirrors the envelope `correlation_id`).
    pub correlation_id: Uuid,
    pub state: CallState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answered_at: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hangup_cause: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<MediaLine>,
}

/// Error returned when a state transition would violate the Call state machine.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("illegal Call transition {from:?} -> {to:?}")]
pub struct IllegalTransition {
    pub from: CallState,
    pub to: CallState,
}

impl Call {
    /// Originate a new Call in the `INITIATED` state (Routing subsystem, CMOS-03-ARCH).
    pub fn originate(
        tenant_id: Uuid,
        direction: Direction,
        from_ref: impl Into<String>,
        to_ref: impl Into<String>,
    ) -> Self {
        let base = EntityBase::new(tenant_id);
        Call {
            correlation_id: Uuid::now_v7(),
            base,
            direction,
            from_ref: from_ref.into(),
            to_ref: to_ref.into(),
            device_id: None,
            identity_id: None,
            state: CallState::Initiated,
            answered_at: None,
            ended_at: None,
            hangup_cause: None,
            media: Vec::new(),
        }
    }

    /// Apply a state transition, enforcing the frozen state machine and bumping the
    /// entity version (CMOS-02-DOM-005). Timestamps for `answered`/`ended` are stamped
    /// as the contract requires.
    pub fn transition(&mut self, to: CallState) -> Result<(), IllegalTransition> {
        if !self.state.can_transition_to(to) {
            return Err(IllegalTransition { from: self.state, to });
        }
        match to {
            CallState::Answered if self.answered_at.is_none() => {
                self.answered_at = Some(Timestamp::now());
            }
            _ if to.is_terminal() => {
                self.ended_at = Some(Timestamp::now());
            }
            _ => {}
        }
        self.state = to;
        self.base.touch();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn originate_starts_initiated_v0() {
        let t = Uuid::now_v7();
        let call = Call::originate(t, Direction::Outbound, "sip:100", "+14155550100");
        assert_eq!(call.state, CallState::Initiated);
        assert_eq!(call.base.version, 0);
        assert_eq!(call.base.tenant_id, t);
    }

    #[test]
    fn legal_transition_bumps_version_and_stamps() {
        let mut call =
            Call::originate(Uuid::now_v7(), Direction::Inbound, "+14155550100", "sip:100");
        call.transition(CallState::Ringing).unwrap();
        call.transition(CallState::Answered).unwrap();
        assert!(call.answered_at.is_some());
        assert_eq!(call.base.version, 2);
        call.transition(CallState::Ended).unwrap();
        assert!(call.ended_at.is_some());
        assert!(call.state.is_terminal());
    }

    #[test]
    fn illegal_transition_is_rejected() {
        let mut call =
            Call::originate(Uuid::now_v7(), Direction::Internal, "sip:100", "sip:200");
        // Reach a terminal state legally (INITIATED -> REJECTED)...
        call.transition(CallState::Rejected).unwrap();
        // ...then a rejected call cannot be answered.
        let err = call.transition(CallState::Answered).unwrap_err();
        assert_eq!(err.from, CallState::Rejected);
    }
}
