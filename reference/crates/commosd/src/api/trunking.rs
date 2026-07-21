//! `/v1/{carriers,gateways,trunks,dids}` — PSTN / SIP trunking config (Volume 4).
//!
//! Reads are tenant-scoped (`carriers.manage` / `numbering.manage`); writes are admin-gated,
//! mirroring the provisioning directory. These are configuration resources (no lifecycle
//! events); they wire the platform to real carriers for outbound calls and inbound DIDs.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use commos_core::common::Uuid;
use commos_core::entities::carrier::{Carrier, CarrierKind};
use commos_core::entities::did::Did;
use commos_core::entities::gateway::{Gateway, GatewayHealth, GatewayKind};
use commos_core::entities::trunk::Trunk;

use super::admin::AdminContext;
use super::auth::TenantContext;
use super::calls::ListParams;
use super::problem::Problem;
use crate::control::trunking::TrunkingError;
use crate::state::AppState;

fn tid(s: &str) -> Result<Uuid, Problem> {
    Uuid::parse(s).map_err(|_| Problem::bad_request("id is not a valid UUIDv7"))
}
fn opt_id(s: Option<String>, field: &'static str) -> Result<Option<Uuid>, Problem> {
    match s {
        Some(v) => Uuid::parse(&v).map(Some).map_err(|_| Problem::bad_request(format!("{field} is not a valid UUIDv7"))),
        None => Ok(None),
    }
}
fn terr(e: TrunkingError) -> Problem {
    match e {
        TrunkingError::NotFound => Problem::not_found("no such trunking entity"),
        TrunkingError::Store(crate::store::StoreError::VersionConflict { .. }) => {
            Problem::new(StatusCode::CONFLICT, "version_conflict", "concurrent modification")
        }
        TrunkingError::Store(e) => Problem::internal(e.to_string()),
    }
}

#[derive(Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

// ---- Carriers -------------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CreateCarrier {
    pub name: String,
    pub kind: CarrierKind,
    pub rating_profile_id: Option<String>,
}

/// `GET /v1/carriers`
pub async fn list_carriers(
    State(st): State<AppState>,
    t: TenantContext,
    Query(p): Query<ListParams>,
) -> Result<Json<Page<Carrier>>, Problem> {
    let limit = p.limit.unwrap_or(50).clamp(1, 200);
    let page = st.trunking.list_carriers(t.tenant_id, limit, p.cursor).await.map_err(terr)?;
    Ok(Json(Page { items: page.items, next_cursor: page.next_cursor }))
}
/// `POST /v1/carriers`
pub async fn create_carrier(
    State(st): State<AppState>,
    admin: AdminContext,
    Json(b): Json<CreateCarrier>,
) -> Result<impl IntoResponse, Problem> {
    let rating = opt_id(b.rating_profile_id, "rating_profile_id")?;
    let c = st.trunking.create_carrier(admin.tenant_id, b.name, b.kind, rating).await.map_err(terr)?;
    Ok((StatusCode::CREATED, Json(c)))
}
/// `GET /v1/carriers/{id}`
pub async fn get_carrier(State(st): State<AppState>, t: TenantContext, Path(id): Path<String>) -> Result<Json<Carrier>, Problem> {
    Ok(Json(st.trunking.get_carrier(t.tenant_id, tid(&id)?).await.map_err(terr)?))
}
/// `DELETE /v1/carriers/{id}`
pub async fn delete_carrier(State(st): State<AppState>, admin: AdminContext, Path(id): Path<String>) -> Result<StatusCode, Problem> {
    st.trunking.delete_carrier(admin.tenant_id, tid(&id)?).await.map_err(terr)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- Gateways -------------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CreateGateway {
    pub carrier_id: String,
    pub kind: GatewayKind,
    pub address: Option<String>,
    pub health: Option<GatewayHealth>,
}
#[derive(Deserialize)]
pub struct PatchGateway {
    pub health: GatewayHealth,
}

/// `GET /v1/gateways`
pub async fn list_gateways(
    State(st): State<AppState>,
    t: TenantContext,
    Query(p): Query<ListParams>,
) -> Result<Json<Page<Gateway>>, Problem> {
    let limit = p.limit.unwrap_or(50).clamp(1, 200);
    let page = st.trunking.list_gateways(t.tenant_id, limit, p.cursor).await.map_err(terr)?;
    Ok(Json(Page { items: page.items, next_cursor: page.next_cursor }))
}
/// `POST /v1/gateways`
pub async fn create_gateway(
    State(st): State<AppState>,
    admin: AdminContext,
    Json(b): Json<CreateGateway>,
) -> Result<impl IntoResponse, Problem> {
    let carrier = tid(&b.carrier_id)?;
    let g = st
        .trunking
        .create_gateway(admin.tenant_id, carrier, b.kind, b.address, b.health.unwrap_or(GatewayHealth::Online))
        .await
        .map_err(terr)?;
    Ok((StatusCode::CREATED, Json(g)))
}
/// `GET /v1/gateways/{id}`
pub async fn get_gateway(State(st): State<AppState>, t: TenantContext, Path(id): Path<String>) -> Result<Json<Gateway>, Problem> {
    Ok(Json(st.trunking.get_gateway(t.tenant_id, tid(&id)?).await.map_err(terr)?))
}
/// `PATCH /v1/gateways/{id}` — set the observed health (`ONLINE`/`OFFLINE`).
pub async fn patch_gateway(
    State(st): State<AppState>,
    admin: AdminContext,
    Path(id): Path<String>,
    Json(b): Json<PatchGateway>,
) -> Result<Json<Gateway>, Problem> {
    Ok(Json(st.trunking.set_gateway_health(admin.tenant_id, tid(&id)?, b.health).await.map_err(terr)?))
}
/// `DELETE /v1/gateways/{id}`
pub async fn delete_gateway(State(st): State<AppState>, admin: AdminContext, Path(id): Path<String>) -> Result<StatusCode, Problem> {
    st.trunking.delete_gateway(admin.tenant_id, tid(&id)?).await.map_err(terr)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- Trunks ---------------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CreateTrunk {
    pub carrier_id: String,
    pub channels_max: Option<i64>,
    #[serde(default)]
    pub codecs: Vec<String>,
    pub auth: Option<serde_json::Value>,
}

/// `GET /v1/trunks`
pub async fn list_trunks(
    State(st): State<AppState>,
    t: TenantContext,
    Query(p): Query<ListParams>,
) -> Result<Json<Page<Trunk>>, Problem> {
    let limit = p.limit.unwrap_or(50).clamp(1, 200);
    let page = st.trunking.list_trunks(t.tenant_id, limit, p.cursor).await.map_err(terr)?;
    Ok(Json(Page { items: page.items, next_cursor: page.next_cursor }))
}
/// `POST /v1/trunks`
pub async fn create_trunk(
    State(st): State<AppState>,
    admin: AdminContext,
    Json(b): Json<CreateTrunk>,
) -> Result<impl IntoResponse, Problem> {
    let carrier = tid(&b.carrier_id)?;
    let t = st
        .trunking
        .create_trunk(admin.tenant_id, carrier, b.channels_max, b.codecs, b.auth)
        .await
        .map_err(terr)?;
    Ok((StatusCode::CREATED, Json(t)))
}
/// `GET /v1/trunks/{id}`
pub async fn get_trunk(State(st): State<AppState>, t: TenantContext, Path(id): Path<String>) -> Result<Json<Trunk>, Problem> {
    Ok(Json(st.trunking.get_trunk(t.tenant_id, tid(&id)?).await.map_err(terr)?))
}
/// `DELETE /v1/trunks/{id}`
pub async fn delete_trunk(State(st): State<AppState>, admin: AdminContext, Path(id): Path<String>) -> Result<StatusCode, Problem> {
    st.trunking.delete_trunk(admin.tenant_id, tid(&id)?).await.map_err(terr)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- DIDs -----------------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CreateDid {
    pub e164: String,
    pub carrier_id: String,
    pub destination_ref: String,
}

/// `GET /v1/dids`
pub async fn list_dids(
    State(st): State<AppState>,
    t: TenantContext,
    Query(p): Query<ListParams>,
) -> Result<Json<Page<Did>>, Problem> {
    let limit = p.limit.unwrap_or(50).clamp(1, 200);
    let page = st.trunking.list_dids(t.tenant_id, limit, p.cursor).await.map_err(terr)?;
    Ok(Json(Page { items: page.items, next_cursor: page.next_cursor }))
}
/// `POST /v1/dids`
pub async fn create_did(
    State(st): State<AppState>,
    admin: AdminContext,
    Json(b): Json<CreateDid>,
) -> Result<impl IntoResponse, Problem> {
    let carrier = tid(&b.carrier_id)?;
    let d = st.trunking.create_did(admin.tenant_id, b.e164, carrier, b.destination_ref).await.map_err(terr)?;
    Ok((StatusCode::CREATED, Json(d)))
}
/// `GET /v1/dids/{id}`
pub async fn get_did(State(st): State<AppState>, t: TenantContext, Path(id): Path<String>) -> Result<Json<Did>, Problem> {
    Ok(Json(st.trunking.get_did(t.tenant_id, tid(&id)?).await.map_err(terr)?))
}
/// `DELETE /v1/dids/{id}`
pub async fn delete_did(State(st): State<AppState>, admin: AdminContext, Path(id): Path<String>) -> Result<StatusCode, Problem> {
    st.trunking.delete_did(admin.tenant_id, tid(&id)?).await.map_err(terr)?;
    Ok(StatusCode::NO_CONTENT)
}
