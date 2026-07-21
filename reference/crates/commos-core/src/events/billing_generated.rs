//! `BillingGenerated` event — Rust projection of
//! `contracts/json-schema/events/BillingGenerated.schema.json`.

use serde::{Deserialize, Serialize};

use crate::common::{Money, Uuid};
use crate::event::EventPayload;

/// Payload of the `BillingGenerated` canonical event (Volume 5). Produced by Billing when
/// a CDR is rated (`components.md`: Billing consumes `CallEnded` and produces the CDR /
/// `BillingGenerated`). `cdr_id`, `call_id`, `billable_ms` and `cost` are required;
/// `user_id` and `device_id` are optional attribution carried through from the CDR.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BillingGenerated {
    pub cdr_id: Uuid,
    pub call_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<Uuid>,
    /// Billable portion in milliseconds (schema `minimum: 0`).
    pub billable_ms: u64,
    pub cost: Money,
}

impl EventPayload for BillingGenerated {
    const TYPE: &'static str = "BillingGenerated";
    // Billing is the emitting subsystem for rating/CDR events (Volume 3 components.md).
    const SOURCE: &'static str = "/billing";

    fn subject(&self) -> String {
        // The CDR is the entity this event is about.
        self.cdr_id.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::Currency;
    use crate::event::{Correlation, Envelope};

    #[test]
    fn envelope_serialises_cdr_and_cost() {
        let cdr_id = Uuid::now_v7();
        let call_id = Uuid::now_v7();
        let ctx = Correlation::root(Uuid::now_v7());
        let env = Envelope::new(
            BillingGenerated {
                cdr_id,
                call_id,
                user_id: None,
                device_id: None,
                billable_ms: 60_000,
                cost: Money { currency: Currency::parse("USD").unwrap(), minor_units: 2 },
            },
            &ctx,
            "idem-1",
        );
        assert_eq!(env.event_type, "BillingGenerated");
        assert_eq!(env.source, "/billing");
        assert_eq!(env.subject, cdr_id.to_string());
        let json = env.to_json();
        assert_eq!(json["data"]["cdr_id"], cdr_id.to_string());
        assert_eq!(json["data"]["call_id"], call_id.to_string());
        assert_eq!(json["data"]["billable_ms"], 60_000);
        assert_eq!(json["data"]["cost"]["currency"], "USD");
        assert_eq!(json["data"]["cost"]["minor_units"], 2);
        // Optional attribution absent when unset.
        assert!(json["data"].get("user_id").is_none());
    }
}
