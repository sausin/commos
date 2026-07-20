//! `CallTransferred` event — Rust projection of
//! `contracts/json-schema/events/CallTransferred.schema.json`.

use serde::{Deserialize, Serialize};

use crate::event::EventPayload;
use crate::common::Uuid;

/// Transfer kind (`CallTransferred.schema.json` `transfer_type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum TransferType {
    Blind,
    Attended,
}

/// Payload of the `CallTransferred` canonical event (Volume 5). Produced by SIP when a
/// Call is transferred (`components.md`: SIP produces `CallTransferred`). `to_ref` (the
/// transfer target) is required; `from_ref` (the transferor) is optional.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallTransferred {
    pub call_id: Uuid,
    pub transfer_type: TransferType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_ref: Option<String>,
    pub to_ref: String,
}

impl EventPayload for CallTransferred {
    const TYPE: &'static str = "CallTransferred";
    // SIP is the emitting subsystem for Call signalling events (Volume 3 components.md).
    const SOURCE: &'static str = "/sip";

    fn subject(&self) -> String {
        self.call_id.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Correlation, Envelope};

    #[test]
    fn envelope_serialises_transfer_enum() {
        let call_id = Uuid::now_v7();
        let ctx = Correlation::root(Uuid::now_v7());
        let env = Envelope::new(
            CallTransferred {
                call_id,
                transfer_type: TransferType::Attended,
                from_ref: Some("sip:100".into()),
                to_ref: "sip:200".into(),
            },
            &ctx,
            "idem-1",
        );
        assert_eq!(env.event_type, "CallTransferred");
        assert_eq!(env.source, "/sip");
        let json = env.to_json();
        assert_eq!(json["data"]["transfer_type"], "ATTENDED");
        assert_eq!(json["data"]["to_ref"], "sip:200");
        assert_eq!(json["data"]["from_ref"], "sip:100");
    }
}
