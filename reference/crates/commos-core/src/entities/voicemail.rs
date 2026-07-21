//! `Voicemail` entity — Rust projection of
//! `contracts/json-schema/entities/Voicemail.schema.json`.
//!
//! A Voicemail is a message a caller left when a callee did not answer (Volume 2). It links
//! the [`Call`](super::call::Call) that produced it to the stored audio
//! [`Object`](super::object::Object) (`ObjectKind::Voicemail`), records the owning `user_id`
//! (the mailbox), how long it runs, whether it has been `read`, and an optional transcript
//! object once one exists (Volume 11 AI is an external consumer). The audio bytes live in
//! Object storage; this entity is the queryable, mutable record (the `read` flag versions
//! forward on retrieval — CMOS-02-DOM-005).

use serde::{Deserialize, Serialize};

use crate::common::{EntityBase, Uuid};

/// The Voicemail entity. `object_id` (the stored audio) is the only required field; a fresh
/// voicemail starts unread.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Voicemail {
    #[serde(flatten)]
    pub base: EntityBase,
    /// The mailbox owner this voicemail is for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
    /// The Call that produced the voicemail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<Uuid>,
    /// The stored audio Object holding the (as-recorded, un-transcoded) media.
    pub object_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_object_id: Option<Uuid>,
    /// Whether the mailbox owner has listened to it. Drives the message-waiting indicator.
    pub read: bool,
}

impl Voicemail {
    /// Create an unread Voicemail for a stored audio object. Callers set `user_id`/`call_id`/
    /// `duration_ms` on the returned value.
    pub fn new(tenant: Uuid, object_id: Uuid) -> Self {
        Voicemail {
            base: EntityBase::new(tenant),
            user_id: None,
            call_id: None,
            object_id,
            duration_ms: None,
            transcript_object_id: None,
            read: false,
        }
    }

    /// Mark the voicemail read, advancing its version (optimistic concurrency). Returns
    /// whether the flag actually changed — an already-read voicemail is left untouched so a
    /// repeat retrieval is not a spurious write (SD-card longevity, CMOS-14-DEP-021).
    pub fn mark_read(&mut self) -> bool {
        if self.read {
            return false;
        }
        self.read = true;
        self.base.touch();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialises_object_id_and_read_flag_and_omits_empty_optionals() {
        let obj = Uuid::now_v7();
        let vm = Voicemail::new(Uuid::now_v7(), obj);
        let j = serde_json::to_value(&vm).unwrap();
        assert_eq!(j["object_id"], obj.to_string());
        assert_eq!(j["read"], false);
        assert!(j.get("user_id").is_none());
        assert!(j.get("call_id").is_none());
        assert!(j.get("duration_ms").is_none());
        assert!(j.get("transcript_object_id").is_none());
    }

    #[test]
    fn round_trips_with_user_call_and_duration() {
        let mut vm = Voicemail::new(Uuid::now_v7(), Uuid::now_v7());
        vm.user_id = Some(Uuid::now_v7());
        vm.call_id = Some(Uuid::now_v7());
        vm.duration_ms = Some(15000);
        let back: Voicemail = serde_json::from_value(serde_json::to_value(&vm).unwrap()).unwrap();
        assert_eq!(back.user_id, vm.user_id);
        assert_eq!(back.call_id, vm.call_id);
        assert_eq!(back.duration_ms, Some(15000));
        assert!(!back.read);
    }

    #[test]
    fn mark_read_versions_forward_once() {
        let mut vm = Voicemail::new(Uuid::now_v7(), Uuid::now_v7());
        assert_eq!(vm.base.version, 0);
        assert!(vm.mark_read(), "first mark flips the flag");
        assert!(vm.read);
        assert_eq!(vm.base.version, 1);
        // Idempotent: a second mark is a no-op (no spurious version bump / write).
        assert!(!vm.mark_read());
        assert_eq!(vm.base.version, 1);
    }
}
