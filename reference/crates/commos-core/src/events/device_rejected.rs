//! `DeviceRejected` event — Rust projection of
//! `contracts/json-schema/events/DeviceRejected.schema.json`.

use serde::{Deserialize, Serialize};

use crate::common::Uuid;
use crate::event::EventPayload;

/// Payload of the `DeviceRejected` canonical event (Volume 5). Produced by the
/// Provisioning subsystem when a Device is rejected for provisioning.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceRejected {
    pub device_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl EventPayload for DeviceRejected {
    const TYPE: &'static str = "DeviceRejected";
    // Provisioning is the emitting subsystem for Device lifecycle events.
    const SOURCE: &'static str = "/provisioning";

    fn subject(&self) -> String {
        // The event is about the Device.
        self.device_id.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Correlation, Envelope};

    #[test]
    fn envelope_carries_type_source_subject() {
        let device_id = Uuid::now_v7();
        let ctx = Correlation::root(Uuid::now_v7());
        let env = Envelope::new(
            DeviceRejected {
                device_id,
                reason: None,
            },
            &ctx,
            "idem-1",
        );
        assert_eq!(env.event_type, "DeviceRejected");
        assert_eq!(env.source, "/provisioning");
        assert_eq!(env.subject, device_id.to_string());
        let json = env.to_json();
        assert_eq!(json["data"]["device_id"], device_id.to_string());
    }
}
