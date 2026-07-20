//! `/v1/video-rooms` — the real-time Video workload resource (`list_video_rooms` /
//! `create_video_room` / `get_video_room`).
//!
//! Faithful to the frozen API: cursor pagination returning `{items, next_cursor}`,
//! Problem-details errors, and strict tenant scoping. A real-time peer of `/v1/calls`.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use commos_core::common::Uuid;
use commos_core::entities::video_room::VideoRoom;

use super::auth::TenantContext;
use super::problem::Problem;
use crate::state::AppState;

/// Query params for `list_video_rooms` (OpenAPI `Limit` default 50 / max 200, `Cursor`).
#[derive(Deserialize)]
pub struct ListParams {
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

#[derive(Serialize)]
pub struct VideoRoomPage {
    pub items: Vec<VideoRoom>,
    pub next_cursor: Option<String>,
}

/// `GET /v1/video-rooms`
pub async fn list_video_rooms(
    State(st): State<AppState>,
    tenant: TenantContext,
    Query(params): Query<ListParams>,
) -> Result<Json<VideoRoomPage>, Problem> {
    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    let page = st
        .store
        .list_video_rooms(tenant.tenant_id, limit, params.cursor)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?;
    Ok(Json(VideoRoomPage {
        items: page.items,
        next_cursor: page.next_cursor,
    }))
}

/// Body for `create_video_room`. Server-managed fields (id, version, mode, state,
/// timestamps) are assigned by the platform; a client supplies only intent.
#[derive(Deserialize)]
pub struct CreateVideoRoom {
    pub name: Option<String>,
}

/// `POST /v1/video-rooms` — start a VideoRoom in `ACTIVE`.
pub async fn create_video_room(
    State(st): State<AppState>,
    tenant: TenantContext,
    Json(body): Json<CreateVideoRoom>,
) -> Result<impl IntoResponse, Problem> {
    let room = st
        .realtime
        .start_video_room(tenant.tenant_id, body.name)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?;

    Ok((StatusCode::CREATED, Json(room)))
}

/// `GET /v1/video-rooms/{id}`
pub async fn get_video_room(
    State(st): State<AppState>,
    tenant: TenantContext,
    Path(id): Path<String>,
) -> Result<Json<VideoRoom>, Problem> {
    let id = Uuid::parse(&id).map_err(|_| Problem::bad_request("id is not a valid UUIDv7"))?;
    match st
        .store
        .get_video_room(tenant.tenant_id, id)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?
    {
        Some(room) => Ok(Json(room)),
        None => Err(Problem::not_found("no such video room")),
    }
}
