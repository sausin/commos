//! `/v1/ivrs` — interactive-voice-response menu nodes (Volume 4; capability `routing.manage`).
//!
//! Reads (list/get) are tenant-scoped; writes (create/patch/delete) are admin-gated, mirroring
//! the provisioning directory. An IVR is configuration (no lifecycle events); the menu runtime
//! (prompt playback + DTMF collection) is media-plane work.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use commos_core::common::Uuid;
use commos_core::entities::ivr::Ivr;

use super::admin::AdminContext;
use super::auth::TenantContext;
use super::calls::ListParams;
use super::problem::Problem;
use crate::control::ivr::{IvrError, IvrPatch};
use crate::state::AppState;

fn iid(s: &str) -> Result<Uuid, Problem> {
    Uuid::parse(s).map_err(|_| Problem::bad_request("id is not a valid UUIDv7"))
}

/// Map an IVR-service error onto the right Problem status.
fn ierr(e: IvrError) -> Problem {
    match e {
        IvrError::NotFound => Problem::not_found("no such ivr"),
        IvrError::Store(crate::store::StoreError::VersionConflict { .. }) => {
            Problem::new(StatusCode::CONFLICT, "version_conflict", "concurrent modification")
        }
        IvrError::Store(e) => Problem::internal(e.to_string()),
    }
}

/// Parse an optional string UUID field from a request body.
fn opt_id(s: Option<String>, field: &'static str) -> Result<Option<Uuid>, Problem> {
    match s {
        Some(v) => Uuid::parse(&v)
            .map(Some)
            .map_err(|_| Problem::bad_request(format!("{field} is not a valid UUIDv7"))),
        None => Ok(None),
    }
}

#[derive(Serialize)]
pub struct IvrPage {
    pub items: Vec<Ivr>,
    pub next_cursor: Option<String>,
}

/// `GET /v1/ivrs`
pub async fn list_ivrs(
    State(st): State<AppState>,
    t: TenantContext,
    Query(p): Query<ListParams>,
) -> Result<Json<IvrPage>, Problem> {
    let limit = p.limit.unwrap_or(50).clamp(1, 200);
    let page = st.ivrs.list(t.tenant_id, limit, p.cursor).await.map_err(ierr)?;
    Ok(Json(IvrPage { items: page.items, next_cursor: page.next_cursor }))
}

/// Body for `create_ivr`. `options` (the digit map) is required; the rest are optional.
#[derive(Deserialize)]
pub struct CreateIvr {
    pub options: serde_json::Value,
    pub prompt_object_id: Option<String>,
    pub timeout_ms: Option<i64>,
    pub invalid_action: Option<String>,
}

/// `POST /v1/ivrs` — create an IVR menu (admin).
pub async fn create_ivr(
    State(st): State<AppState>,
    admin: AdminContext,
    Json(body): Json<CreateIvr>,
) -> Result<impl IntoResponse, Problem> {
    let prompt = opt_id(body.prompt_object_id, "prompt_object_id")?;
    let ivr = st
        .ivrs
        .create(admin.tenant_id, body.options, prompt, body.timeout_ms, body.invalid_action)
        .await
        .map_err(ierr)?;
    Ok((StatusCode::CREATED, Json(ivr)))
}

/// `GET /v1/ivrs/{id}`
pub async fn get_ivr(
    State(st): State<AppState>,
    t: TenantContext,
    Path(id): Path<String>,
) -> Result<Json<Ivr>, Problem> {
    let ivr = st.ivrs.get(t.tenant_id, iid(&id)?).await.map_err(ierr)?;
    Ok(Json(ivr))
}

/// Body for `patch_ivr` — each present field replaces the stored value (admin).
#[derive(Deserialize)]
pub struct PatchIvr {
    pub options: Option<serde_json::Value>,
    pub prompt_object_id: Option<String>,
    pub timeout_ms: Option<i64>,
    pub invalid_action: Option<String>,
}

/// `PATCH /v1/ivrs/{id}`
pub async fn patch_ivr(
    State(st): State<AppState>,
    admin: AdminContext,
    Path(id): Path<String>,
    Json(body): Json<PatchIvr>,
) -> Result<Json<Ivr>, Problem> {
    let patch = IvrPatch {
        prompt_object_id: opt_id(body.prompt_object_id, "prompt_object_id")?,
        options: body.options,
        timeout_ms: body.timeout_ms,
        invalid_action: body.invalid_action,
    };
    let ivr = st.ivrs.update(admin.tenant_id, iid(&id)?, patch).await.map_err(ierr)?;
    Ok(Json(ivr))
}

/// `DELETE /v1/ivrs/{id}` — config hard delete (admin).
pub async fn delete_ivr(
    State(st): State<AppState>,
    admin: AdminContext,
    Path(id): Path<String>,
) -> Result<StatusCode, Problem> {
    st.ivrs.delete(admin.tenant_id, iid(&id)?).await.map_err(ierr)?;
    Ok(StatusCode::NO_CONTENT)
}
