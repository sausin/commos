//! `Object` entity — Rust projection of `contracts/json-schema/entities/Object.schema.json`.
//!
//! An Object is a stored binary blob's **metadata**: a recording, voicemail, fax, firmware
//! image, transcript, export, or diagnostic bundle (CMOS-02-DOM-013). The bytes live behind
//! the Object Storage abstraction (Volume 3 §Object Storage, ADR-0008) — local filesystem, or
//! an S3-compatible backend — addressed by `uri`; this entity is the durable, queryable
//! record of *what* was stored, its size, and its content hash.

use serde::{Deserialize, Serialize};

use crate::common::{EntityBase, Timestamp, Uuid};

/// What kind of artefact an Object holds (`Object.schema.json` `kind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ObjectKind {
    Recording,
    Voicemail,
    Fax,
    Firmware,
    Transcript,
    Export,
    Diagnostic,
    Wallpaper,
    Other,
}

/// Optional retention policy for an Object (`Object.schema.json` `retention`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Retention {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
}

/// The Object entity. `EntityBase` is flattened so the wire shape is
/// `allOf: [EntityBase] + Object properties`. `kind`, `uri`, `bytes`, `sha256` are required.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Object {
    #[serde(flatten)]
    pub base: EntityBase,
    pub kind: ObjectKind,
    /// Where the bytes live, e.g. `local://<tenant>/<id>` or `s3://bucket/key`.
    pub uri: String,
    /// Size in bytes.
    pub bytes: u64,
    /// Lowercase hex SHA-256 of the content (integrity / dedupe).
    pub sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retention: Option<Retention>,
}

impl Object {
    /// Create Object metadata for a freshly stored blob.
    pub fn new(
        tenant: Uuid,
        kind: ObjectKind,
        uri: impl Into<String>,
        bytes: u64,
        sha256: impl Into<String>,
    ) -> Self {
        Object {
            base: EntityBase::new(tenant),
            kind,
            uri: uri.into(),
            bytes,
            sha256: sha256.into(),
            content_type: None,
            retention: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialises_required_fields_and_kind_casing() {
        let mut o = Object::new(Uuid::now_v7(), ObjectKind::Recording, "local://t/1", 1234, "abcd");
        o.content_type = Some("audio/wav".into());
        let j = serde_json::to_value(&o).unwrap();
        assert_eq!(j["kind"], "RECORDING");
        assert_eq!(j["uri"], "local://t/1");
        assert_eq!(j["bytes"], 1234);
        assert_eq!(j["sha256"], "abcd");
        assert_eq!(j["content_type"], "audio/wav");
        assert!(j.get("retention").is_none());
        // Round-trips.
        let back: Object = serde_json::from_value(j).unwrap();
        assert_eq!(back.kind, ObjectKind::Recording);
        assert_eq!(back.bytes, 1234);
    }

    #[test]
    fn every_kind_renders_screaming_snake() {
        let render = |k| {
            serde_json::to_value(Object::new(Uuid::now_v7(), k, "u", 0, "h")).unwrap()["kind"].clone()
        };
        assert_eq!(render(ObjectKind::Voicemail), "VOICEMAIL");
        assert_eq!(render(ObjectKind::Firmware), "FIRMWARE");
        assert_eq!(render(ObjectKind::Diagnostic), "DIAGNOSTIC");
        assert_eq!(render(ObjectKind::Other), "OTHER");
    }
}
