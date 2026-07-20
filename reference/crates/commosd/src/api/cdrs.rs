//! `/v1/cdrs` — the Billing CDR resource (`list_cdrs` / `get_cdr`).
//!
//! A read-only projection: CDRs are produced by Billing from completed Calls, never
//! created through the API. Faithful to the frozen API — cursor pagination returning
//! `{items, next_cursor}`, Problem-details errors and strict tenant scoping.

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use commos_core::common::Uuid;
use commos_core::entities::cdr::Cdr;

use super::auth::TenantContext;
use super::problem::Problem;
use crate::state::AppState;

/// Query params for `list_cdrs` (OpenAPI `Limit` default 50 / max 200, `Cursor`).
#[derive(Deserialize)]
pub struct ListParams {
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

#[derive(Serialize)]
pub struct CdrPage {
    pub items: Vec<Cdr>,
    pub next_cursor: Option<String>,
}

/// `GET /v1/cdrs`
pub async fn list_cdrs(
    State(st): State<AppState>,
    tenant: TenantContext,
    Query(params): Query<ListParams>,
) -> Result<Json<CdrPage>, Problem> {
    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    let page = st
        .store
        .list_cdrs(tenant.tenant_id, limit, params.cursor)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?;
    Ok(Json(CdrPage {
        items: page.items,
        next_cursor: page.next_cursor,
    }))
}

/// `GET /v1/cdrs/{id}`
pub async fn get_cdr(
    State(st): State<AppState>,
    tenant: TenantContext,
    Path(id): Path<String>,
) -> Result<Json<Cdr>, Problem> {
    let id = Uuid::parse(&id).map_err(|_| Problem::bad_request("id is not a valid UUIDv7"))?;
    match st
        .store
        .get_cdr(tenant.tenant_id, id)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?
    {
        Some(cdr) => Ok(Json(cdr)),
        None => Err(Problem::not_found("no such cdr")),
    }
}
