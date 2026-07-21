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
use crate::control::agents::{Assignment, EnqueueError};
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
    // Bound the member set so a single request cannot store an unbounded blob.
    if body.members.len() > 1024 {
        return Err(Problem::bad_request("members must be at most 1024 entries"));
    }
    if body.members.iter().any(|m| m.len() > 512) {
        return Err(Problem::bad_request("each member ref must be at most 512 characters"));
    }
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

/// Body for `enqueue_call`: the Call to distribute onto this Queue.
#[derive(Deserialize)]
pub struct EnqueueCall {
    pub call_id: String,
}

/// `POST /v1/queues/{id}/enqueue` — enqueue a Call and distribute it to an available agent
/// (basic ACD). Returns the [`Assignment`] naming the agent that took the call.
///
/// Maps [`EnqueueError`] → `404` (no such queue) / `409` (no available agent).
pub async fn enqueue_call(
    State(st): State<AppState>,
    tenant: TenantContext,
    Path(queue_id): Path<String>,
    Json(body): Json<EnqueueCall>,
) -> Result<Json<Assignment>, Problem> {
    let queue_id =
        Uuid::parse(&queue_id).map_err(|_| Problem::bad_request("id is not a valid UUIDv7"))?;
    let call_id = Uuid::parse(&body.call_id)
        .map_err(|_| Problem::bad_request("call_id is not a valid UUIDv7"))?;

    let assignment = st
        .agents
        .enqueue(tenant.tenant_id, queue_id, call_id)
        .await
        .map_err(|e| match e {
            EnqueueError::QueueNotFound => Problem::not_found("no such queue"),
            EnqueueError::NoAgentAvailable => Problem::new(
                StatusCode::CONFLICT,
                "no_agent_available",
                "no available agent to take the call",
            ),
            EnqueueError::Store(inner) => Problem::internal(inner.to_string()),
        })?;

    Ok(Json(assignment))
}
