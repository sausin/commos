//! `Extension` entity — Rust projection of
//! `contracts/json-schema/entities/Extension.schema.json`.
//!
//! An Extension is a dialable number bound to a routing target: it maps a short `number`
//! onto a `route_id` so inbound work reaches the right destination. It is *configuration*,
//! not an occurrence: it carries no lifecycle state machine (peer of Queue in this respect,
//! CMOS-02-DOM-100).

use serde::{Deserialize, Serialize};

use crate::common::{EntityBase, Uuid};

/// The Extension entity. `EntityBase` is flattened so the wire shape is
/// `allOf: [EntityBase] + Extension properties`, matching the schema. `number` and
/// `route_id` are required; `label` is optional.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Extension {
    #[serde(flatten)]
    pub base: EntityBase,
    pub number: String,
    pub route_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl Extension {
    /// Create a new Extension mapping `number` onto `route_id`, with no label. Callers set
    /// `label` directly on the returned value.
    pub fn new(tenant: Uuid, number: impl Into<String>, route_id: Uuid) -> Self {
        Extension {
            base: EntityBase::new(tenant),
            number: number.into(),
            route_id,
            label: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_starts_v0() {
        let t = Uuid::now_v7();
        let route = Uuid::now_v7();
        let ext = Extension::new(t, "1001", route);
        assert_eq!(ext.number, "1001");
        assert_eq!(ext.route_id, route);
        assert_eq!(ext.base.version, 0);
        assert_eq!(ext.base.tenant_id, t);
    }

    #[test]
    fn serialised_field_names_match_schema() {
        let route = Uuid::now_v7();
        let mut ext = Extension::new(Uuid::now_v7(), "1001", route);
        ext.label = Some("Front desk".into());
        let json = serde_json::to_value(&ext).unwrap();
        // Flattened EntityBase + Extension properties.
        assert_eq!(json["number"], "1001");
        assert_eq!(json["route_id"], route.to_string());
        assert_eq!(json["label"], "Front desk");
        assert!(json.get("id").is_some());
        assert!(json.get("tenant_id").is_some());
        // Round-trips.
        let back: Extension = serde_json::from_value(json).unwrap();
        assert_eq!(back.number, "1001");
        assert_eq!(back.route_id, route);
    }

    #[test]
    fn none_label_is_omitted() {
        let ext = Extension::new(Uuid::now_v7(), "1001", Uuid::now_v7());
        let json = serde_json::to_value(&ext).unwrap();
        assert!(json.get("label").is_none());
    }
}
