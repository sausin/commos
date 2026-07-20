//! Non-normative introspection endpoints (outside `/v1`).
//!
//! These let an operator or a test watch the event flow during bring-up. They are **not**
//! part of the frozen contract and carry no compatibility guarantee.

use std::convert::Infallible;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::state::AppState;

/// `GET /_introspect/events` — the recent-event ring, newest last.
pub async fn recent_events(State(st): State<AppState>) -> Json<Vec<serde_json::Value>> {
    Json(st.recent.snapshot())
}

/// `GET /_introspect/events/stream` — live Server-Sent Events of every relayed event.
pub async fn stream_events(
    State(st): State<AppState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = st.bus.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|res| match res {
        Ok(ev) => Some(Ok(Event::default().data(ev.to_string()))),
        // A lagged subscriber simply skips missed events; the ring has the history.
        Err(_) => None,
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}
