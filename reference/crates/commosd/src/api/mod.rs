//! API Gateway (Volume 3: AuthN/Z, request→command, the OpenAPI surface).
//!
//! Routes are mounted under `/v1` to match the frozen OpenAPI `servers` base
//! (`https://{host}/v1`). Operational signals and non-normative introspection live
//! outside `/v1` so they can never be confused for the versioned contract.

pub mod admin;
pub mod agents;
pub mod auth;
pub mod calls;
pub mod cdrs;
pub mod channels;
pub mod config;
pub mod dashboard;
pub mod directory;
pub mod health;
pub mod introspect;
pub mod messages;
pub mod onboarding;
pub mod presence;
pub mod problem;
pub mod provision;
pub mod queues;
pub mod registrations;
pub mod threads;
pub mod video_rooms;

use axum::routing::{get, post};
use axum::Router;
use tower_http::trace::TraceLayer;

use crate::state::AppState;

/// Build the application router.
pub fn router(state: AppState) -> Router {
    // The versioned contract surface (Volume 4). Every route here is bearer-authenticated
    // and tenant-scoped via the `TenantContext` extractor.
    // The frozen API expresses actions as `/calls/{id}:hold`. axum's matcher can't bind a
    // path param followed by a literal `:action` in one segment, so actions are mounted as
    // sub-paths (`/calls/:id/hold`) for now — same command, same event, same capability.
    let v1 = Router::new()
        .route("/calls", get(calls::list_calls).post(calls::create_calls))
        .route("/calls/:id", get(calls::get_call).patch(calls::patch_call))
        .route("/calls/:id/hold", post(calls::hold_call))
        .route("/calls/:id/resume", post(calls::resume_call))
        .route("/calls/:id/hangup", post(calls::hangup_call))
        .route("/calls/:id/transfer", post(calls::transfer_call))
        // Messaging workload — peers of Call on the same substrate (voice is one workload).
        .route("/channels", get(channels::list_channels).post(channels::create_channel))
        .route("/channels/:id", get(channels::get_channel))
        .route("/threads", get(threads::list_threads).post(threads::create_thread))
        .route("/threads/:id", get(threads::get_thread))
        .route("/messages", get(messages::list_messages).post(messages::create_message))
        .route("/messages/:id", get(messages::get_message))
        // Real-time workloads — video rooms and presence, same substrate again.
        .route("/video-rooms", get(video_rooms::list_video_rooms).post(video_rooms::create_video_room))
        .route("/video-rooms/:id", get(video_rooms::get_video_room))
        .route("/presence", get(presence::list_presence).post(presence::set_presence))
        .route("/presence/:id", get(presence::get_presence))
        // Device registrations — ephemeral in-memory state (not the durable store).
        .route("/registrations", get(registrations::list_registrations).post(registrations::create_registration))
        .route("/registrations/:id", get(registrations::get_registration).delete(registrations::delete_registration))
        // Billing — CDRs are produced by the platform on call end (read-only).
        .route("/cdrs", get(cdrs::list_cdrs))
        .route("/cdrs/:id", get(cdrs::get_cdr))
        // Contact-centre — call queues, agents, and enqueue (ACD).
        .route("/queues", get(queues::list_queues).post(queues::create_queue))
        .route("/queues/:id", get(queues::get_queue))
        .route("/queues/:id/enqueue", post(queues::enqueue_call))
        .route("/agents", get(agents::list_agents).post(agents::set_agent_state))
        .route("/agents/:id", get(agents::get_agent))
        // Admin onboarding wizard — auto-detected suggestions + one-click apply.
        .route("/onboarding/environments", get(onboarding::list_environments))
        .route("/onboarding/suggest", get(onboarding::suggest))
        .route("/onboarding/apply", post(onboarding::apply))
        // Config-as-code (pbx.yaml) export/import — CMOS-14-DEP-080/082.
        .route("/config", get(config::export_config).post(config::import_config))
        // Provisioning directory — people, phones, extensions, routes. Reads are
        // tenant-scoped; writes (create/patch/lifecycle/delete) require an admin.
        .route("/users", get(directory::list_users).post(directory::create_user))
        .route(
            "/users/:id",
            get(directory::get_user).patch(directory::patch_user).delete(directory::delete_user),
        )
        .route("/users/:id/activate", post(directory::activate_user))
        .route("/users/:id/deactivate", post(directory::deactivate_user))
        .route("/users/:id/suspend", post(directory::suspend_user))
        .route("/extensions", get(directory::list_extensions).post(directory::create_extension))
        .route(
            "/extensions/:id",
            get(directory::get_extension)
                .patch(directory::patch_extension)
                .delete(directory::delete_extension),
        )
        .route("/devices", get(directory::list_devices).post(directory::create_device))
        .route(
            "/devices/:id",
            get(directory::get_device).patch(directory::patch_device).delete(directory::delete_device),
        )
        .route("/devices/:id/approve", post(directory::approve_device))
        .route("/devices/:id/reject", post(directory::reject_device))
        .route("/devices/:id/retire", post(directory::retire_device))
        .route("/devices/:id/replace", post(directory::replace_device))
        .route("/routes", get(directory::list_routes).post(directory::create_route))
        .route(
            "/routes/:id",
            get(directory::get_route).patch(directory::patch_route).delete(directory::delete_route),
        );

    Router::new()
        .nest("/v1", v1)
        // Admin authentication — login/logout/whoami gate the privileged setup operations
        // (onboarding apply, config import). Unauthenticated login; the rest need an admin.
        .route("/admin/login", post(admin::login::<AppState>))
        .route("/admin/logout", post(admin::logout::<AppState>))
        .route("/admin/whoami", get(admin::whoami))
        // Operational signals (Volume 15) — unauthenticated, outside the contract surface.
        .route("/livez", get(health::livez))
        .route("/readyz", get(health::readyz))
        .route("/info", get(health::info))
        // Live operations dashboard + setup wizard (self-contained HTML, unauthenticated).
        .route("/dashboard", get(dashboard::dashboard))
        .route("/onboarding", get(onboarding::wizard))
        // Phone auto-provisioning (DHCP option 66 target; unauthenticated).
        .route("/provision/:file", get(provision::provision))
        // Non-normative introspection for bring-up/testing.
        .route("/_introspect/events", get(introspect::recent_events))
        .route("/_introspect/events/stream", get(introspect::stream_events))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
