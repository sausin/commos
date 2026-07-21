//! `CallStarted` event — Rust projection of
//! `contracts/json-schema/events/CallStarted.schema.json`.

use serde::{Deserialize, Serialize};

use crate::entities::call::Direction;
use crate::event::EventPayload;
use crate::common::Uuid;

/// Payload of the `CallStarted` canonical event (Volume 5). Produced by Routing when a
/// Call is originated (`components.md`: Routing produces `CallStarted`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallStarted {
    pub call_id: Uuid,
    pub direction: Direction,
    pub from_ref: String,
    pub to_ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<Uuid>,
}

impl EventPayload for CallStarted {
    const TYPE: &'static str = "CallStarted";
    // Routing is the emitting subsystem for CallStarted (Volume 3 components.md).
    const SOURCE: &'static str = "/routing";

    fn subject(&self) -> String {
        // The event is about the Call.
        self.call_id.to_string()
    }
}
