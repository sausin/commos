//! `Gateway` entity — Rust projection of
//! `contracts/json-schema/entities/Gateway.schema.json`.
//!
//! A Gateway is a concrete media/signalling endpoint of a [`Carrier`](super::carrier::Carrier):
//! the `address` (host[:port]) CommOS sends outbound calls to and recognises inbound calls from,
//! plus an observed `health` (Volume 2). `health` is a *fact the platform observes* (via probing
//! / call outcomes), not a commanded value — `GatewayOffline` / `GatewayRecovered` announce its
//! transitions (Volume 5).

use serde::{Deserialize, Serialize};

use crate::common::{EntityBase, Uuid};

/// The transport kind of a Gateway (`Gateway.schema.json` `kind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GatewayKind {
    #[serde(rename = "SIP")]
    Sip,
    #[serde(rename = "4G")]
    FourG,
    #[serde(rename = "SIM_BANK")]
    SimBank,
}

/// Observed reachability of a Gateway (`Gateway.schema.json` `health`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GatewayHealth {
    Online,
    Offline,
}

/// The Gateway entity. `carrier_id`, `kind`, and `health` are required; `address` is optional
/// (a `4G`/`SIM_BANK` gateway may not have a SIP address).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Gateway {
    #[serde(flatten)]
    pub base: EntityBase,
    pub carrier_id: Uuid,
    pub kind: GatewayKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    pub health: GatewayHealth,
}

impl Gateway {
    /// Create a new `SIP` Gateway for `carrier_id` reachable at `address`, starting `ONLINE`.
    pub fn new_sip(tenant: Uuid, carrier_id: Uuid, address: impl Into<String>) -> Self {
        Gateway {
            base: EntityBase::new(tenant),
            carrier_id,
            kind: GatewayKind::Sip,
            address: Some(address.into()),
            health: GatewayHealth::Online,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialises_kind_and_health() {
        let g = Gateway::new_sip(Uuid::now_v7(), Uuid::now_v7(), "sip.carrier.net:5060");
        let j = serde_json::to_value(&g).unwrap();
        assert_eq!(j["kind"], "SIP");
        assert_eq!(j["health"], "ONLINE");
        assert_eq!(j["address"], "sip.carrier.net:5060");
        let back: Gateway = serde_json::from_value(j).unwrap();
        assert_eq!(back.kind, GatewayKind::Sip);
        assert_eq!(back.health, GatewayHealth::Online);
    }

    #[test]
    fn non_sip_kinds_use_contract_casing() {
        let render = |k| {
            let mut g = Gateway::new_sip(Uuid::now_v7(), Uuid::now_v7(), "x");
            g.kind = k;
            serde_json::to_value(&g).unwrap()["kind"].clone()
        };
        assert_eq!(render(GatewayKind::FourG), "4G");
        assert_eq!(render(GatewayKind::SimBank), "SIM_BANK");
    }
}
