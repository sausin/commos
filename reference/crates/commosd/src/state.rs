//! Shared application state — the wiring that every request handler sees.
//!
//! Handlers are stateless (CMOS-03-ARCH-010); this holds only shared, cloneable handles
//! to the store, the control services, and the bus.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::api::admin::{AdminAuth, HasAdminAuth};
use crate::api::auth::{AuthConfig, HasAuthConfig};
use crate::bus::EventBus;
use crate::control::agents::AgentRegistry;
use crate::control::messaging::MessagingService;
use crate::control::queue::QueueService;
use crate::control::realtime::RealtimeService;
use crate::control::registrations::RegistrationRegistry;
use crate::control::routing::Routing;
use crate::introspect::RecentEvents;
use crate::store::Store;

/// Cheap-to-clone application state (all fields are `Arc`/handles).
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn Store>,
    pub routing: Routing,
    pub messaging: MessagingService,
    pub realtime: RealtimeService,
    pub queues: QueueService,
    /// Ephemeral in-memory contact-centre agent states (like registrations, not durable).
    pub agents: AgentRegistry,
    /// Ephemeral in-memory device registrations (deliberately NOT the durable store —
    /// keeps write volume near zero for SD-card longevity; CMOS-14-DEP-021).
    pub registrations: RegistrationRegistry,
    /// Bearer-auth verifier config (JWT secret + dev-token flag).
    pub auth: AuthConfig,
    /// Admin authentication — gates privileged setup/config operations. Dev mode (no admin
    /// password) falls back to tenant-bearer auth so local setup needs zero config.
    pub admin: AdminAuth,
    /// SIP registrar address advertised to phones (for auto-provisioning configs).
    pub media_ip: std::net::IpAddr,
    pub sip_port: u16,
    pub bus: EventBus,
    pub recent: RecentEvents,
    /// Readiness flag — a node reports not-ready before it can serve and again while
    /// draining (CMOS-14-DEP-033), gating load-balancer membership.
    ready: Arc<AtomicBool>,
    pub started_at: commos_core::common::Timestamp,
}

impl AppState {
    pub fn new(
        store: Arc<dyn Store>,
        routing: Routing,
        messaging: MessagingService,
        realtime: RealtimeService,
        queues: QueueService,
        agents: AgentRegistry,
        registrations: RegistrationRegistry,
        auth: AuthConfig,
        admin: AdminAuth,
        media_ip: std::net::IpAddr,
        sip_port: u16,
        bus: EventBus,
        recent: RecentEvents,
    ) -> Self {
        AppState {
            store,
            routing,
            messaging,
            realtime,
            queues,
            agents,
            registrations,
            auth,
            admin,
            media_ip,
            sip_port,
            bus,
            recent,
            ready: Arc::new(AtomicBool::new(false)),
            started_at: commos_core::common::Timestamp::now(),
        }
    }

    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::SeqCst);
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }
}

/// Lets the `TenantContext` extractor reach the auth verifier config without `api::auth`
/// depending on the concrete state type.
impl HasAuthConfig for AppState {
    fn auth_config(&self) -> &AuthConfig {
        &self.auth
    }
}

/// Lets the `AdminContext` extractor and the admin handlers reach the admin-auth state
/// without `api::admin` depending on the concrete state type.
impl HasAdminAuth for AppState {
    fn admin_auth(&self) -> &AdminAuth {
        &self.admin
    }
}
