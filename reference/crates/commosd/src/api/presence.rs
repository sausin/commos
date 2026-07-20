//! `/v1/presence` — the real-time Presence workload resource (`list_presence` /
//! `set_presence` / `get_presence`).
//!
//! Faithful to the frozen API: cursor pagination returning `{items, next_cursor}`,
//! Problem-details errors, and strict tenant scoping. A real-time peer of `/v1/calls`.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use commos_core::common::Uuid;
use commos_core::entities::presence_state::{PresenceState, PresenceStatus};

use super::auth::TenantContext;
use super::problem::Problem;
use crate::state::AppState;

/// Query params for `list_presence` (OpenAPI `Limit` default 50 / max 200, `Cursor`).
#[derive(Deserialize)]
pub struct ListParams {
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

#[derive(Serialize)]
pub struct PresencePage {
    pub items: Vec<PresenceState>,
    pub next_cursor: Option<String>,
}

/// `GET /v1/presence`
pub async fn list_presence(
    State(st): State<AppState>,
    tenant: TenantContext,
    Query(params): Query<ListParams>,
) -> Result<Json<PresencePage>, Problem> {
    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    let page = st
        .store
        .list_presence(tenant.tenant_id, limit, params.cursor)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?;
    Ok(Json(PresencePage {
        items: page.items,
        next_cursor: page.next_cursor,
    }))
}

/// Body for `set_presence`. The subject is the schema's `user_id` (UUIDv7); `status` is the
/// availability signal. Server-managed fields (id, version, timestamps) are assigned by the
/// platform.
#[derive(Deserialize)]
pub struct SetPresence {
    pub user_id: String,
    pub status: PresenceStatus,
}

/// `POST /v1/presence` — record a user's current presence.
pub async fn set_presence(
    State(st): State<AppState>,
    tenant: TenantContext,
    Json(body): Json<SetPresence>,
) -> Result<impl IntoResponse, Problem> {
    let user_id = Uuid::parse(&body.user_id)
        .map_err(|_| Problem::bad_request("user_id is not a valid UUIDv7"))?;

    let presence = st
        .realtime
        .set_presence(tenant.tenant_id, user_id, body.status)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?;

    Ok((StatusCode::CREATED, Json(presence)))
}

/// `GET /v1/presence/{id}`
pub async fn get_presence(
    State(st): State<AppState>,
    tenant: TenantContext,
    Path(id): Path<String>,
) -> Result<Json<PresenceState>, Problem> {
    let id = Uuid::parse(&id).map_err(|_| Problem::bad_request("id is not a valid UUIDv7"))?;
    match st
        .store
        .get_presence(tenant.tenant_id, id)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?
    {
        Some(presence) => Ok(Json(presence)),
        None => Err(Problem::not_found("no such presence state")),
    }
}
