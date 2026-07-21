//! # commos-core
//!
//! The Rust projection of the **frozen** CommOS contract spine (Volumes 0/2/3/4/5 and
//! `contracts/`). These types are the shared vocabulary every subsystem speaks; they
//! encode their JSON-Schema constraints in code so the rest of the implementation
//! cannot construct an out-of-contract value.
//!
//! Layout mirrors `contracts/json-schema/`:
//! - [`common`] — shared primitives (`common.schema.json`)
//! - [`event`] — the CloudEvents envelope (`envelope.schema.json`)
//! - [`entities`] — domain entities (`entities/*`)
//! - [`events`] — canonical event payloads (`events/*`)
//!
//! Fidelity to the contract is the point (see the workspace README): conformance is
//! defined against contracts, not code, so these types are validated against the frozen
//! schemas and examples in the conformance harness.

pub mod common;
pub mod entities;
pub mod error;
pub mod event;
pub mod events;

pub use error::CoreError;
