//! `DeviceDetected` event — Rust projection of
//! `contracts/json-schema/events/DeviceDetected.schema.json`.

use serde::{Deserialize, Serialize};

use crate::common::Uuid;
use crate::entities::device::DeviceState;
use crate::event::EventPayload;

/// Payload of the `DeviceDetected` canonical event (Volume 5). Produced by the
/// Provisioning subsystem when a Device is first detected on the substrate.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceDetected {
    pub device_id: Uuid,
    pub vendor_key: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    pub state: DeviceState,
}

impl EventPayload for DeviceDetected {
    const TYPE: &'static str = "DeviceDetected";
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
            DeviceDetected {
                device_id,
                vendor_key: "polycom.vvx450".into(),
                model: "VVX 450".into(),
                mac: None,
                state: DeviceState::Detected,
            },
            &ctx,
            "idem-1",
        );
        assert_eq!(env.event_type, "DeviceDetected");
        assert_eq!(env.source, "/provisioning");
        assert_eq!(env.subject, device_id.to_string());
        let json = env.to_json();
        assert_eq!(json["data"]["device_id"], device_id.to_string());
    }
}
