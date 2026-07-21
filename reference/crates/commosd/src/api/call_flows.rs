//! `/v1/call-flows` — versioned routing programs (Volume 4; capability `routing.manage`).
//!
//! Reads (list/get/revisions) are tenant-scoped; writes (create/edit/publish/rollback) are
//! admin-gated, mirroring the provisioning directory. Publishing snapshots the draft graph
//! into immutable, append-only revision history and emits `CallFlowPublished`; rollback
//! republishes a prior revision as a new one (`/publish` and `/rollback` are the action
//! sub-paths — same command, same event — since axum can't bind `{id}:publish` in one
//! segment).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use commos_core::common::Uuid;
use commos_core::entities::call_flow::{CallFlow, CallFlowRevision};

use super::admin::AdminContext;
use super::auth::TenantContext;
use super::calls::ListParams;
use super::problem::Problem;
use crate::control::callflow::CallFlowError;
use crate::state::AppState;

fn cid(s: &str) -> Result<Uuid, Problem> {
    Uuid::parse(s).map_err(|_| Problem::bad_request("id is not a valid UUIDv7"))
}

/// Map a call-flow-service error onto the right Problem status.
fn cferr(e: CallFlowError) -> Problem {
    match e {
        CallFlowError::NotFound => Problem::not_found("no such call flow"),
        CallFlowError::RevisionNotFound => Problem::not_found("no such published revision"),
        CallFlowError::Store(crate::store::StoreError::VersionConflict { .. }) => {
            Problem::new(StatusCode::CONFLICT, "version_conflict", "concurrent modification")
        }
        CallFlowError::Store(e) => Problem::internal(e.to_string()),
    }
}

#[derive(Serialize)]
pub struct CallFlowPage {
    pub items: Vec<CallFlow>,
    pub next_cursor: Option<String>,
}

/// `GET /v1/call-flows`
pub async fn list_call_flows(
    State(st): State<AppState>,
    t: TenantContext,
    Query(p): Query<ListParams>,
) -> Result<Json<CallFlowPage>, Problem> {
    let limit = p.limit.unwrap_or(50).clamp(1, 200);
    let page = st.call_flows.list(t.tenant_id, limit, p.cursor).await.map_err(cferr)?;
    Ok(Json(CallFlowPage { items: page.items, next_cursor: page.next_cursor }))
}

/// Body for `create_call_flow`. Server-managed fields (id, version, state, timestamps) are
/// assigned by the platform; a client supplies only `name` and an optional initial `graph`.
#[derive(Deserialize)]
pub struct CreateCallFlow {
    pub name: String,
    #[serde(default)]
    pub graph: Option<serde_json::Value>,
}

/// `POST /v1/call-flows` — create a DRAFT call flow (admin).
pub async fn create_call_flow(
    State(st): State<AppState>,
    admin: AdminContext,
    Json(body): Json<CreateCallFlow>,
) -> Result<impl IntoResponse, Problem> {
    let cf = st.call_flows.create(admin.tenant_id, body.name, body.graph).await.map_err(cferr)?;
    Ok((StatusCode::CREATED, Json(cf)))
}

/// `GET /v1/call-flows/{id}`
pub async fn get_call_flow(
    State(st): State<AppState>,
    t: TenantContext,
    Path(id): Path<String>,
) -> Result<Json<CallFlow>, Problem> {
    let cf = st.call_flows.get(t.tenant_id, cid(&id)?).await.map_err(cferr)?;
    Ok(Json(cf))
}

/// Body for `patch_call_flow` — edit the draft `name` and/or `graph` (admin).
#[derive(Deserialize)]
pub struct PatchCallFlow {
    pub name: Option<String>,
    pub graph: Option<serde_json::Value>,
}

/// `PATCH /v1/call-flows/{id}` — edit the draft (returns the flow to DRAFT).
pub async fn patch_call_flow(
    State(st): State<AppState>,
    admin: AdminContext,
    Path(id): Path<String>,
    Json(body): Json<PatchCallFlow>,
) -> Result<Json<CallFlow>, Problem> {
    let cf = st
        .call_flows
        .edit(admin.tenant_id, cid(&id)?, body.name, body.graph)
        .await
        .map_err(cferr)?;
    Ok(Json(cf))
}

/// `POST /v1/call-flows/{id}/publish` — publish the current draft (admin), emitting
/// `CallFlowPublished`.
pub async fn publish_call_flow(
    State(st): State<AppState>,
    admin: AdminContext,
    Path(id): Path<String>,
) -> Result<Json<CallFlow>, Problem> {
    let cf = st.call_flows.publish(admin.tenant_id, cid(&id)?).await.map_err(cferr)?;
    Ok(Json(cf))
}

/// Body for `rollback_call_flow`: the prior published version to republish.
#[derive(Deserialize)]
pub struct RollbackCallFlow {
    pub target_version: u64,
}

/// `POST /v1/call-flows/{id}/rollback` — republish a prior revision as a new PUBLISHED
/// version (admin). Append-only: the target revision is never mutated.
pub async fn rollback_call_flow(
    State(st): State<AppState>,
    admin: AdminContext,
    Path(id): Path<String>,
    Json(body): Json<RollbackCallFlow>,
) -> Result<Json<CallFlow>, Problem> {
    let cf = st
        .call_flows
        .rollback(admin.tenant_id, cid(&id)?, body.target_version)
        .await
        .map_err(cferr)?;
    Ok(Json(cf))
}

#[derive(Serialize)]
pub struct RevisionsPage {
    pub items: Vec<CallFlowRevision>,
}

/// `GET /v1/call-flows/{id}/revisions` — the append-only publish history (ascending by
/// version). Operational read outside the frozen surface, alongside publish/rollback.
pub async fn list_call_flow_revisions(
    State(st): State<AppState>,
    t: TenantContext,
    Path(id): Path<String>,
) -> Result<Json<RevisionsPage>, Problem> {
    let items = st.call_flows.revisions(t.tenant_id, cid(&id)?).await.map_err(cferr)?;
    Ok(Json(RevisionsPage { items }))
}
