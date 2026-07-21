//! `/v1/channels` — the Messaging Channel resource (`list_channels` / `create_channel` /
//! `get_channel`).
//!
//! Faithful to the frozen API: cursor pagination returning `{items, next_cursor}`,
//! Problem-details errors, and strict tenant scoping. The messaging peer of `/v1/calls`.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use commos_core::common::Uuid;
use commos_core::entities::channel::{Channel, ChannelKind};

use super::auth::TenantContext;
use super::problem::Problem;
use crate::state::AppState;

/// Query params for `list_channels` (OpenAPI `Limit` default 50 / max 200, `Cursor`).
#[derive(Deserialize)]
pub struct ListParams {
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

#[derive(Serialize)]
pub struct ChannelPage {
    pub items: Vec<Channel>,
    pub next_cursor: Option<String>,
}

/// `GET /v1/channels`
pub async fn list_channels(
    State(st): State<AppState>,
    tenant: TenantContext,
    Query(params): Query<ListParams>,
) -> Result<Json<ChannelPage>, Problem> {
    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    let page = st
        .store
        .list_channels(tenant.tenant_id, limit, params.cursor)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?;
    Ok(Json(ChannelPage {
        items: page.items,
        next_cursor: page.next_cursor,
    }))
}

/// Body for `create_channel`. Server-managed fields (id, version, state, timestamps) are
/// assigned by the platform; a client supplies only intent.
#[derive(Deserialize)]
pub struct CreateChannel {
    pub kind: ChannelKind,
    pub name: Option<String>,
    #[serde(default)]
    pub members: Vec<String>,
}

/// `POST /v1/channels` — create a Channel.
pub async fn create_channel(
    State(st): State<AppState>,
    tenant: TenantContext,
    Json(body): Json<CreateChannel>,
) -> Result<impl IntoResponse, Problem> {
    let channel = st
        .messaging
        .create_channel(tenant.tenant_id, body.kind, body.name, body.members)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?;

    Ok((StatusCode::CREATED, Json(channel)))
}

/// `GET /v1/channels/{id}`
pub async fn get_channel(
    State(st): State<AppState>,
    tenant: TenantContext,
    Path(id): Path<String>,
) -> Result<Json<Channel>, Problem> {
    let id = Uuid::parse(&id).map_err(|_| Problem::bad_request("id is not a valid UUIDv7"))?;
    match st
        .store
        .get_channel(tenant.tenant_id, id)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?
    {
        Some(channel) => Ok(Json(channel)),
        None => Err(Problem::not_found("no such channel")),
    }
}
