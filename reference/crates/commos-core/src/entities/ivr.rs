//! `IVR` entity — Rust projection of `contracts/json-schema/entities/IVR.schema.json`.
//!
//! An IVR is an interactive-voice-response menu node (Volume 2): it plays a prompt
//! (`prompt_object_id`, an audio [`Object`](super::object::Object)) and maps collected DTMF
//! digits to destinations via `options` (`digit → destination_ref`), with a `timeout_ms`
//! collection window and an `invalid_action` for unmatched input. It is *configuration*, not
//! an occurrence — no lifecycle state machine and no creation event in the frozen catalogue
//! (peer of Queue/Extension, CMOS-02-DOM-100). The menu *runtime* (prompt playback, DTMF
//! collection) is media-plane work; this entity is the durable, queryable definition.

use serde::{Deserialize, Serialize};

use crate::common::{EntityBase, Uuid};

/// The IVR entity. `EntityBase` is flattened so the wire shape is
/// `allOf: [EntityBase] + IVR properties`. Only `options` (the digit map) is required.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ivr {
    #[serde(flatten)]
    pub base: EntityBase,
    /// The prompt audio played on entry (an Object of kind e.g. `RECORDING`/`OTHER`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_object_id: Option<Uuid>,
    /// `digit → destination_ref` map (e.g. `{"1": "queue:sales", "2": "voicemail"}`). Kept as
    /// a free-form object to match the schema; the runtime resolves each `destination_ref`.
    pub options: serde_json::Value,
    /// Digit-collection window in milliseconds before `invalid_action`/`timeout` applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<i64>,
    /// What to do on an unmatched digit (e.g. `repeat`, `hangup`, a `destination_ref`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalid_action: Option<String>,
}

impl Ivr {
    /// Create a new IVR with the given `options` map and all other fields unset. Callers set
    /// `prompt_object_id` / `timeout_ms` / `invalid_action` on the returned value.
    pub fn new(tenant: Uuid, options: serde_json::Value) -> Self {
        Ivr {
            base: EntityBase::new(tenant),
            prompt_object_id: None,
            options,
            timeout_ms: None,
            invalid_action: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialises_options_and_omits_empty_optionals() {
        let ivr = Ivr::new(Uuid::now_v7(), serde_json::json!({"1": "queue:sales"}));
        let j = serde_json::to_value(&ivr).unwrap();
        assert_eq!(j["options"]["1"], "queue:sales");
        assert!(j.get("prompt_object_id").is_none());
        assert!(j.get("timeout_ms").is_none());
        assert!(j.get("invalid_action").is_none());
    }

    #[test]
    fn round_trips_with_prompt_and_timeout() {
        let mut ivr = Ivr::new(Uuid::now_v7(), serde_json::json!({"1": "voicemail", "2": "queue:sales"}));
        ivr.prompt_object_id = Some(Uuid::now_v7());
        ivr.timeout_ms = Some(5000);
        ivr.invalid_action = Some("repeat".into());
        let back: Ivr = serde_json::from_value(serde_json::to_value(&ivr).unwrap()).unwrap();
        assert_eq!(back.prompt_object_id, ivr.prompt_object_id);
        assert_eq!(back.timeout_ms, Some(5000));
        assert_eq!(back.invalid_action.as_deref(), Some("repeat"));
        assert_eq!(back.options["2"], "queue:sales");
    }
}
