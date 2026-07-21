//! `/v1/{users,extensions,devices}` — read access to the provisioning directory (the
//! people, numbers, and phones onboarding creates). List + get, tenant-scoped, mirroring
//! the other resources.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use commos_core::common::Uuid;
use commos_core::entities::{device::Device, extension::Extension, route::Route, user::User};

use super::admin::AdminContext;
use super::auth::TenantContext;
use super::calls::ListParams;
use super::problem::Problem;
use crate::control::provisioning::{
    DevicePatch, ExtensionPatch, NewDevice, NewExtension, NewUser, ProvisioningError, UserPatch,
};
use crate::state::AppState;

/// Map a provisioning-service error onto the right Problem status.
fn prob(e: ProvisioningError) -> Problem {
    match e {
        ProvisioningError::NotFound => Problem::not_found("no such entity"),
        ProvisioningError::IllegalState(m) => {
            Problem::new(StatusCode::CONFLICT, "illegal_state", m)
        }
        ProvisioningError::Invalid(m) => Problem::bad_request(m),
        ProvisioningError::Store(crate::store::StoreError::VersionConflict { .. }) => {
            Problem::new(StatusCode::CONFLICT, "version_conflict", "concurrent modification")
        }
        ProvisioningError::Store(e) => Problem::internal(e.to_string()),
    }
}

/// Parse an optional string UUID field from a request body.
fn opt_id(s: Option<String>) -> Result<Option<Uuid>, Problem> {
    match s {
        Some(v) => Uuid::parse(&v)
            .map(Some)
            .map_err(|_| Problem::bad_request("assigned_user_id is not a valid UUIDv7")),
        None => Ok(None),
    }
}

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

/// `GET /v1/routes` — the routing rules that resolve dialled numbers to destinations.
pub async fn list_routes(
    State(st): State<AppState>,
    t: TenantContext,
    Query(p): Query<ListParams>,
) -> Result<Json<Page<Route>>, Problem> {
    let page = st.store.list_routes(t.tenant_id, limit(&p), p.cursor).await.map_err(internal)?;
    Ok(Json(Page { items: page.items, next_cursor: page.next_cursor }))
}
/// `GET /v1/routes/{id}`
pub async fn get_route(
    State(st): State<AppState>,
    t: TenantContext,
    Path(i): Path<String>,
) -> Result<Json<Route>, Problem> {
    match st.store.get_route(t.tenant_id, id(&i)?).await.map_err(internal)? {
        Some(r) => Ok(Json(r)),
        None => Err(Problem::not_found("no such route")),
    }
}

// --- Write path (privileged: AdminContext) -----------------------------------------------
//
// Adding/editing/removing people, phones, extensions, and routes is comms *management* —
// the operator's core job. These require an admin (dev mode falls back to a tenant bearer,
// so local setup stays zero-config), and each lifecycle change emits its canonical event
// through the outbox.

// ----- Users -----

#[derive(Deserialize)]
pub struct CreateUserBody {
    pub display_name: String,
    pub email: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
}
/// `POST /v1/users`
pub async fn create_user(
    State(st): State<AppState>,
    admin: AdminContext,
    Json(b): Json<CreateUserBody>,
) -> Result<impl IntoResponse, Problem> {
    let u = st
        .provisioning
        .create_user(
            admin.tenant_id,
            NewUser { display_name: b.display_name, email: b.email, capabilities: b.capabilities },
        )
        .await
        .map_err(prob)?;
    Ok((StatusCode::CREATED, Json(u)))
}

#[derive(Deserialize)]
pub struct PatchUserBody {
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub capabilities: Option<Vec<String>>,
}
/// `PATCH /v1/users/{id}`
pub async fn patch_user(
    State(st): State<AppState>,
    admin: AdminContext,
    Path(i): Path<String>,
    Json(b): Json<PatchUserBody>,
) -> Result<Json<User>, Problem> {
    let u = st
        .provisioning
        .update_user(
            admin.tenant_id,
            id(&i)?,
            UserPatch { display_name: b.display_name, email: b.email, capabilities: b.capabilities },
        )
        .await
        .map_err(prob)?;
    Ok(Json(u))
}

/// `POST /v1/users/{id}/activate`
pub async fn activate_user(
    State(st): State<AppState>,
    admin: AdminContext,
    Path(i): Path<String>,
) -> Result<Json<User>, Problem> {
    Ok(Json(st.provisioning.activate_user(admin.tenant_id, id(&i)?).await.map_err(prob)?))
}

/// `POST /v1/users/{id}/deactivate` — also the soft-delete for a person.
pub async fn deactivate_user(
    State(st): State<AppState>,
    admin: AdminContext,
    Path(i): Path<String>,
) -> Result<Json<User>, Problem> {
    Ok(Json(st.provisioning.deactivate_user(admin.tenant_id, id(&i)?).await.map_err(prob)?))
}

#[derive(Deserialize, Default)]
pub struct ReasonBody {
    pub reason: Option<String>,
}
/// `POST /v1/users/{id}/suspend`
pub async fn suspend_user(
    State(st): State<AppState>,
    admin: AdminContext,
    Path(i): Path<String>,
    body: Option<Json<ReasonBody>>,
) -> Result<Json<User>, Problem> {
    let reason = body.and_then(|Json(b)| b.reason);
    Ok(Json(st.provisioning.suspend_user(admin.tenant_id, id(&i)?, reason).await.map_err(prob)?))
}

/// `DELETE /v1/users/{id}` — soft-delete (deactivate); deletion is a state transition.
pub async fn delete_user(
    State(st): State<AppState>,
    admin: AdminContext,
    Path(i): Path<String>,
) -> Result<Json<User>, Problem> {
    Ok(Json(st.provisioning.deactivate_user(admin.tenant_id, id(&i)?).await.map_err(prob)?))
}

// ----- Devices -----

#[derive(Deserialize)]
pub struct CreateDeviceBody {
    pub vendor_key: String,
    pub model: String,
    pub mac: Option<String>,
    pub assigned_user_id: Option<String>,
}
/// `POST /v1/devices`
pub async fn create_device(
    State(st): State<AppState>,
    admin: AdminContext,
    Json(b): Json<CreateDeviceBody>,
) -> Result<impl IntoResponse, Problem> {
    let d = st
        .provisioning
        .create_device(
            admin.tenant_id,
            NewDevice {
                vendor_key: b.vendor_key,
                model: b.model,
                mac: b.mac,
                assigned_user_id: opt_id(b.assigned_user_id)?,
            },
        )
        .await
        .map_err(prob)?;
    Ok((StatusCode::CREATED, Json(d)))
}

#[derive(Deserialize)]
pub struct PatchDeviceBody {
    pub model: Option<String>,
    pub mac: Option<String>,
    pub assigned_user_id: Option<String>,
    pub firmware: Option<String>,
}
/// `PATCH /v1/devices/{id}`
pub async fn patch_device(
    State(st): State<AppState>,
    admin: AdminContext,
    Path(i): Path<String>,
    Json(b): Json<PatchDeviceBody>,
) -> Result<Json<Device>, Problem> {
    let d = st
        .provisioning
        .update_device(
            admin.tenant_id,
            id(&i)?,
            DevicePatch {
                model: b.model,
                mac: b.mac,
                assigned_user_id: opt_id(b.assigned_user_id)?,
                firmware: b.firmware,
            },
        )
        .await
        .map_err(prob)?;
    Ok(Json(d))
}

/// `POST /v1/devices/{id}/approve`
pub async fn approve_device(
    State(st): State<AppState>,
    admin: AdminContext,
    Path(i): Path<String>,
) -> Result<Json<Device>, Problem> {
    Ok(Json(st.provisioning.approve_device(admin.tenant_id, id(&i)?).await.map_err(prob)?))
}

/// `POST /v1/devices/{id}/reject`
pub async fn reject_device(
    State(st): State<AppState>,
    admin: AdminContext,
    Path(i): Path<String>,
    body: Option<Json<ReasonBody>>,
) -> Result<Json<Device>, Problem> {
    let reason = body.and_then(|Json(b)| b.reason);
    Ok(Json(st.provisioning.reject_device(admin.tenant_id, id(&i)?, reason).await.map_err(prob)?))
}

/// `POST /v1/devices/{id}/retire` — also the soft-delete for a phone.
pub async fn retire_device(
    State(st): State<AppState>,
    admin: AdminContext,
    Path(i): Path<String>,
) -> Result<Json<Device>, Problem> {
    Ok(Json(st.provisioning.retire_device(admin.tenant_id, id(&i)?).await.map_err(prob)?))
}

#[derive(Serialize)]
pub struct ReplaceOutcome {
    pub retiring: Device,
    pub replacement: Device,
}
/// `POST /v1/devices/{id}/replace` — one-click swap: old → REPLACING, mint a replacement.
pub async fn replace_device(
    State(st): State<AppState>,
    admin: AdminContext,
    Path(i): Path<String>,
) -> Result<Json<ReplaceOutcome>, Problem> {
    let (retiring, replacement) =
        st.provisioning.replace_device(admin.tenant_id, id(&i)?).await.map_err(prob)?;
    Ok(Json(ReplaceOutcome { retiring, replacement }))
}

/// `DELETE /v1/devices/{id}` — soft-delete (retire).
pub async fn delete_device(
    State(st): State<AppState>,
    admin: AdminContext,
    Path(i): Path<String>,
) -> Result<Json<Device>, Problem> {
    Ok(Json(st.provisioning.retire_device(admin.tenant_id, id(&i)?).await.map_err(prob)?))
}

// ----- Extensions -----

#[derive(Deserialize)]
pub struct CreateExtensionBody {
    pub number: String,
    pub destination_ref: String,
    pub label: Option<String>,
}
/// `POST /v1/extensions`
pub async fn create_extension(
    State(st): State<AppState>,
    admin: AdminContext,
    Json(b): Json<CreateExtensionBody>,
) -> Result<impl IntoResponse, Problem> {
    let e = st
        .provisioning
        .create_extension(
            admin.tenant_id,
            NewExtension { number: b.number, destination_ref: b.destination_ref, label: b.label },
        )
        .await
        .map_err(prob)?;
    Ok((StatusCode::CREATED, Json(e)))
}

#[derive(Deserialize)]
pub struct PatchExtensionBody {
    pub number: Option<String>,
    pub label: Option<String>,
    pub destination_ref: Option<String>,
}
/// `PATCH /v1/extensions/{id}`
pub async fn patch_extension(
    State(st): State<AppState>,
    admin: AdminContext,
    Path(i): Path<String>,
    Json(b): Json<PatchExtensionBody>,
) -> Result<Json<Extension>, Problem> {
    let e = st
        .provisioning
        .update_extension(
            admin.tenant_id,
            id(&i)?,
            ExtensionPatch { number: b.number, label: b.label, destination_ref: b.destination_ref },
        )
        .await
        .map_err(prob)?;
    Ok(Json(e))
}

/// `DELETE /v1/extensions/{id}` — config hard delete.
pub async fn delete_extension(
    State(st): State<AppState>,
    admin: AdminContext,
    Path(i): Path<String>,
) -> Result<StatusCode, Problem> {
    st.provisioning.delete_extension(admin.tenant_id, id(&i)?).await.map_err(prob)?;
    Ok(StatusCode::NO_CONTENT)
}

// ----- Routes -----

#[derive(Deserialize)]
pub struct CreateRouteBody {
    pub destination_ref: String,
    pub priority: Option<i64>,
}
/// `POST /v1/routes`
pub async fn create_route(
    State(st): State<AppState>,
    admin: AdminContext,
    Json(b): Json<CreateRouteBody>,
) -> Result<impl IntoResponse, Problem> {
    let r = st
        .provisioning
        .create_route(admin.tenant_id, b.destination_ref, b.priority)
        .await
        .map_err(prob)?;
    Ok((StatusCode::CREATED, Json(r)))
}

#[derive(Deserialize)]
pub struct PatchRouteBody {
    pub destination_ref: Option<String>,
    pub priority: Option<i64>,
}
/// `PATCH /v1/routes/{id}`
pub async fn patch_route(
    State(st): State<AppState>,
    admin: AdminContext,
    Path(i): Path<String>,
    Json(b): Json<PatchRouteBody>,
) -> Result<Json<Route>, Problem> {
    let r = st
        .provisioning
        .update_route(admin.tenant_id, id(&i)?, b.destination_ref, b.priority)
        .await
        .map_err(prob)?;
    Ok(Json(r))
}

/// `DELETE /v1/routes/{id}` — config hard delete.
pub async fn delete_route(
    State(st): State<AppState>,
    admin: AdminContext,
    Path(i): Path<String>,
) -> Result<StatusCode, Problem> {
    st.provisioning.delete_route(admin.tenant_id, id(&i)?).await.map_err(prob)?;
    Ok(StatusCode::NO_CONTENT)
}
