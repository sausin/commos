//! `/v1/threads` — the Messaging Thread resource (`list_threads` / `create_thread` /
//! `get_thread`).
//!
//! Faithful to the frozen API: cursor pagination returning `{items, next_cursor}`,
//! Problem-details errors, and strict tenant scoping. The messaging peer of `/v1/calls`.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use commos_core::common::Uuid;
use commos_core::entities::thread::Thread;

use super::auth::TenantContext;
use super::problem::Problem;
use crate::state::AppState;

/// Query params for `list_threads` (OpenAPI `Limit` default 50 / max 200, `Cursor`).
#[derive(Deserialize)]
pub struct ListParams {
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

#[derive(Serialize)]
pub struct ThreadPage {
    pub items: Vec<Thread>,
    pub next_cursor: Option<String>,
}

/// `GET /v1/threads`
pub async fn list_threads(
    State(st): State<AppState>,
    tenant: TenantContext,
    Query(params): Query<ListParams>,
) -> Result<Json<ThreadPage>, Problem> {
    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    let page = st
        .store
        .list_threads(tenant.tenant_id, limit, params.cursor)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?;
    Ok(Json(ThreadPage {
        items: page.items,
        next_cursor: page.next_cursor,
    }))
}

/// Body for `create_thread`. Server-managed fields (id, version, state, timestamps) are
/// assigned by the platform; a client supplies only intent.
#[derive(Deserialize)]
pub struct CreateThread {
    pub channel_id: String,
    pub subject: Option<String>,
}

/// `POST /v1/threads` — open a Thread in a Channel.
pub async fn create_thread(
    State(st): State<AppState>,
    tenant: TenantContext,
    Json(body): Json<CreateThread>,
) -> Result<impl IntoResponse, Problem> {
    let channel_id = Uuid::parse(&body.channel_id)
        .map_err(|_| Problem::bad_request("channel_id is not a valid UUIDv7"))?;

    let thread = st
        .messaging
        .open_thread(tenant.tenant_id, channel_id, body.subject)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?;

    Ok((StatusCode::CREATED, Json(thread)))
}

/// `GET /v1/threads/{id}`
pub async fn get_thread(
    State(st): State<AppState>,
    tenant: TenantContext,
    Path(id): Path<String>,
) -> Result<Json<Thread>, Problem> {
    let id = Uuid::parse(&id).map_err(|_| Problem::bad_request("id is not a valid UUIDv7"))?;
    match st
        .store
        .get_thread(tenant.tenant_id, id)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?
    {
        Some(thread) => Ok(Json(thread)),
        None => Err(Problem::not_found("no such thread")),
    }
}
