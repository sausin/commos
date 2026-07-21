//! `/v1/webhooks` — register and manage outbound webhook subscriptions (Volume 5 §EVT-014).
//!
//! A webhook subscribes an HTTP endpoint to a set of canonical event types; every matching
//! event the platform relays is POSTed to it, HMAC-signed when a `secret_ref` is set. Writes
//! require an admin; reads are tenant-scoped.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use commos_core::common::Uuid;
use commos_core::entities::webhook::Webhook;

use super::admin::AdminContext;
use super::auth::TenantContext;
use super::calls::ListParams;
use super::problem::Problem;
use crate::state::AppState;

fn pid(s: &str) -> Result<Uuid, Problem> {
    Uuid::parse(s).map_err(|_| Problem::bad_request("id is not a valid UUIDv7"))
}
fn oops(e: impl std::fmt::Display) -> Problem {
    Problem::internal(e.to_string())
}

#[derive(Serialize)]
pub struct WebhookPage {
    pub items: Vec<Webhook>,
    pub next_cursor: Option<String>,
}

/// `GET /v1/webhooks`
pub async fn list_webhooks(
    State(st): State<AppState>,
    t: TenantContext,
    Query(p): Query<ListParams>,
) -> Result<Json<WebhookPage>, Problem> {
    let limit = p.limit.unwrap_or(50).clamp(1, 200);
    let page = st.webhooks.list(t.tenant_id, limit, p.cursor).await.map_err(oops)?;
    Ok(Json(WebhookPage { items: page.items, next_cursor: page.next_cursor }))
}

/// `GET /v1/webhooks/{id}`
pub async fn get_webhook(
    State(st): State<AppState>,
    t: TenantContext,
    Path(id): Path<String>,
) -> Result<Json<Webhook>, Problem> {
    match st.store.get_webhook(t.tenant_id, pid(&id)?).await.map_err(oops)? {
        Some(w) => Ok(Json(w)),
        None => Err(Problem::not_found("no such webhook")),
    }
}

#[derive(Deserialize)]
pub struct CreateWebhookBody {
    pub url: String,
    /// Canonical event types to deliver; `["*"]` for all.
    #[serde(default)]
    pub event_types: Vec<String>,
    /// Optional signing-secret reference (`env://…` / `file://…`), never an inline secret.
    pub secret_ref: Option<String>,
}

/// `POST /v1/webhooks` — register a subscription (admin).
pub async fn create_webhook(
    State(st): State<AppState>,
    _admin: AdminContext,
    Json(b): Json<CreateWebhookBody>,
) -> Result<impl IntoResponse, Problem> {
    if !(b.url.starts_with("http://") || b.url.starts_with("https://")) {
        return Err(Problem::bad_request("url must be http:// or https://"));
    }
    let event_types = if b.event_types.is_empty() { vec!["*".to_string()] } else { b.event_types };
    let w = st
        .webhooks
        .create(_admin.tenant_id, b.url, event_types, b.secret_ref)
        .await
        .map_err(oops)?;
    Ok((StatusCode::CREATED, Json(w)))
}

/// `DELETE /v1/webhooks/{id}` — remove a subscription (admin).
pub async fn delete_webhook(
    State(st): State<AppState>,
    admin: AdminContext,
    Path(id): Path<String>,
) -> Result<StatusCode, Problem> {
    if st.webhooks.delete(admin.tenant_id, pid(&id)?).await.map_err(oops)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Problem::not_found("no such webhook"))
    }
}
