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
use commos_core::events::call_transferred::TransferType;

use super::auth::TenantContext;
use super::problem::Problem;
use crate::control::routing::{OriginateRequest, RoutingError};
use crate::state::AppState;
use crate::store::{StoreError, Tx};

/// Parse a path id into a validated UUIDv7 or a 400 Problem.
fn parse_id(id: &str) -> Result<Uuid, Problem> {
    Uuid::parse(id).map_err(|_| Problem::bad_request("id is not a valid UUIDv7"))
}

/// Map a Routing error to its Problem-details response (Volume 4 §5).
fn map_routing_err(e: RoutingError) -> Problem {
    match e {
        RoutingError::NotFound => Problem::not_found("no such call"),
        RoutingError::IllegalState(m) => Problem::new(StatusCode::CONFLICT, "illegal_state", m),
        RoutingError::MediaRejected(m) => Problem::new(StatusCode::BAD_GATEWAY, "media_rejected", m),
        RoutingError::PolicyDenied(m) => Problem::new(StatusCode::FORBIDDEN, "policy_denied", m),
        RoutingError::Store(e) => Problem::internal(e.to_string()),
    }
}

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
) -> Result<Json<CallPage>, Problem> {
    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    let page = st
        .store
        .list_calls(tenant.tenant_id, limit, params.cursor)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?;
    Ok(Json(CallPage {
        items: page.items,
        next_cursor: page.next_cursor,
    }))
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
        .await
        .map_err(map_routing_err)?;

    Ok((StatusCode::CREATED, Json(call)))
}

/// `GET /v1/calls/{id}`
pub async fn get_call(
    State(st): State<AppState>,
    tenant: TenantContext,
    Path(id): Path<String>,
) -> Result<Json<Call>, Problem> {
    let id = Uuid::parse(&id).map_err(|_| Problem::bad_request("id is not a valid UUIDv7"))?;
    match st
        .store
        .get_call(tenant.tenant_id, id)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?
    {
        Some(call) => Ok(Json(call)),
        None => Err(Problem::not_found("no such call")),
    }
}

// --- Action verbs (Volume 4 `/v1/calls/{id}:<action>`; mounted as sub-paths). ------------
// Each transitions the Call (where applicable) and emits the corresponding event through the
// transactional outbox, then commands the media plane.

/// `POST /v1/calls/{id}:hold`
pub async fn hold_call(
    State(st): State<AppState>,
    tenant: TenantContext,
    Path(id): Path<String>,
) -> Result<Json<Call>, Problem> {
    let id = parse_id(&id)?;
    st.routing
        .hold(tenant.tenant_id, id)
        .await
        .map(Json)
        .map_err(map_routing_err)
}

/// `POST /v1/calls/{id}:resume`
pub async fn resume_call(
    State(st): State<AppState>,
    tenant: TenantContext,
    Path(id): Path<String>,
) -> Result<Json<Call>, Problem> {
    let id = parse_id(&id)?;
    st.routing
        .resume(tenant.tenant_id, id)
        .await
        .map(Json)
        .map_err(map_routing_err)
}

/// `POST /v1/calls/{id}:hangup`
pub async fn hangup_call(
    State(st): State<AppState>,
    tenant: TenantContext,
    Path(id): Path<String>,
) -> Result<Json<Call>, Problem> {
    let id = parse_id(&id)?;
    st.routing
        .hangup(tenant.tenant_id, id, Some("NORMAL_CLEARING".to_string()))
        .await
        .map(Json)
        .map_err(map_routing_err)
}

/// Body for `transfer`: the target is required; kind defaults to BLIND.
#[derive(Deserialize)]
pub struct TransferBody {
    pub to_ref: String,
    #[serde(default)]
    pub transfer_type: Option<TransferType>,
    #[serde(default)]
    pub from_ref: Option<String>,
}

/// `POST /v1/calls/{id}:transfer`
pub async fn transfer_call(
    State(st): State<AppState>,
    tenant: TenantContext,
    Path(id): Path<String>,
    Json(body): Json<TransferBody>,
) -> Result<Json<Call>, Problem> {
    let id = parse_id(&id)?;
    if body.to_ref.trim().is_empty() {
        return Err(Problem::bad_request("to_ref is required"));
    }
    let transfer_type = body.transfer_type.unwrap_or(TransferType::Blind);
    st.routing
        .transfer(tenant.tenant_id, id, body.to_ref, transfer_type, body.from_ref)
        .await
        .map(Json)
        .map_err(map_routing_err)
}

// --- Optimistic-concurrency update (OpenAPI `update_calls`). ------------------------------
// `PATCH /v1/calls/{id}` with `If-Match` + RFC 7386 JSON Merge Patch (Volume 4).

/// Apply an RFC 7386 JSON Merge Patch: for an object patch, recurse per key —
/// a `null` value removes the key, any other value replaces it; a non-object patch
/// replaces the target wholesale.
fn merge(target: &mut serde_json::Value, patch: &serde_json::Value) {
    match patch {
        serde_json::Value::Object(patch_obj) => {
            // If the target is not an object, RFC 7386 says start from an empty one.
            if !target.is_object() {
                *target = serde_json::Value::Object(serde_json::Map::new());
            }
            let target_obj = target.as_object_mut().expect("target is an object");
            for (key, value) in patch_obj {
                if value.is_null() {
                    target_obj.remove(key);
                } else {
                    merge(target_obj.entry(key.clone()).or_insert(serde_json::Value::Null), value);
                }
            }
        }
        _ => *target = patch.clone(),
    }
}

/// `PATCH /v1/calls/{id}` — optimistic-concurrency update via JSON Merge Patch (RFC 7386).
///
/// A generic PATCH has no dedicated catalogue event, so this MVP persists the change
/// WITHOUT emitting one. This is a deliberate deviation from the "no state change without
/// its event" guarantee that the action verbs uphold; a future refinement should emit an
/// audit/`updated` event through the outbox alongside the write.
pub async fn patch_call(
    State(st): State<AppState>,
    tenant: TenantContext,
    Path(id): Path<String>,
    headers: HeaderMap,
    // Accept any content type (contract: `application/merge-patch+json`); read the raw JSON.
    Json(patch): Json<serde_json::Value>,
) -> Result<Json<Call>, Problem> {
    let id = parse_id(&id)?;

    // 1. Load the current call (tenant-scoped).
    let mut call = st
        .store
        .get_call(tenant.tenant_id, id)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?
        .ok_or_else(|| Problem::not_found("no such call"))?;

    // 2. Require `If-Match` and check it against the current version.
    let if_match = headers
        .get(axum::http::header::IF_MATCH)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            Problem::new(
                StatusCode::PRECONDITION_REQUIRED,
                "if_match_required",
                "If-Match header with the entity version is required",
            )
        })?;
    // Tolerate a quoted ETag form (e.g. `"3"`) as well as a bare integer.
    let expected: u64 = if_match
        .trim()
        .trim_matches('"')
        .parse()
        .map_err(|_| Problem::bad_request("If-Match must be an integer entity version"))?;
    if expected != call.base.version {
        return Err(Problem::new(
            StatusCode::PRECONDITION_FAILED,
            "version_conflict",
            format!(
                "If-Match {expected} does not match current version {}",
                call.base.version
            ),
        ));
    }

    // 3. Serialize, apply the merge patch.
    let original = serde_json::to_value(&call).map_err(|e| Problem::internal(e.to_string()))?;
    let mut merged = original.clone();
    merge(&mut merged, &patch);

    // 4. Protect server-managed identity/tenant fields: force them back to their originals
    //    so a patch cannot re-home or re-identify the entity, then re-validate the shape.
    if let Some(obj) = merged.as_object_mut() {
        for field in ["id", "tenant_id", "created_at", "correlation_id"] {
            if let Some(orig) = original.get(field) {
                obj.insert(field.to_string(), orig.clone());
            }
        }
    }
    call = serde_json::from_value(merged)
        .map_err(|e| Problem::bad_request(format!("patch produced an invalid Call: {e}")))?;

    // 5. Bump version + updated_at so the store's If-Match check (version = N updates only a
    //    stored row at N-1) guards the write, then commit.
    call.base.touch();
    st.store
        .commit(Tx {
            calls: vec![call.clone()],
            ..Default::default()
        })
        .await
        .map_err(|e| match e {
            StoreError::VersionConflict { .. } => Problem::new(
                StatusCode::PRECONDITION_FAILED,
                "version_conflict",
                "the call was modified concurrently; reload and retry",
            ),
            other => Problem::internal(other.to_string()),
        })?;

    // 6. Return the updated Call.
    Ok(Json(call))
}
