//! `/v1/{ring-groups,forwardings}` — multi-destination routing config (Volume 4).
//!
//! Reads are tenant-scoped; writes are admin-gated, mirroring the trunking and provisioning
//! resources. Both are configuration entities (no lifecycle events): a [`RingGroup`] fans an
//! inbound call out to a set of members, and a [`Forwarding`] rule redirects a dialled
//! extension elsewhere (call-forward / follow-me).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use commos_core::common::Uuid;
use commos_core::entities::forwarding::{ForwardMode, Forwarding};
use commos_core::entities::ring_group::{RingGroup, RingStrategy};

use super::admin::AdminContext;
use super::auth::TenantContext;
use super::calls::ListParams;
use super::problem::Problem;
use crate::control::ringing::{ForwardingInput, RingGroupInput, RingingError};
use crate::state::AppState;

/// Upper bounds so a single request cannot store an unbounded blob (mirrors `create_queue`).
const MAX_MEMBERS: usize = 1024;
const MAX_REF_LEN: usize = 512;

fn tid(s: &str) -> Result<Uuid, Problem> {
    Uuid::parse(s).map_err(|_| Problem::bad_request("id is not a valid UUIDv7"))
}

fn rerr(e: RingingError) -> Problem {
    match e {
        RingingError::NotFound => Problem::not_found("no such entity"),
        RingingError::Store(crate::store::StoreError::VersionConflict { .. }) => {
            Problem::new(StatusCode::CONFLICT, "version_conflict", "concurrent modification")
        }
        RingingError::Store(e) => Problem::internal(e.to_string()),
    }
}

/// Validate a member/target reference list against the size bounds.
fn check_refs(refs: &[String]) -> Result<(), Problem> {
    if refs.len() > MAX_MEMBERS {
        return Err(Problem::bad_request(format!("at most {MAX_MEMBERS} entries")));
    }
    if refs.iter().any(|m| m.len() > MAX_REF_LEN) {
        return Err(Problem::bad_request(format!("each ref must be at most {MAX_REF_LEN} characters")));
    }
    Ok(())
}

#[derive(Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

// ---- Ring groups ----------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct WriteRingGroup {
    pub strategy: RingStrategy,
    #[serde(default)]
    pub members: Vec<String>,
    pub ring_seconds: Option<i64>,
    pub no_answer_ref: Option<String>,
    pub label: Option<String>,
}

impl WriteRingGroup {
    fn into_input(self) -> Result<RingGroupInput, Problem> {
        check_refs(&self.members)?;
        Ok(RingGroupInput {
            strategy: self.strategy,
            members: self.members,
            ring_seconds: self.ring_seconds,
            no_answer_ref: self.no_answer_ref,
            label: self.label,
        })
    }
}

/// `GET /v1/ring-groups`
pub async fn list_ring_groups(
    State(st): State<AppState>,
    t: TenantContext,
    Query(p): Query<ListParams>,
) -> Result<Json<Page<RingGroup>>, Problem> {
    let limit = p.limit.unwrap_or(50).clamp(1, 200);
    let page = st
        .store
        .list_ring_groups(t.tenant_id, limit, p.cursor)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?;
    Ok(Json(Page { items: page.items, next_cursor: page.next_cursor }))
}

/// `POST /v1/ring-groups`
pub async fn create_ring_group(
    State(st): State<AppState>,
    admin: AdminContext,
    Json(b): Json<WriteRingGroup>,
) -> Result<impl IntoResponse, Problem> {
    let input = b.into_input()?;
    let g = st
        .ringing
        .create_ring_group(admin.tenant_id, input)
        .await
        .map_err(|e| rerr(RingingError::Store(e)))?;
    Ok((StatusCode::CREATED, Json(g)))
}

/// `GET /v1/ring-groups/{id}`
pub async fn get_ring_group(
    State(st): State<AppState>,
    t: TenantContext,
    Path(id): Path<String>,
) -> Result<Json<RingGroup>, Problem> {
    match st
        .store
        .get_ring_group(t.tenant_id, tid(&id)?)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?
    {
        Some(g) => Ok(Json(g)),
        None => Err(Problem::not_found("no such ring group")),
    }
}

/// `PATCH /v1/ring-groups/{id}` — full replace of the mutable fields.
pub async fn patch_ring_group(
    State(st): State<AppState>,
    admin: AdminContext,
    Path(id): Path<String>,
    Json(b): Json<WriteRingGroup>,
) -> Result<Json<RingGroup>, Problem> {
    let input = b.into_input()?;
    let g = st.ringing.update_ring_group(admin.tenant_id, tid(&id)?, input).await.map_err(rerr)?;
    Ok(Json(g))
}

/// `DELETE /v1/ring-groups/{id}`
pub async fn delete_ring_group(
    State(st): State<AppState>,
    admin: AdminContext,
    Path(id): Path<String>,
) -> Result<StatusCode, Problem> {
    st.ringing
        .delete_ring_group(admin.tenant_id, tid(&id)?)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- Forwarding -----------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct WriteForwarding {
    pub number: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub mode: ForwardMode,
    #[serde(default)]
    pub targets: Vec<String>,
    pub ring_seconds: Option<i64>,
}

fn default_true() -> bool {
    true
}

impl WriteForwarding {
    fn into_input(self) -> Result<ForwardingInput, Problem> {
        if self.number.is_empty() || self.number.len() > MAX_REF_LEN {
            return Err(Problem::bad_request("number must be 1..=512 characters"));
        }
        check_refs(&self.targets)?;
        Ok(ForwardingInput {
            number: self.number,
            enabled: self.enabled,
            mode: self.mode,
            targets: self.targets,
            ring_seconds: self.ring_seconds,
        })
    }
}

/// `GET /v1/forwardings`
pub async fn list_forwardings(
    State(st): State<AppState>,
    t: TenantContext,
    Query(p): Query<ListParams>,
) -> Result<Json<Page<Forwarding>>, Problem> {
    let limit = p.limit.unwrap_or(50).clamp(1, 200);
    let page = st
        .store
        .list_forwardings(t.tenant_id, limit, p.cursor)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?;
    Ok(Json(Page { items: page.items, next_cursor: page.next_cursor }))
}

/// `POST /v1/forwardings`
pub async fn create_forwarding(
    State(st): State<AppState>,
    admin: AdminContext,
    Json(b): Json<WriteForwarding>,
) -> Result<impl IntoResponse, Problem> {
    let input = b.into_input()?;
    let f = st
        .ringing
        .create_forwarding(admin.tenant_id, input)
        .await
        .map_err(|e| rerr(RingingError::Store(e)))?;
    Ok((StatusCode::CREATED, Json(f)))
}

/// `GET /v1/forwardings/{id}`
pub async fn get_forwarding(
    State(st): State<AppState>,
    t: TenantContext,
    Path(id): Path<String>,
) -> Result<Json<Forwarding>, Problem> {
    match st
        .store
        .get_forwarding(t.tenant_id, tid(&id)?)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?
    {
        Some(f) => Ok(Json(f)),
        None => Err(Problem::not_found("no such forwarding rule")),
    }
}

/// `PATCH /v1/forwardings/{id}` — full replace of the mutable fields.
pub async fn patch_forwarding(
    State(st): State<AppState>,
    admin: AdminContext,
    Path(id): Path<String>,
    Json(b): Json<WriteForwarding>,
) -> Result<Json<Forwarding>, Problem> {
    let input = b.into_input()?;
    let f = st.ringing.update_forwarding(admin.tenant_id, tid(&id)?, input).await.map_err(rerr)?;
    Ok(Json(f))
}

/// `DELETE /v1/forwardings/{id}`
pub async fn delete_forwarding(
    State(st): State<AppState>,
    admin: AdminContext,
    Path(id): Path<String>,
) -> Result<StatusCode, Problem> {
    st.ringing
        .delete_forwarding(admin.tenant_id, tid(&id)?)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}
