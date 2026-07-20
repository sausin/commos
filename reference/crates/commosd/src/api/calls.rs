//! `/v1/calls` — the Routing resource (OpenAPI `list_calls` / `create_calls` / `get_calls`).
//!
//! Faithful to the frozen API: cursor pagination returning `{items, next_cursor}`, the
//! `Idempotency-Key` header on create, Problem-details errors, and strict tenant scoping.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use commos_core::common::Uuid;
use commos_core::entities::call::{Call, Direction};

use super::auth::TenantContext;
use super::problem::Problem;
use crate::control::routing::{OriginateRequest, RoutingError};
use crate::state::AppState;

/// Query params for `list_calls` (OpenAPI `Limit` default 50 / max 200, `Cursor`).
#[derive(Deserialize)]
pub struct ListParams {
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

#[derive(Serialize)]
pub struct CallPage {
    pub items: Vec<Call>,
    pub next_cursor: Option<String>,
}

/// `GET /v1/calls`
pub async fn list_calls(
    State(st): State<AppState>,
    tenant: TenantContext,
    Query(params): Query<ListParams>,
) -> Json<CallPage> {
    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    let page = st
        .store
        .list_calls(tenant.tenant_id, limit, params.cursor.as_deref());
    Json(CallPage {
        items: page.items,
        next_cursor: page.next_cursor,
    })
}

/// Body for `create_calls`. The full `Call` schema is the contract shape, but
/// server-managed fields (id, version, state, timestamps) are assigned by the platform;
/// a client supplies only intent.
#[derive(Deserialize)]
pub struct CreateCall {
    pub direction: Direction,
    pub from_ref: String,
    pub to_ref: String,
}

/// `POST /v1/calls` — originate a Call.
pub async fn create_calls(
    State(st): State<AppState>,
    tenant: TenantContext,
    headers: HeaderMap,
    Json(body): Json<CreateCall>,
) -> Result<impl IntoResponse, Problem> {
    if body.from_ref.trim().is_empty() || body.to_ref.trim().is_empty() {
        return Err(Problem::bad_request("from_ref and to_ref are required"));
    }
    let idempotency_key = headers
        .get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let call = st
        .routing
        .originate(
            tenant.tenant_id,
            OriginateRequest {
                direction: body.direction,
                from_ref: body.from_ref,
                to_ref: body.to_ref,
                idempotency_key,
            },
        )
        .map_err(|e| match e {
            RoutingError::MediaRejected(reason) => {
                Problem::new(StatusCode::BAD_GATEWAY, "media_rejected", reason)
            }
            RoutingError::Store(e) => Problem::internal(e.to_string()),
        })?;

    Ok((StatusCode::CREATED, Json(call)))
}

/// `GET /v1/calls/{id}`
pub async fn get_call(
    State(st): State<AppState>,
    tenant: TenantContext,
    Path(id): Path<String>,
) -> Result<Json<Call>, Problem> {
    let id = Uuid::parse(&id).map_err(|_| Problem::bad_request("id is not a valid UUIDv7"))?;
    match st.store.get_call(tenant.tenant_id, id) {
        Some(call) => Ok(Json(call)),
        None => Err(Problem::not_found("no such call")),
    }
}
