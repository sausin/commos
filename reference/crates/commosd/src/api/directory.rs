//! `/v1/{users,extensions,devices}` — read access to the provisioning directory (the
//! people, numbers, and phones onboarding creates). List + get, tenant-scoped, mirroring
//! the other resources.

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Serialize;

use commos_core::common::Uuid;
use commos_core::entities::{device::Device, extension::Extension, user::User};

use super::auth::TenantContext;
use super::calls::ListParams;
use super::problem::Problem;
use crate::state::AppState;

#[derive(Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

fn id(s: &str) -> Result<Uuid, Problem> {
    Uuid::parse(s).map_err(|_| Problem::bad_request("id is not a valid UUIDv7"))
}
fn limit(p: &ListParams) -> usize {
    p.limit.unwrap_or(50).clamp(1, 200)
}
fn internal(e: impl std::fmt::Display) -> Problem {
    Problem::internal(e.to_string())
}

/// `GET /v1/users`
pub async fn list_users(
    State(st): State<AppState>,
    t: TenantContext,
    Query(p): Query<ListParams>,
) -> Result<Json<Page<User>>, Problem> {
    let page = st.store.list_users(t.tenant_id, limit(&p), p.cursor).await.map_err(internal)?;
    Ok(Json(Page { items: page.items, next_cursor: page.next_cursor }))
}
/// `GET /v1/users/{id}`
pub async fn get_user(
    State(st): State<AppState>,
    t: TenantContext,
    Path(i): Path<String>,
) -> Result<Json<User>, Problem> {
    match st.store.get_user(t.tenant_id, id(&i)?).await.map_err(internal)? {
        Some(u) => Ok(Json(u)),
        None => Err(Problem::not_found("no such user")),
    }
}

/// `GET /v1/extensions`
pub async fn list_extensions(
    State(st): State<AppState>,
    t: TenantContext,
    Query(p): Query<ListParams>,
) -> Result<Json<Page<Extension>>, Problem> {
    let page = st.store.list_extensions(t.tenant_id, limit(&p), p.cursor).await.map_err(internal)?;
    Ok(Json(Page { items: page.items, next_cursor: page.next_cursor }))
}
/// `GET /v1/extensions/{id}`
pub async fn get_extension(
    State(st): State<AppState>,
    t: TenantContext,
    Path(i): Path<String>,
) -> Result<Json<Extension>, Problem> {
    match st.store.get_extension(t.tenant_id, id(&i)?).await.map_err(internal)? {
        Some(e) => Ok(Json(e)),
        None => Err(Problem::not_found("no such extension")),
    }
}

/// `GET /v1/devices`
pub async fn list_devices(
    State(st): State<AppState>,
    t: TenantContext,
    Query(p): Query<ListParams>,
) -> Result<Json<Page<Device>>, Problem> {
    let page = st.store.list_devices(t.tenant_id, limit(&p), p.cursor).await.map_err(internal)?;
    Ok(Json(Page { items: page.items, next_cursor: page.next_cursor }))
}
/// `GET /v1/devices/{id}`
pub async fn get_device(
    State(st): State<AppState>,
    t: TenantContext,
    Path(i): Path<String>,
) -> Result<Json<Device>, Problem> {
    match st.store.get_device(t.tenant_id, id(&i)?).await.map_err(internal)? {
        Some(d) => Ok(Json(d)),
        None => Err(Problem::not_found("no such device")),
    }
}
