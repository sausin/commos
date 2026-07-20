//! API Gateway (Volume 3: AuthN/Z, request→command, the OpenAPI surface).
//!
//! Routes are mounted under `/v1` to match the frozen OpenAPI `servers` base
//! (`https://{host}/v1`). Operational signals and non-normative introspection live
//! outside `/v1` so they can never be confused for the versioned contract.

pub mod auth;
pub mod calls;
pub mod health;
pub mod introspect;
pub mod problem;

use axum::routing::get;
use axum::Router;
use tower_http::trace::TraceLayer;

use crate::state::AppState;

/// Build the application router.
pub fn router(state: AppState) -> Router {
    // The versioned contract surface (Volume 4). Every route here is bearer-authenticated
    // and tenant-scoped via the `TenantContext` extractor.
    let v1 = Router::new()
        .route("/calls", get(calls::list_calls).post(calls::create_calls))
        .route("/calls/:id", get(calls::get_call));

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
