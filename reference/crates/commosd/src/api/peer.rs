//! Per-request peer-trust helpers for axum handlers.
//!
//! The IP classification itself lives in [`crate::net`] (shared with the SIP plane); this
//! module only bridges it to axum's request/`ConnectInfo` plumbing. See [`crate::net`] for the
//! trust model and the reverse-proxy caveat.

use std::net::SocketAddr;

use axum::extract::ConnectInfo;
use axum::http::request::Parts;

pub use crate::net::is_trusted_ip;

/// The immediate TCP peer for this request, if the server was started with connection info
/// (`into_make_service_with_connect_info`). `None` when unavailable — callers treat an unknown
/// peer as **untrusted** (fail closed).
pub fn peer_addr(parts: &Parts) -> Option<SocketAddr> {
    parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0)
}

/// Whether the request's peer is on a trusted network. An unknown peer fails closed
/// (untrusted), so a missing `ConnectInfo` never silently widens trust.
pub fn is_trusted_peer(parts: &Parts) -> bool {
    peer_addr(parts).is_some_and(|a| is_trusted_ip(&a.ip()))
}
