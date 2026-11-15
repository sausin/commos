//! `/v1/schedules` — business hours + holidays (open/closed routing).
//!
//! Reads are tenant-scoped; writes are admin-gated. Thin handlers over
//! [`ScheduleService`](crate::control::schedule::ScheduleService).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use commos_core::common::Uuid;
use commos_core::entities::schedule::{Holiday, Schedule, WeeklyRange};

use super::admin::AdminContext;
use super::auth::TenantContext;
use super::calls::ListParams;
use super::problem::Problem;
use crate::control::schedule::{ScheduleError, SchedulePatch, ScheduleService};
use crate::state::AppState;

fn tid(s: &str) -> Result<Uuid, Problem> {
    Uuid::parse(s).map_err(|_| Problem::bad_request("id is not a valid UUIDv7"))
}

fn serr(e: ScheduleError) -> Problem {
    match e {
        ScheduleError::NotFound => Problem::not_found("no such schedule"),
        ScheduleError::Invalid(m) => Problem::bad_request(m),
        ScheduleError::Store(crate::store::StoreError::VersionConflict { .. }) => {
            Problem::new(StatusCode::CONFLICT, "version_conflict", "concurrent modification")
        }
        ScheduleError::Store(e) => Problem::internal(e.to_string()),
    }
}

fn svc(st: &AppState) -> ScheduleService {
    ScheduleService::new(st.store.clone(), st.timezone.clone())
}

#[derive(Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

#[derive(Deserialize)]
pub struct CreateSchedule {
    pub name: String,
    pub timezone: Option<String>,
    #[serde(default)]
    pub weekly: Vec<WeeklyRange>,
    #[serde(default)]
    pub holidays: Vec<Holiday>,
}

#[derive(Deserialize)]
pub struct PatchSchedule {
    pub name: Option<String>,
    pub timezone: Option<String>,
    pub weekly: Option<Vec<WeeklyRange>>,
    pub holidays: Option<Vec<Holiday>>,
}

/// `GET /v1/schedules`
pub async fn list_schedules(
    State(st): State<AppState>,
    t: TenantContext,
    Query(p): Query<ListParams>,
) -> Result<Json<Page<Schedule>>, Problem> {
    let limit = p.limit.unwrap_or(50).clamp(1, 200);
    let page = svc(&st).list(t.tenant_id, limit, p.cursor).await.map_err(serr)?;
    Ok(Json(Page { items: page.items, next_cursor: page.next_cursor }))
}

/// `POST /v1/schedules`
pub async fn create_schedule(
    State(st): State<AppState>,
    admin: AdminContext,
    Json(b): Json<CreateSchedule>,
) -> Result<impl IntoResponse, Problem> {
    let s = svc(&st).create(admin.tenant_id, b.name, b.timezone, b.weekly, b.holidays).await.map_err(serr)?;
    Ok((StatusCode::CREATED, Json(s)))
}

/// `GET /v1/schedules/{id}`
pub async fn get_schedule(State(st): State<AppState>, t: TenantContext, Path(id): Path<String>) -> Result<Json<Schedule>, Problem> {
    Ok(Json(svc(&st).get(t.tenant_id, tid(&id)?).await.map_err(serr)?))
}

/// `PATCH /v1/schedules/{id}` — partial: any of name/timezone/weekly/holidays.
pub async fn patch_schedule(
    State(st): State<AppState>,
    admin: AdminContext,
    Path(id): Path<String>,
    Json(b): Json<PatchSchedule>,
) -> Result<Json<Schedule>, Problem> {
    let p = SchedulePatch { name: b.name, timezone: b.timezone, weekly: b.weekly, holidays: b.holidays };
    Ok(Json(svc(&st).patch(admin.tenant_id, tid(&id)?, p).await.map_err(serr)?))
}

/// `DELETE /v1/schedules/{id}`
pub async fn delete_schedule(State(st): State<AppState>, admin: AdminContext, Path(id): Path<String>) -> Result<StatusCode, Problem> {
    let sid = tid(&id)?;
    // Refuse while a DID still routes by this schedule (deleting would silently make it
    // always-open).
    let mut referencing: Vec<String> = Vec::new();
    let mut cursor = None;
    loop {
        let page = st
            .store
            .list_dids(admin.tenant_id, 200, cursor)
            .await
            .map_err(|e| Problem::internal(e.to_string()))?;
        referencing.extend(page.items.iter().filter(|d| d.schedule_id == Some(sid)).map(|d| d.e164.clone()));
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    if !referencing.is_empty() {
        return Err(Problem::new(
            StatusCode::CONFLICT,
            "schedule_in_use",
            format!("schedule is referenced by DID(s): {}", referencing.join(", ")),
        ));
    }
    svc(&st).delete(admin.tenant_id, sid).await.map_err(serr)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct ScheduleStatus {
    pub open: bool,
    pub now: String,
}

/// `GET /v1/schedules/:id/status` — is the schedule open right now?
pub async fn schedule_status(State(st): State<AppState>, t: TenantContext, Path(id): Path<String>) -> Result<Json<ScheduleStatus>, Problem> {
    let (open, now) = svc(&st).status(t.tenant_id, tid(&id)?, jiff::Timestamp::now()).await.map_err(serr)?;
    Ok(Json(ScheduleStatus { open, now }))
}
