//! `Route` entity — Rust projection of `contracts/json-schema/entities/Route.schema.json`.
//!
//! A Route resolves a match (dialled number, DID, time-of-day) to a `destination_ref` — the
//! place a call goes. This reference implementation uses a small scheme-prefixed
//! `destination_ref` convention (`sip:100@domain`, `queue:<uuid>`, `external:+1…`) that the
//! control plane interprets when it routes a Call (Volume 3 Routing).

use serde::{Deserialize, Serialize};

use crate::common::{EntityBase, Uuid};

/// A routing rule. `destination_ref` is required; `match` (an open object) and `priority`
/// order competing routes (Volume 2).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Route {
    #[serde(flatten)]
    pub base: EntityBase,
    /// Where the Call goes. Convention: `sip:<user>@<host>` (a registered endpoint),
    /// `queue:<uuid>` (an ACD queue), or `external:<e164>` (off-net).
    pub destination_ref: String,
    /// Optional match criteria (DID, prefix, hours) — an open object per the schema.
    #[serde(rename = "match", skip_serializing_if = "Option::is_none")]
    pub match_: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
}

impl Route {
    /// A simple route to a destination (no match/priority).
    pub fn new(tenant: Uuid, destination_ref: impl Into<String>) -> Self {
        Route {
            base: EntityBase::new(tenant),
            destination_ref: destination_ref.into(),
            match_: None,
            priority: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialises_destination_and_omits_match() {
        let r = Route::new(Uuid::now_v7(), "sip:100@commos.local");
        let j = serde_json::to_value(&r).unwrap();
        assert_eq!(j["destination_ref"], "sip:100@commos.local");
        assert!(j.get("match").is_none());
        assert!(j.get("priority").is_none());
    }
}
