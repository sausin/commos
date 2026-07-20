//! Control plane — decides *what* happens (Volume 3 §1). Stateless services over the
//! [`crate::store::Store`]; they issue typed commands to the media plane and emit events
//! through the outbox.

pub mod routing;
