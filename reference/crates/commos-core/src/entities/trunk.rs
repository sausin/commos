//! `Trunk` entity — Rust projection of
//! `contracts/json-schema/entities/Trunk.schema.json`.
//!
//! A Trunk is a [`Carrier`](super::carrier::Carrier)'s capacity + credentials envelope: the
//! max concurrent `channels`, the offered `codecs`, and the `auth` used when the platform
//! registers/authenticates to the carrier as a UAC (Volume 2). Configuration, no lifecycle.

use serde::{Deserialize, Serialize};

use crate::common::{EntityBase, Uuid};

/// The Trunk entity. Only `carrier_id` is required.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Trunk {
    #[serde(flatten)]
    pub base: EntityBase,
    pub carrier_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels_max: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub codecs: Vec<String>,
    /// Free-form auth object (e.g. `{ "username": "...", "password": "..." }`) used for outbound
    /// digest authentication to the carrier. Held as a generic object to match the schema.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<serde_json::Value>,
}

impl Trunk {
    /// Create a new Trunk for `carrier_id` with no channel cap, codecs, or auth.
    pub fn new(tenant: Uuid, carrier_id: Uuid) -> Self {
        Trunk {
            base: EntityBase::new(tenant),
            carrier_id,
            channels_max: None,
            codecs: Vec::new(),
            auth: None,
        }
    }

    /// The `(username, password)` from `auth`, if both are present — the credentials for outbound
    /// digest authentication to the carrier.
    pub fn credentials(&self) -> Option<(String, String)> {
        let auth = self.auth.as_ref()?.as_object()?;
        let user = auth.get("username")?.as_str()?.to_string();
        let pass = auth.get("password")?.as_str()?.to_string();
        Some((user, pass))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialises_required_carrier_and_omits_empty() {
        let carrier = Uuid::now_v7();
        let t = Trunk::new(Uuid::now_v7(), carrier);
        let j = serde_json::to_value(&t).unwrap();
        assert_eq!(j["carrier_id"], carrier.to_string());
        assert!(j.get("channels_max").is_none());
        assert!(j.get("codecs").is_none());
        assert!(j.get("auth").is_none());
    }

    #[test]
    fn extracts_credentials_from_auth_object() {
        let mut t = Trunk::new(Uuid::now_v7(), Uuid::now_v7());
        assert_eq!(t.credentials(), None);
        t.auth = Some(serde_json::json!({"username": "acme", "password": "s3cret"}));
        assert_eq!(t.credentials(), Some(("acme".into(), "s3cret".into())));
        // Missing password → no usable credentials.
        t.auth = Some(serde_json::json!({"username": "acme"}));
        assert_eq!(t.credentials(), None);
    }
}
