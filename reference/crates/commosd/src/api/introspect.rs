//! Non-normative introspection endpoints (outside `/v1`).
//!
//! These let an operator or a test watch the event flow during bring-up. They are **not**
//! part of the frozen contract and carry no compatibility guarantee.
//!
//! ## Access
//! The event ring and bus are **global** (not tenant-partitioned) and carry every tenant's
//! call/message/user activity, so exposing them unauthenticated would be a cross-tenant leak.
//! They are therefore restricted to **trusted peers** (loopback / private LAN — see
//! [`super::peer`]): local bring-up and the dashboard keep working with zero config, while a
//! request from a public network is refused. A request with an unknown peer fails closed.

use std::convert::Infallible;
use std::net::SocketAddr;

use axum::extract::{ConnectInfo, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use super::problem::Problem;
use crate::state::AppState;

/// Refuse the request unless it came from a trusted (loopback/private-LAN) peer.
fn require_trusted(peer: SocketAddr) -> Result<(), Problem> {
    if super::peer::is_trusted_ip(&peer.ip()) {
        Ok(())
    } else {
        Err(Problem::new(
            axum::http::StatusCode::FORBIDDEN,
            "introspection_local_only",
            "the introspection feed is restricted to the local/private network",
        ))
    }
}

/// `GET /_introspect/events` — the recent-event ring, newest last. Trusted peers only.
pub async fn recent_events(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Result<Json<Vec<serde_json::Value>>, Problem> {
    require_trusted(peer)?;
    Ok(Json(st.recent.snapshot()))
}

/// `GET /_introspect/events/stream` — live Server-Sent Events of every relayed event.
/// Trusted peers only.
pub async fn stream_events(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, Problem> {
    require_trusted(peer)?;
    let rx = st.bus.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|res| match res {
        Ok(ev) => Some(Ok(Event::default().data(ev.to_string()))),
        // A lagged subscriber simply skips missed events; the ring has the history.
        Err(_) => None,
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
