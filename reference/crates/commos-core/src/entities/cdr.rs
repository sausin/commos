//! `CDR` entity — Rust projection of
//! `contracts/json-schema/entities/CDR.schema.json`.
//!
//! A Call Detail Record is the billable, attributable projection of a `Call`
//! (Volumes 2 & 10). Billing derives it from a completed Call: `call_id`,
//! `organisation_id`, the measured `duration_ms`/`billable_ms` and the rated `cost`
//! are required; the attribution and enrichment fields (cost centre, department,
//! user/identity/device, DID/carrier/codec, recording/transcript objects, tags) are
//! optional and only present when known. `EntityBase` is flattened so the wire shape is
//! `allOf: [EntityBase] + CDR properties`, matching the schema.

use serde::{Deserialize, Serialize};

use crate::common::{EntityBase, Money, Uuid};

/// The CDR entity. `call_id`, `organisation_id`, `duration_ms`, `billable_ms` and `cost`
/// are required; every attribution/enrichment field is optional and skipped when absent
/// so the serialised form validates against the schema.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Cdr {
    #[serde(flatten)]
    pub base: EntityBase,
    /// The Call this record bills (schema `call_id`).
    pub call_id: Uuid,
    /// Owning organisation for billing roll-up (schema `organisation_id`).
    pub organisation_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_centre_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub department_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extension: Option<String>,
    /// Dialled/associated number in E.164 form (schema `did`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub did: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub carrier_id: Option<Uuid>,
    /// Total Call duration in milliseconds (schema `minimum: 0`).
    pub duration_ms: u64,
    /// Billable portion in milliseconds (schema `minimum: 0`).
    pub billable_ms: u64,
    /// Rated cost as integer minor units (schema `cost`, `common#/$defs/Money`).
    pub cost: Money,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording_object_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_object_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::Currency;

    #[test]
    fn serialised_field_names_match_schema() {
        let call_id = Uuid::now_v7();
        let organisation_id = Uuid::now_v7();
        let device_id = Uuid::now_v7();
        let cdr = Cdr {
            base: EntityBase::new(Uuid::now_v7()),
            call_id,
            organisation_id,
            cost_centre_id: None,
            department_id: None,
            user_id: None,
            identity_id: None,
            device_id: Some(device_id),
            extension: Some("100".into()),
            did: None,
            carrier_id: None,
            duration_ms: 65_000,
            billable_ms: 60_000,
            cost: Money { currency: Currency::parse("USD").unwrap(), minor_units: 2 },
            codec: Some("opus".into()),
            recording_object_id: None,
            transcript_object_id: None,
            tags: vec!["sales".into()],
        };
        let json = serde_json::to_value(&cdr).unwrap();
        // Required wire fields.
        assert_eq!(json["call_id"], call_id.to_string());
        assert_eq!(json["organisation_id"], organisation_id.to_string());
        assert_eq!(json["duration_ms"], 65_000);
        assert_eq!(json["billable_ms"], 60_000);
        assert_eq!(json["cost"]["currency"], "USD");
        assert_eq!(json["cost"]["minor_units"], 2);
        // Optional fields present when set, absent otherwise.
        assert_eq!(json["device_id"], device_id.to_string());
        assert_eq!(json["extension"], "100");
        assert_eq!(json["codec"], "opus");
        assert_eq!(json["tags"][0], "sales");
        assert!(json.get("user_id").is_none());
        assert!(json.get("did").is_none());
        // Round-trips.
        let back: Cdr = serde_json::from_value(json).unwrap();
        assert_eq!(back.call_id, call_id);
        assert_eq!(back.organisation_id, organisation_id);
        assert_eq!(back.billable_ms, 60_000);
        assert_eq!(back.device_id, Some(device_id));
        assert_eq!(back.cost.minor_units, 2);
    }
}
