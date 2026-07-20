//! `/v1/queues` — the contact-centre Queue resource (`list_queues` / `create_queue` /
//! `get_queue`).
//!
//! Faithful to the frozen API: cursor pagination returning `{items, next_cursor}`,
//! Problem-details errors, and strict tenant scoping. The contact-centre peer of
//! `/v1/channels` and `/v1/calls`.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use commos_core::common::Uuid;
use commos_core::entities::queue::{Queue, QueueStrategy};

use super::auth::TenantContext;
use super::problem::Problem;
use crate::state::AppState;

/// Query params for `list_queues` (OpenAPI `Limit` default 50 / max 200, `Cursor`).
#[derive(Deserialize)]
pub struct ListParams {
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

#[derive(Serialize)]
pub struct QueuePage {
    pub items: Vec<Queue>,
    pub next_cursor: Option<String>,
}

/// `GET /v1/queues`
pub async fn list_queues(
    State(st): State<AppState>,
    tenant: TenantContext,
    Query(params): Query<ListParams>,
) -> Result<Json<QueuePage>, Problem> {
    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    let page = st
        .store
        .list_queues(tenant.tenant_id, limit, params.cursor)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?;
    Ok(Json(QueuePage {
        items: page.items,
        next_cursor: page.next_cursor,
    }))
}

/// Body for `create_queue`. Server-managed fields (id, version, timestamps) are assigned by
/// the platform; a client supplies only intent.
#[derive(Deserialize)]
pub struct CreateQueue {
    pub strategy: QueueStrategy,
    #[serde(default)]
    pub members: Vec<String>,
    pub sla_seconds: Option<i64>,
    pub max_wait_ms: Option<i64>,
    pub overflow_ref: Option<String>,
}

/// `POST /v1/queues` — create a Queue.
pub async fn create_queue(
    State(st): State<AppState>,
    tenant: TenantContext,
    Json(body): Json<CreateQueue>,
) -> Result<impl IntoResponse, Problem> {
    let queue = st
        .queues
        .create_queue(
            tenant.tenant_id,
            body.strategy,
            body.members,
            body.sla_seconds,
            body.max_wait_ms,
            body.overflow_ref,
        )
        .await
        .map_err(|e| Problem::internal(e.to_string()))?;

    Ok((StatusCode::CREATED, Json(queue)))
}

/// `GET /v1/queues/{id}`
pub async fn get_queue(
    State(st): State<AppState>,
    tenant: TenantContext,
    Path(id): Path<String>,
) -> Result<Json<Queue>, Problem> {
    let id = Uuid::parse(&id).map_err(|_| Problem::bad_request("id is not a valid UUIDv7"))?;
    match st
        .store
        .get_queue(tenant.tenant_id, id)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?
    {
        Some(queue) => Ok(Json(queue)),
        None => Err(Problem::not_found("no such queue")),
    }
}
