//! `DeviceReplacementStarted` event — Rust projection of
//! `contracts/json-schema/events/DeviceReplacementStarted.schema.json`.

use serde::{Deserialize, Serialize};

use crate::common::Uuid;
use crate::event::EventPayload;

/// Payload of the `DeviceReplacementStarted` canonical event (Volume 5). Produced by the
/// Provisioning subsystem when a Device begins replacement by another Device.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceReplacementStarted {
    pub device_id: Uuid,
    pub replacement_device_id: Uuid,
}

impl EventPayload for DeviceReplacementStarted {
    const TYPE: &'static str = "DeviceReplacementStarted";
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
            DeviceReplacementStarted {
                device_id,
                replacement_device_id: Uuid::now_v7(),
            },
            &ctx,
            "idem-1",
        );
        assert_eq!(env.event_type, "DeviceReplacementStarted");
        assert_eq!(env.source, "/provisioning");
        assert_eq!(env.subject, device_id.to_string());
        let json = env.to_json();
        assert_eq!(json["data"]["device_id"], device_id.to_string());
    }
}
