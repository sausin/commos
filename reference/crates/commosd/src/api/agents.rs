//! `/v1/agents` — contact-centre agent live-state (login + availability changes).
//!
//! Agent live-state is **ephemeral** in-memory runtime state served from the
//! [`crate::control::agents::AgentRegistry`], not the durable store — the same class as
//! device registrations. Every state change still emits the frozen `AgentStateChanged`
//! event. Problem-details errors and strict tenant scoping, matching the rest of the API.

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use commos_core::common::Uuid;

use super::auth::TenantContext;
use super::problem::Problem;
use crate::control::agents::Agent;
use crate::state::AppState;

#[derive(Serialize)]
pub struct AgentList {
    pub items: Vec<Agent>,
}

/// `GET /v1/agents` — the tenant's live agents.
pub async fn list_agents(
    State(st): State<AppState>,
    tenant: TenantContext,
) -> Json<AgentList> {
    Json(AgentList {
        items: st.agents.list(tenant.tenant_id),
    })
}

/// Body for `set_agent_state`: an agent logs in / changes availability. The client supplies
/// its Identity user id and the target state (e.g. `AVAILABLE` / `BUSY` / `OFFLINE`).
#[derive(Deserialize)]
pub struct SetAgentState {
    pub agent_user_id: String,
    pub state: String,
}

/// `POST /v1/agents` — set an agent's live state (upsert), emitting `AgentStateChanged`.
pub async fn set_agent_state(
    State(st): State<AppState>,
    tenant: TenantContext,
    Json(body): Json<SetAgentState>,
) -> Result<impl IntoResponse, Problem> {
    let agent_user_id = Uuid::parse(&body.agent_user_id)
        .map_err(|_| Problem::bad_request("agent_user_id is not a valid UUIDv7"))?;

    let agent = st
        .agents
        .set_state(tenant.tenant_id, agent_user_id, body.state)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?;

    Ok(Json(agent))
}

/// `GET /v1/agents/{id}` — one agent's live state.
pub async fn get_agent(
    State(st): State<AppState>,
    tenant: TenantContext,
    Path(id): Path<String>,
) -> Result<Json<Agent>, Problem> {
    let id = Uuid::parse(&id).map_err(|_| Problem::bad_request("id is not a valid UUIDv7"))?;
    match st.agents.get(tenant.tenant_id, id) {
        Some(agent) => Ok(Json(agent)),
        None => Err(Problem::not_found("no such agent")),
    }
}
