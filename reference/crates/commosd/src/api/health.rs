//! Liveness / readiness / info.
//!
//! Health and readiness gate load-balancer membership (CMOS-14-DEP-033): a node reports
//! **not ready** before it can serve and while draining, but stays **live** so it is not
//! killed mid-drain. These endpoints are unauthenticated operational signals (Volume 15).

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;

use crate::state::AppState;

#[derive(Serialize)]
pub struct Health {
    pub status: &'static str,
}

/// Liveness: the process is up. Always 200 while the event loop runs.
pub async fn livez() -> Json<Health> {
    Json(Health { status: "live" })
}

/// Readiness: the node is ready to serve traffic. 503 until warmed and again while draining.
pub async fn readyz(State(st): State<AppState>) -> (StatusCode, Json<Health>) {
    if st.is_ready() {
        (StatusCode::OK, Json(Health { status: "ready" }))
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, Json(Health { status: "not_ready" }))
    }
}

#[derive(Serialize)]
pub struct Info {
    pub product: &'static str,
    pub version: &'static str,
    /// CommOS event/API spec version this binary implements.
    pub spec_version: &'static str,
    pub topology: &'static str,
    pub started_at: String,
    pub arch: &'static str,
    pub os: &'static str,
}

/// `GET /metrics` — Prometheus / OpenMetrics exposition (Volume 15 §OBS-010). Unauthenticated
/// operational signal, served as `text/plain`.
pub async fn metrics(State(st): State<AppState>) -> impl axum::response::IntoResponse {
    let uptime = (time::OffsetDateTime::now_utc() - st.started_at.into_offset())
        .whole_seconds()
        .max(0) as u64;
    let body = st.metrics.render(
        uptime,
        env!("COMMOS_VERSION"),
        std::env::consts::ARCH,
        st.registrations.total(),
    );
    (
        [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        body,
    )
}

/// Build/runtime info — handy for verifying which artifact (and which architecture) is running.
pub async fn info(State(st): State<AppState>) -> Json<Info> {
    Json(Info {
        product: "commosd",
        version: env!("COMMOS_VERSION"),
        spec_version: commos_core::event::SPEC_VERSION,
        topology: "single-binary",
        started_at: st.started_at.to_string(),
        arch: std::env::consts::ARCH,
        os: std::env::consts::OS,
    })
}
