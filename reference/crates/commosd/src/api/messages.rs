//! `/v1/messages` — the Messaging Message resource (`list_messages` / `create_message` /
//! `get_message`).
//!
//! Faithful to the frozen API: cursor pagination returning `{items, next_cursor}`,
//! Problem-details errors, and strict tenant scoping. The messaging peer of `/v1/calls`.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use commos_core::common::Uuid;
use commos_core::entities::message::Message;

use super::auth::TenantContext;
use super::problem::Problem;
use crate::state::AppState;

/// Query params for `list_messages` (OpenAPI `Limit` default 50 / max 200, `Cursor`).
#[derive(Deserialize)]
pub struct ListParams {
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

#[derive(Serialize)]
pub struct MessagePage {
    pub items: Vec<Message>,
    pub next_cursor: Option<String>,
}

/// `GET /v1/messages`
pub async fn list_messages(
    State(st): State<AppState>,
    tenant: TenantContext,
    Query(params): Query<ListParams>,
) -> Result<Json<MessagePage>, Problem> {
    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    let page = st
        .store
        .list_messages(tenant.tenant_id, limit, params.cursor)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?;
    Ok(Json(MessagePage {
        items: page.items,
        next_cursor: page.next_cursor,
    }))
}

/// Body for `create_message`. Server-managed fields (id, version, state, timestamps) are
/// assigned by the platform; a client supplies only intent.
#[derive(Deserialize)]
pub struct CreateMessage {
    pub channel_id: String,
    pub thread_id: Option<String>,
    pub sender_ref: String,
    pub body: Option<String>,
}

/// `POST /v1/messages` — send a Message on a Channel (optionally within a Thread).
pub async fn create_message(
    State(st): State<AppState>,
    tenant: TenantContext,
    Json(body): Json<CreateMessage>,
) -> Result<impl IntoResponse, Problem> {
    if body.sender_ref.trim().is_empty() {
        return Err(Problem::bad_request("sender_ref is required"));
    }
    let channel_id = Uuid::parse(&body.channel_id)
        .map_err(|_| Problem::bad_request("channel_id is not a valid UUIDv7"))?;
    let thread_id = match body.thread_id {
        Some(t) => Some(
            Uuid::parse(&t).map_err(|_| Problem::bad_request("thread_id is not a valid UUIDv7"))?,
        ),
        None => None,
    };

    let message = st
        .messaging
        .send_message(tenant.tenant_id, channel_id, thread_id, body.sender_ref, body.body)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?;

    Ok((StatusCode::CREATED, Json(message)))
}

/// `GET /v1/messages/{id}`
pub async fn get_message(
    State(st): State<AppState>,
    tenant: TenantContext,
    Path(id): Path<String>,
) -> Result<Json<Message>, Problem> {
    let id = Uuid::parse(&id).map_err(|_| Problem::bad_request("id is not a valid UUIDv7"))?;
    match st
        .store
        .get_message(tenant.tenant_id, id)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?
    {
        Some(message) => Ok(Json(message)),
        None => Err(Problem::not_found("no such message")),
    }
}
