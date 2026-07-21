//! `Carrier` entity — Rust projection of
//! `contracts/json-schema/entities/Carrier.schema.json`.
//!
//! A Carrier is a logical telephony provider account — a PSTN/mobile/SIP-trunk operator (or the
//! `INTERNAL` pseudo-carrier) that [`Gateway`](super::gateway::Gateway)s, [`Trunk`](super::trunk::Trunk)s,
//! and [`DID`](super::did::Did)s belong to (Volume 2). It is *configuration*: no lifecycle state
//! machine (peer of Queue/Extension, CMOS-02-DOM-100).

use serde::{Deserialize, Serialize};

use crate::common::{EntityBase, Uuid};

/// What kind of provider a Carrier is (`Carrier.schema.json` `kind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CarrierKind {
    Pstn,
    Mobile,
    SipTrunk,
    Internal,
}

/// The Carrier entity. `name` and `kind` are required; `rating_profile_id` is optional.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Carrier {
    #[serde(flatten)]
    pub base: EntityBase,
    pub name: String,
    pub kind: CarrierKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating_profile_id: Option<Uuid>,
}

impl Carrier {
    /// Create a new Carrier of `kind` named `name`, with no rating profile.
    pub fn new(tenant: Uuid, name: impl Into<String>, kind: CarrierKind) -> Self {
        Carrier { base: EntityBase::new(tenant), name: name.into(), kind, rating_profile_id: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialises_kind_screaming_snake() {
        let c = Carrier::new(Uuid::now_v7(), "Acme Telecom", CarrierKind::SipTrunk);
        let j = serde_json::to_value(&c).unwrap();
        assert_eq!(j["name"], "Acme Telecom");
        assert_eq!(j["kind"], "SIP_TRUNK");
        assert!(j.get("rating_profile_id").is_none());
        // Round-trips.
        let back: Carrier = serde_json::from_value(j).unwrap();
        assert_eq!(back.kind, CarrierKind::SipTrunk);
    }
}
