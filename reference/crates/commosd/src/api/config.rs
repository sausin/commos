//! `/v1/config` — config-as-code through the API (CMOS-14-DEP-082).
//!
//! The whole point of config-as-code is that it goes through the *same* authenticated,
//! tenant-scoped API as everything else (CMOS-14-DEP-082) — not a side door. So a tenant's
//! `pbx.yaml` is just two operations on one resource:
//!
//! - `GET /v1/config` — export the tenant's live configuration as a deterministic
//!   `pbx.yaml` (`text/yaml`).
//! - `POST /v1/config` — parse a `pbx.yaml` body and apply it, returning an
//!   [`ImportSummary`] of what was created.
//!
//! The heavy lifting (deterministic projection, sorting, transactional apply) lives in
//! [`crate::control::configexport`]; these handlers are the thin request→command edge.
//! Inline-secret rejection is enforced at file load upstream; here we only parse and apply.

use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use axum::Json;

use crate::control::configexport::{self, ImportSummary, PbxConfig};
use crate::state::AppState;

use super::admin::AdminContext;
use super::auth::TenantContext;
use super::problem::Problem;

/// `GET /v1/config` — export this tenant's configuration as `pbx.yaml`.
///
/// Returns the YAML with `content-type: text/yaml`. The output is deterministic
/// (CMOS-14-DEP-081), so committing it to Git and re-exporting produces no spurious diff.
pub async fn export_config(
    State(st): State<AppState>,
    tenant: TenantContext,
) -> Result<impl IntoResponse, Problem> {
    let cfg = configexport::export(&st.store, tenant.tenant_id)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?;
    let yaml = configexport::to_yaml(&cfg).map_err(|e| Problem::internal(e.to_string()))?;
    Ok(([(header::CONTENT_TYPE, "text/yaml")], yaml))
}

/// `POST /v1/config` — apply a `pbx.yaml` body to this tenant.
///
/// The raw request body is the YAML document. A parse failure is a client error
/// (`400 Problem`); a valid document is applied transactionally and the count of created
/// rows is returned. Privileged: importing config rewrites the tenant's provisioning
/// directory, so it requires an admin (see [`AdminContext`]).
pub async fn import_config(
    State(st): State<AppState>,
    admin: AdminContext,
    body: String,
) -> Result<Json<ImportSummary>, Problem> {
    let cfg: PbxConfig = serde_yaml::from_str(&body)
        .map_err(|e| Problem::bad_request(format!("invalid pbx.yaml: {e}")))?;
    let summary = configexport::import(&st.store, admin.tenant_id, &cfg)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?;
    Ok(Json(summary))
}
