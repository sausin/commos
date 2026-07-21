//! Control plane — decides *what* happens (Volume 3 §1). Stateless services over the
//! [`crate::store::Store`]; they issue typed commands to the media plane and emit events
//! through the outbox.

pub mod agents;
pub mod billing;
pub mod callflow;
pub mod configexport;
pub mod dialplan;
pub mod ivr;
pub mod messaging;
pub mod objects;
pub mod onboarding;
pub mod policy;
pub mod provisioning;
pub mod queue;
pub mod rating;
pub mod recordings;
pub mod realtime;
pub mod registrations;
pub mod routing;
pub mod voicemail;
pub mod webhook_delivery;
pub mod webhooks;
