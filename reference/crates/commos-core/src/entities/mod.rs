//! Domain entities — Rust projections of `contracts/json-schema/entities/*`.
//!
//! This module currently realises the `Call` entity end-to-end (the voice workload's
//! keystone). The remaining 35 frozen entities are added the same way: one file, one
//! faithful projection, its enums and invariants enforced at the type boundary.

pub mod call;
pub mod channel;
pub mod message;
pub mod participant;
pub mod presence_state;
pub mod thread;
pub mod video_room;
