//! `/v1/registrations` — SIP-style device registrations.
//!
//! These bindings are **ephemeral, in-memory only** (CMOS-14-DEP-021): they are served
//! from [`crate::control::registrations::RegistrationRegistry`], never the durable store,
//! so a high-churn re-register storm produces zero disk writes. Mirrors the handler style
//! of `api/calls.rs` — bearer auth via `TenantContext`, Problem-details errors, strict
//! tenant scoping — but with no cursor pagination (the working set is small) and no store.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use commos_core::common::Uuid;

use super::auth::TenantContext;
use super::problem::Problem;
use crate::control::registrations::Registration;
use crate::state::AppState;

/// Default registration lifetime when a client omits `expires_secs` (SIP's customary hour).
const DEFAULT_EXPIRES_SECS: u64 = 3600;

/// Body for `create_registration`. `id`, `expires_at`, and `created_at` are server-managed.
#[derive(Deserialize)]
pub struct CreateRegistration {
    /// Address-of-record, e.g. `sip:100@example.com`.
    pub aor: String,
    /// Contact URI where the AOR is reachable, e.g. `sip:100@192.168.1.5:5060`.
    pub contact: String,
    pub user_agent: Option<String>,
    pub expires_secs: Option<u64>,
}

#[derive(Serialize)]
pub struct RegistrationList {
    pub items: Vec<Registration>,
}

/// `POST /v1/registrations` — register (or refresh) a device binding.
pub async fn create_registration(
    State(st): State<AppState>,
    tenant: TenantContext,
    Json(body): Json<CreateRegistration>,
) -> Result<impl IntoResponse, Problem> {
    if body.aor.trim().is_empty() || body.contact.trim().is_empty() {
        return Err(Problem::bad_request("aor and contact are required"));
    }
    let expires_secs = body.expires_secs.unwrap_or(DEFAULT_EXPIRES_SECS);

    let registration = st.registrations.register(
        tenant.tenant_id,
        body.aor,
        body.contact,
        body.user_agent,
        expires_secs,
    );

    Ok((StatusCode::CREATED, Json(registration)))
}

/// `GET /v1/registrations`
pub async fn list_registrations(
    State(st): State<AppState>,
    tenant: TenantContext,
) -> Json<RegistrationList> {
    Json(RegistrationList {
        items: st.registrations.list(tenant.tenant_id),
    })
}

/// `GET /v1/registrations/{id}`
pub async fn get_registration(
    State(st): State<AppState>,
    tenant: TenantContext,
    Path(id): Path<String>,
) -> Result<Json<Registration>, Problem> {
    let id = Uuid::parse(&id).map_err(|_| Problem::bad_request("id is not a valid UUIDv7"))?;
    match st.registrations.get(tenant.tenant_id, id) {
        Some(registration) => Ok(Json(registration)),
        None => Err(Problem::not_found("no such registration")),
    }
}

/// `DELETE /v1/registrations/{id}` — unregister (SIP `REGISTER` with `Expires: 0`).
pub async fn delete_registration(
    State(st): State<AppState>,
    tenant: TenantContext,
    Path(id): Path<String>,
) -> Result<StatusCode, Problem> {
    let id = Uuid::parse(&id).map_err(|_| Problem::bad_request("id is not a valid UUIDv7"))?;
    if st.registrations.unregister(tenant.tenant_id, id) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Problem::not_found("no such registration"))
    }
}
