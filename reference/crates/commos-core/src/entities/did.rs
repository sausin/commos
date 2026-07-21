//! `DID` entity — Rust projection of `contracts/json-schema/entities/DID.schema.json`.
//!
//! A DID (Direct Inward Dialing number) maps a real external `e164` telephone number, provided
//! by a [`Carrier`](super::carrier::Carrier), onto an internal `destination_ref` (an extension,
//! IVR, queue, voicemail, …) (Volume 2). It is the inbound counterpart of a [`Trunk`]: an INVITE
//! arriving from the carrier for this number is routed to `destination_ref`. Configuration, no
//! lifecycle.

use serde::{Deserialize, Serialize};

use crate::common::{EntityBase, Uuid};

/// The DID entity. `e164`, `carrier_id`, and `destination_ref` are all required.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Did {
    #[serde(flatten)]
    pub base: EntityBase,
    /// The external number in canonical E.164 form (`+<digits>`).
    pub e164: String,
    pub carrier_id: Uuid,
    /// Where an inbound call to this number goes (e.g. `sip:200@host`, `ivr:<id>`, `voicemail`).
    pub destination_ref: String,
}

impl Did {
    /// Create a DID routing `e164` (from `carrier_id`) to `destination_ref`.
    pub fn new(
        tenant: Uuid,
        e164: impl Into<String>,
        carrier_id: Uuid,
        destination_ref: impl Into<String>,
    ) -> Self {
        Did {
            base: EntityBase::new(tenant),
            e164: e164.into(),
            carrier_id,
            destination_ref: destination_ref.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialises_required_fields() {
        let carrier = Uuid::now_v7();
        let d = Did::new(Uuid::now_v7(), "+14155550100", carrier, "ivr:abc");
        let j = serde_json::to_value(&d).unwrap();
        assert_eq!(j["e164"], "+14155550100");
        assert_eq!(j["carrier_id"], carrier.to_string());
        assert_eq!(j["destination_ref"], "ivr:abc");
        let back: Did = serde_json::from_value(j).unwrap();
        assert_eq!(back.e164, "+14155550100");
    }
}
