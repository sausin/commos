//! Domain entities — Rust projections of `contracts/json-schema/entities/*`.
//!
//! This module currently realises the `Call` entity end-to-end (the voice workload's
//! keystone). The remaining 35 frozen entities are added the same way: one file, one
//! faithful projection, its enums and invariants enforced at the type boundary.

pub mod call;
pub mod cdr;
pub mod channel;
pub mod device;
pub mod extension;
pub mod message;
pub mod object;
pub mod participant;
pub mod presence_state;
pub mod queue;
pub mod recording;
pub mod route;
pub mod thread;
pub mod user;
pub mod video_room;
pub mod webhook;
