//! `Recording` entity — Rust projection of
//! `contracts/json-schema/entities/Recording.schema.json`.
//!
//! A Recording links a [`Call`](super::call::Call) to the stored audio
//! [`Object`](super::object::Object) that captured it (CMOS-02-DOM-013). The bytes live in
//! Object storage; this entity is the queryable record — which call, which object, how long,
//! and an optional transcript object once one exists (Volume 11 AI is an external consumer).

use serde::{Deserialize, Serialize};

use crate::common::{EntityBase, Uuid};

/// The Recording entity. `object_id` (the stored audio) is the only required field.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Recording {
    #[serde(flatten)]
    pub base: EntityBase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<Uuid>,
    /// The stored audio Object holding the (as-recorded, un-transcoded) media.
    pub object_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_object_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

impl Recording {
    /// Create a Recording for a stored audio object. Callers set `call_id`/`bytes`/
    /// `duration_ms` on the returned value.
    pub fn new(tenant: Uuid, object_id: Uuid) -> Self {
        Recording {
            base: EntityBase::new(tenant),
            call_id: None,
            object_id,
            bytes: None,
            transcript_object_id: None,
            duration_ms: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialises_object_id_and_omits_empty_optionals() {
        let obj = Uuid::now_v7();
        let r = Recording::new(Uuid::now_v7(), obj);
        let j = serde_json::to_value(&r).unwrap();
        assert_eq!(j["object_id"], obj.to_string());
        assert!(j.get("call_id").is_none());
        assert!(j.get("bytes").is_none());
        assert!(j.get("transcript_object_id").is_none());
        assert!(j.get("duration_ms").is_none());
    }

    #[test]
    fn round_trips_with_call_and_duration() {
        let mut r = Recording::new(Uuid::now_v7(), Uuid::now_v7());
        r.call_id = Some(Uuid::now_v7());
        r.bytes = Some(64000);
        r.duration_ms = Some(8000);
        let back: Recording = serde_json::from_value(serde_json::to_value(&r).unwrap()).unwrap();
        assert_eq!(back.call_id, r.call_id);
        assert_eq!(back.bytes, Some(64000));
        assert_eq!(back.duration_ms, Some(8000));
    }
}
