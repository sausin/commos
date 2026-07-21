//! `/v1/objects` — the Object Storage surface (Volume 3 §Object Storage; ADR-0008).
//!
//! Upload raw bytes (the platform hashes + stores them and returns the [`Object`] metadata),
//! list/read metadata, download the bytes, or delete. Recordings, voicemail, exports, and
//! diagnostic bundles are all Objects; this generic surface is their common substrate.
//! Tenant-scoped throughout.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use commos_core::common::Uuid;
use commos_core::entities::object::{Object, ObjectKind};

use super::auth::TenantContext;
use super::calls::ListParams;
use super::problem::Problem;
use crate::control::objects::ObjectError;
use crate::state::AppState;

fn oid(s: &str) -> Result<Uuid, Problem> {
    Uuid::parse(s).map_err(|_| Problem::bad_request("id is not a valid UUIDv7"))
}

/// Map an object-service error onto the right Problem status.
fn oerr(e: ObjectError) -> Problem {
    match e {
        ObjectError::NotFound => Problem::not_found("no such object"),
        ObjectError::Blob(e) => Problem::internal(e.to_string()),
        ObjectError::Store(e) => Problem::internal(e.to_string()),
    }
}

#[derive(Serialize)]
pub struct ObjectPage {
    pub items: Vec<Object>,
    pub next_cursor: Option<String>,
}

/// `GET /v1/objects`
pub async fn list_objects(
    State(st): State<AppState>,
    t: TenantContext,
    Query(p): Query<ListParams>,
) -> Result<Json<ObjectPage>, Problem> {
    let limit = p.limit.unwrap_or(50).clamp(1, 200);
    let page = st.objects.list(t.tenant_id, limit, p.cursor).await.map_err(oerr)?;
    Ok(Json(ObjectPage { items: page.items, next_cursor: page.next_cursor }))
}

#[derive(Deserialize)]
pub struct UploadParams {
    /// One of the Object kinds (default `OTHER`).
    pub kind: Option<String>,
}

fn parse_kind(s: Option<&str>) -> Result<ObjectKind, Problem> {
    match s.map(|s| s.to_ascii_uppercase()).as_deref() {
        None | Some("OTHER") => Ok(ObjectKind::Other),
        Some("RECORDING") => Ok(ObjectKind::Recording),
        Some("VOICEMAIL") => Ok(ObjectKind::Voicemail),
        Some("FAX") => Ok(ObjectKind::Fax),
        Some("FIRMWARE") => Ok(ObjectKind::Firmware),
        Some("TRANSCRIPT") => Ok(ObjectKind::Transcript),
        Some("EXPORT") => Ok(ObjectKind::Export),
        Some("DIAGNOSTIC") => Ok(ObjectKind::Diagnostic),
        Some("WALLPAPER") => Ok(ObjectKind::Wallpaper),
        Some(other) => Err(Problem::bad_request(format!("unknown object kind '{other}'"))),
    }
}

/// `POST /v1/objects?kind=RECORDING` — upload raw bytes; the body is the content, the
/// `Content-Type` header its MIME type. Returns the stored Object metadata.
pub async fn upload_object(
    State(st): State<AppState>,
    t: TenantContext,
    Query(p): Query<UploadParams>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, Problem> {
    if body.is_empty() {
        return Err(Problem::bad_request("request body (object content) is empty"));
    }
    let kind = parse_kind(p.kind.as_deref())?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let obj = st.objects.put(t.tenant_id, kind, content_type, &body).await.map_err(oerr)?;
    Ok((StatusCode::CREATED, Json(obj)))
}

/// `GET /v1/objects/{id}` — the Object metadata.
pub async fn get_object(
    State(st): State<AppState>,
    t: TenantContext,
    Path(id): Path<String>,
) -> Result<Json<Object>, Problem> {
    let obj = st.objects.get(t.tenant_id, oid(&id)?).await.map_err(oerr)?;
    Ok(Json(obj))
}

/// `GET /v1/objects/{id}/content` — download the stored bytes with the recorded MIME type.
pub async fn get_object_content(
    State(st): State<AppState>,
    t: TenantContext,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, Problem> {
    let (obj, bytes) = st.objects.get_bytes(t.tenant_id, oid(&id)?).await.map_err(oerr)?;
    let ct = obj.content_type.unwrap_or_else(|| "application/octet-stream".to_string());
    Ok(([(header::CONTENT_TYPE, ct)], bytes))
}

/// `DELETE /v1/objects/{id}` — remove the blob and its metadata.
pub async fn delete_object(
    State(st): State<AppState>,
    t: TenantContext,
    Path(id): Path<String>,
) -> Result<StatusCode, Problem> {
    st.objects.delete(t.tenant_id, oid(&id)?).await.map_err(oerr)?;
    Ok(StatusCode::NO_CONTENT)
}
