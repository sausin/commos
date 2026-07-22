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
///
/// The stored `Content-Type` is attacker-controlled (set on upload), so serving it inline would
/// let an uploaded `text/html`/`image/svg+xml` blob execute as script in the daemon's own origin
/// (the same origin as `/dashboard`) — stored XSS. Defend on the way out: force a safe MIME for
/// active types, always send `X-Content-Type-Options: nosniff` so the browser cannot re-sniff a
/// benign type into an active one, and `Content-Disposition: attachment` so content is downloaded
/// rather than rendered.
pub async fn get_object_content(
    State(st): State<AppState>,
    t: TenantContext,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, Problem> {
    let (obj, bytes) = st.objects.get_bytes(t.tenant_id, oid(&id)?).await.map_err(oerr)?;
    let ct = sanitized_content_type(obj.content_type.as_deref());
    Ok((
        [
            (header::CONTENT_TYPE, ct),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (header::CONTENT_DISPOSITION, "attachment".to_string()),
        ],
        bytes,
    ))
}

/// Coerce a stored MIME type to something safe to serve. Anything that a browser could execute in
/// our origin (HTML, SVG, XML, or a script/`javascript:` type) is downgraded to
/// `application/octet-stream`; absent/other types pass through (and `nosniff` +
/// `attachment` on the response prevent inline rendering regardless).
fn sanitized_content_type(stored: Option<&str>) -> String {
    let raw = stored.unwrap_or("application/octet-stream").trim();
    let base = raw.split(';').next().unwrap_or(raw).trim().to_ascii_lowercase();
    let dangerous = matches!(
        base.as_str(),
        "text/html"
            | "application/xhtml+xml"
            | "image/svg+xml"
            | "text/xml"
            | "application/xml"
            | "application/javascript"
            | "text/javascript"
    );
    if base.is_empty() || dangerous {
        "application/octet-stream".to_string()
    } else {
        base
    }
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

#[cfg(test)]
mod tests {
    use super::sanitized_content_type;

    #[test]
    fn active_types_are_downgraded() {
        for ct in [
            "text/html",
            "text/html; charset=utf-8",
            "IMAGE/SVG+XML",
            "application/javascript",
            "application/xhtml+xml",
        ] {
            assert_eq!(sanitized_content_type(Some(ct)), "application/octet-stream", "{ct}");
        }
    }

    #[test]
    fn benign_types_pass_through_normalised() {
        assert_eq!(sanitized_content_type(Some("audio/basic")), "audio/basic");
        assert_eq!(sanitized_content_type(Some("image/png")), "image/png");
        assert_eq!(sanitized_content_type(Some("application/pdf; x=1")), "application/pdf");
        assert_eq!(sanitized_content_type(None), "application/octet-stream");
    }
}
