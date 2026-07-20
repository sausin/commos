//! API Gateway (Volume 3: AuthN/Z, request→command, the OpenAPI surface).
//!
//! Routes are mounted under `/v1` to match the frozen OpenAPI `servers` base
//! (`https://{host}/v1`). Operational signals and non-normative introspection live
//! outside `/v1` so they can never be confused for the versioned contract.

pub mod auth;
pub mod calls;
pub mod channels;
pub mod health;
pub mod introspect;
pub mod messages;
pub mod problem;
pub mod threads;

use axum::routing::{get, post};
use axum::Router;
use tower_http::trace::TraceLayer;

use crate::state::AppState;

/// Build the application router.
pub fn router(state: AppState) -> Router {
    // The versioned contract surface (Volume 4). Every route here is bearer-authenticated
    // and tenant-scoped via the `TenantContext` extractor.
    // The frozen API expresses actions as `/calls/{id}:hold`. axum's matcher can't bind a
    // path param followed by a literal `:action` in one segment, so actions are mounted as
    // sub-paths (`/calls/:id/hold`) for now — same command, same event, same capability.
    let v1 = Router::new()
        .route("/calls", get(calls::list_calls).post(calls::create_calls))
        .route("/calls/:id", get(calls::get_call))
        .route("/calls/:id/hold", post(calls::hold_call))
        .route("/calls/:id/resume", post(calls::resume_call))
        .route("/calls/:id/hangup", post(calls::hangup_call))
        .route("/calls/:id/transfer", post(calls::transfer_call))
        // Messaging workload — peers of Call on the same substrate (voice is one workload).
        .route("/channels", get(channels::list_channels).post(channels::create_channel))
        .route("/channels/:id", get(channels::get_channel))
        .route("/threads", get(threads::list_threads).post(threads::create_thread))
        .route("/threads/:id", get(threads::get_thread))
        .route("/messages", get(messages::list_messages).post(messages::create_message))
        .route("/messages/:id", get(messages::get_message));

    Router::new()
        .nest("/v1", v1)
        // Operational signals (Volume 15) — unauthenticated, outside the contract surface.
        .route("/livez", get(health::livez))
        .route("/readyz", get(health::readyz))
        .route("/info", get(health::info))
        // Non-normative introspection for bring-up/testing.
        .route("/_introspect/events", get(introspect::recent_events))
        .route("/_introspect/events/stream", get(introspect::stream_events))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
