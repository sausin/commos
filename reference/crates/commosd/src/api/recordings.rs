//! `/v1/recordings` — the call-recording surface (Volume 7; CMOS-02-DOM-013).
//!
//! List recordings, read one's metadata, or download its audio. The audio is served **as
//! captured** — the negotiated payload (G.711 μ-law) written byte-for-byte with **no
//! transcoding** (the pure-Rust, no-codec-libs posture); the consumer decodes (e.g. a browser
//! playing back `audio/basic`). Recordings are produced by the SIP media plane on hangup when
//! `record_calls` is enabled; this surface is read-only. Tenant-scoped throughout.

use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::IntoResponse;
use axum::Json;
use serde::Serialize;

use commos_core::common::Uuid;
use commos_core::entities::recording::Recording;

use super::auth::TenantContext;
use super::calls::ListParams;
use super::problem::Problem;
use crate::control::recordings::RecordingError;
use crate::state::AppState;

fn rid(s: &str) -> Result<Uuid, Problem> {
    Uuid::parse(s).map_err(|_| Problem::bad_request("id is not a valid UUIDv7"))
}

/// Map a recording-service error onto the right Problem status.
fn rerr(e: RecordingError) -> Problem {
    match e {
        RecordingError::NotFound => Problem::not_found("no such recording"),
        RecordingError::Object(e) => Problem::internal(e.to_string()),
        RecordingError::Store(e) => Problem::internal(e.to_string()),
    }
}

#[derive(Serialize)]
pub struct RecordingPage {
    pub items: Vec<Recording>,
    pub next_cursor: Option<String>,
}

/// `GET /v1/recordings`
pub async fn list_recordings(
    State(st): State<AppState>,
    t: TenantContext,
    Query(p): Query<ListParams>,
) -> Result<Json<RecordingPage>, Problem> {
    let limit = p.limit.unwrap_or(50).clamp(1, 200);
    let page = st.recordings.list(t.tenant_id, limit, p.cursor).await.map_err(rerr)?;
    Ok(Json(RecordingPage { items: page.items, next_cursor: page.next_cursor }))
}

/// `GET /v1/recordings/{id}` — the Recording metadata (call, object, bytes, duration).
pub async fn get_recording(
    State(st): State<AppState>,
    t: TenantContext,
    Path(id): Path<String>,
) -> Result<Json<Recording>, Problem> {
    let rec = st.recordings.get(t.tenant_id, rid(&id)?).await.map_err(rerr)?;
    Ok(Json(rec))
}

/// `GET /v1/recordings/{id}/content` — download the captured audio (`audio/basic`, as-is).
pub async fn get_recording_content(
    State(st): State<AppState>,
    t: TenantContext,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, Problem> {
    let (_rec, bytes) = st.recordings.get_audio(t.tenant_id, rid(&id)?).await.map_err(rerr)?;
    Ok(([(header::CONTENT_TYPE, "audio/basic")], bytes))
}
