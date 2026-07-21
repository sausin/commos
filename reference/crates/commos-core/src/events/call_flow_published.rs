//! `CallFlowPublished` event — Rust projection of
//! `contracts/json-schema/events/CallFlowPublished.schema.json`.
//!
//! Not a Call *lifecycle* event: this announces that a `CallFlow` entity has a new
//! published revision. It is included because the schema name is `Call*`.

use serde::{Deserialize, Serialize};

use crate::event::EventPayload;
use crate::common::Uuid;

/// Payload of the `CallFlowPublished` canonical event (Volume 5). Produced by Routing
/// when a CallFlow revision is published (`components.md`: Routing produces
/// `CallFlowPublished`). The subject is the CallFlow, not a Call.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallFlowPublished {
    pub call_flow_id: Uuid,
    /// The newly-published CallFlow revision number.
    pub published_version: u64,
}

impl EventPayload for CallFlowPublished {
    const TYPE: &'static str = "CallFlowPublished";
    // Routing is the emitting subsystem for CallFlowPublished (Volume 3 components.md).
    const SOURCE: &'static str = "/routing";

    fn subject(&self) -> String {
        // The event is about the CallFlow entity.
        self.call_flow_id.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Correlation, Envelope};

    #[test]
    fn envelope_subject_is_call_flow() {
        let call_flow_id = Uuid::now_v7();
        let ctx = Correlation::root(Uuid::now_v7());
        let env = Envelope::new(
            CallFlowPublished { call_flow_id, published_version: 3 },
            &ctx,
            "idem-1",
        );
        assert_eq!(env.event_type, "CallFlowPublished");
        assert_eq!(env.source, "/routing");
        assert_eq!(env.subject, call_flow_id.to_string());
        let json = env.to_json();
        assert_eq!(json["data"]["call_flow_id"], call_flow_id.to_string());
        assert_eq!(json["data"]["published_version"], 3);
    }
}
