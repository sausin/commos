//! `Device` entity — Rust projection of
//! `contracts/json-schema/entities/Device.schema.json`.
//!
//! A Device is a physical or virtual endpoint identified by a vendor key and driven
//! through a provisioning lifecycle (`DETECTED → PENDING → APPROVED → PROVISIONED →
//! OPERATIONAL → REPLACING → RETIRED`, with `REJECTED` as a rejection sink). It is the
//! attribution anchor at the edge of the substrate (CMOS-02-DOM-113): workload artefacts
//! resolve members back to Devices/Identities. It is a *peer* domain entity on the same
//! substrate (CMOS-02-DOM-100).

use serde::{Deserialize, Serialize};

use crate::common::{EntityBase, Timestamp, Uuid};

/// Device provisioning lifecycle state (`Device.schema.json` `state`; Volume 2
/// §Provisioning lifecycle).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DeviceState {
    Detected,
    Pending,
    Approved,
    Provisioned,
    Operational,
    Replacing,
    Retired,
    Rejected,
}

/// SIP/registration status of a Device (`Device.schema.json` `registration.status`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DeviceRegStatus {
    Registered,
    Unregistered,
}

/// Network attachment of a Device (`Device.schema.json` `network`). All fields optional;
/// skipped when absent so the serialised form validates against the schema.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DeviceNetwork {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vlan: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub switch_port: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

/// Registration snapshot of a Device (`Device.schema.json` `registration`). All fields
/// optional; skipped when absent.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DeviceRegistration {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<DeviceRegStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<Timestamp>,
}

/// The Device entity. `EntityBase` is flattened so the wire shape is
/// `allOf: [EntityBase] + Device properties`, matching the schema. `vendor_key`, `model`
/// and `state` are required; the rest are optional.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Device {
    #[serde(flatten)]
    pub base: EntityBase,
    /// Vendor-scoped key (OpenKey pattern `^[a-z0-9_.-]+$` in the schema).
    pub vendor_key: String,
    pub model: String,
    pub state: DeviceState,
    /// 12 lowercase hex digits (schema constraint).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assigned_user_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<DeviceNetwork>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registration: Option<DeviceRegistration>,
}

impl Device {
    /// Create a new Device in the `APPROVED` state with no optional fields set. Callers set
    /// `mac` / `assigned_user_id` / `firmware` / `network` / `registration` on the returned
    /// value.
    pub fn new(tenant: Uuid, vendor_key: impl Into<String>, model: impl Into<String>) -> Self {
        Device {
            base: EntityBase::new(tenant),
            vendor_key: vendor_key.into(),
            model: model.into(),
            state: DeviceState::Approved,
            mac: None,
            assigned_user_id: None,
            firmware: None,
            network: None,
            registration: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_starts_approved_v0() {
        let t = Uuid::now_v7();
        let d = Device::new(t, "polycom.vvx450", "VVX 450");
        assert_eq!(d.state, DeviceState::Approved);
        assert_eq!(d.base.version, 0);
        assert_eq!(d.base.tenant_id, t);
        assert_eq!(d.vendor_key, "polycom.vvx450");
        assert_eq!(d.model, "VVX 450");
    }

    #[test]
    fn serialised_field_names_match_schema() {
        let mut d = Device::new(Uuid::now_v7(), "polycom.vvx450", "VVX 450");
        d.mac = Some("001122aabbcc".into());
        d.assigned_user_id = Some(Uuid::now_v7());
        d.firmware = Some("6.4.1".into());
        d.network = Some(DeviceNetwork {
            ip: Some("10.0.0.5".into()),
            vlan: Some(30),
            switch_port: Some("Gi1/0/12".into()),
            location: Some("HQ-2F".into()),
        });
        d.registration = Some(DeviceRegistration {
            status: Some(DeviceRegStatus::Registered),
            last_seen_at: Some(Timestamp::now()),
        });
        let json = serde_json::to_value(&d).unwrap();
        // Flattened EntityBase + Device properties, faithful casing.
        assert_eq!(json["vendor_key"], "polycom.vvx450");
        assert_eq!(json["model"], "VVX 450");
        assert_eq!(json["state"], "APPROVED");
        assert_eq!(json["mac"], "001122aabbcc");
        assert_eq!(json["firmware"], "6.4.1");
        assert_eq!(json["network"]["ip"], "10.0.0.5");
        assert_eq!(json["network"]["vlan"], 30);
        assert_eq!(json["network"]["switch_port"], "Gi1/0/12");
        assert_eq!(json["network"]["location"], "HQ-2F");
        assert_eq!(json["registration"]["status"], "REGISTERED");
        assert!(json["registration"].get("last_seen_at").is_some());
        assert!(json.get("id").is_some());
        assert!(json.get("tenant_id").is_some());
        assert!(json.get("assigned_user_id").is_some());
        // Round-trips.
        let back: Device = serde_json::from_value(json).unwrap();
        assert_eq!(back.state, DeviceState::Approved);
        assert_eq!(back.registration.unwrap().status, Some(DeviceRegStatus::Registered));

        // Every state variant renders SCREAMING_SNAKE.
        let render = |s| {
            let mut x = Device::new(Uuid::now_v7(), "k", "m");
            x.state = s;
            serde_json::to_value(&x).unwrap()["state"].clone()
        };
        assert_eq!(render(DeviceState::Detected), "DETECTED");
        assert_eq!(render(DeviceState::Pending), "PENDING");
        assert_eq!(render(DeviceState::Provisioned), "PROVISIONED");
        assert_eq!(render(DeviceState::Operational), "OPERATIONAL");
        assert_eq!(render(DeviceState::Replacing), "REPLACING");
        assert_eq!(render(DeviceState::Retired), "RETIRED");
        assert_eq!(render(DeviceState::Rejected), "REJECTED");
    }

    #[test]
    fn empty_optionals_are_omitted() {
        let d = Device::new(Uuid::now_v7(), "k", "m");
        let json = serde_json::to_value(&d).unwrap();
        assert!(json.get("mac").is_none());
        assert!(json.get("assigned_user_id").is_none());
        assert!(json.get("firmware").is_none());
        assert!(json.get("network").is_none());
        assert!(json.get("registration").is_none());
    }

    #[test]
    fn nested_none_fields_are_omitted() {
        let net = DeviceNetwork::default();
        let json = serde_json::to_value(&net).unwrap();
        assert!(json.get("ip").is_none());
        assert!(json.get("vlan").is_none());
        assert!(json.get("switch_port").is_none());
        assert!(json.get("location").is_none());
        let reg = DeviceRegistration::default();
        let json = serde_json::to_value(&reg).unwrap();
        assert!(json.get("status").is_none());
        assert!(json.get("last_seen_at").is_none());
    }
}
