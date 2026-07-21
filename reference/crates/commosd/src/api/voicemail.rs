//! `/v1/voicemails` — the voicemail surface (Volume 4; capability `voicemail.view`).
//!
//! List voicemails, read one's metadata, download its audio, or mark it read. The audio is
//! served **as captured** — the negotiated payload (G.711 μ-law) written byte-for-byte with
//! **no transcoding** (the pure-Rust, no-codec-libs posture); the consumer decodes (e.g. a
//! browser playing `audio/basic`). Voicemails are produced by the SIP media plane when a
//! callee does not answer; this surface is read/mark-read only. Tenant-scoped throughout.

use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::IntoResponse;
use axum::Json;
use serde::Serialize;

use commos_core::common::Uuid;
use commos_core::entities::voicemail::Voicemail;

use super::auth::TenantContext;
use super::calls::ListParams;
use super::problem::Problem;
use crate::control::voicemail::VoicemailError;
use crate::state::AppState;

fn vid(s: &str) -> Result<Uuid, Problem> {
    Uuid::parse(s).map_err(|_| Problem::bad_request("id is not a valid UUIDv7"))
}

/// Map a voicemail-service error onto the right Problem status.
fn verr(e: VoicemailError) -> Problem {
    match e {
        VoicemailError::NotFound => Problem::not_found("no such voicemail"),
        VoicemailError::Object(e) => Problem::internal(e.to_string()),
        VoicemailError::Store(e) => Problem::internal(e.to_string()),
    }
}

#[derive(Serialize)]
pub struct VoicemailPage {
    pub items: Vec<Voicemail>,
    pub next_cursor: Option<String>,
}

/// `GET /v1/voicemails`
pub async fn list_voicemails(
    State(st): State<AppState>,
    t: TenantContext,
    Query(p): Query<ListParams>,
) -> Result<Json<VoicemailPage>, Problem> {
    let limit = p.limit.unwrap_or(50).clamp(1, 200);
    let page = st.voicemails.list(t.tenant_id, limit, p.cursor).await.map_err(verr)?;
    Ok(Json(VoicemailPage { items: page.items, next_cursor: page.next_cursor }))
}

/// `GET /v1/voicemails/{id}` — the Voicemail metadata (mailbox, call, object, duration, read).
pub async fn get_voicemail(
    State(st): State<AppState>,
    t: TenantContext,
    Path(id): Path<String>,
) -> Result<Json<Voicemail>, Problem> {
    let vm = st.voicemails.get(t.tenant_id, vid(&id)?).await.map_err(verr)?;
    Ok(Json(vm))
}

/// `GET /v1/voicemails/{id}/content` — download the captured audio (`audio/basic`, as-is).
pub async fn get_voicemail_content(
    State(st): State<AppState>,
    t: TenantContext,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, Problem> {
    let (_vm, bytes) = st.voicemails.get_audio(t.tenant_id, vid(&id)?).await.map_err(verr)?;
    Ok(([(header::CONTENT_TYPE, "audio/basic")], bytes))
}

/// `POST /v1/voicemails/{id}/read` — mark the voicemail read (idempotent). Clearing the last
/// unread message is what turns a phone's message-waiting lamp off on the next MWI push.
pub async fn mark_voicemail_read(
    State(st): State<AppState>,
    t: TenantContext,
    Path(id): Path<String>,
) -> Result<Json<Voicemail>, Problem> {
    let vm = st.voicemails.mark_read(t.tenant_id, vid(&id)?).await.map_err(verr)?;
    Ok(Json(vm))
}
