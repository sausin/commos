//! Control plane — decides *what* happens (Volume 3 §1). Stateless services over the
//! [`crate::store::Store`]; they issue typed commands to the media plane and emit events
//! through the outbox.

pub mod agents;
pub mod billing;
pub mod dialplan;
pub mod messaging;
pub mod onboarding;
pub mod queue;
pub mod rating;
pub mod realtime;
pub mod registrations;
pub mod routing;
