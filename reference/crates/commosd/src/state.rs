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
use crate::control::callflow::CallFlowService;
use crate::control::ivr::IvrService;
use crate::control::messaging::MessagingService;
use crate::control::objects::ObjectService;
use crate::control::provisioning::Provisioning;
use crate::control::queue::QueueService;
use crate::control::realtime::RealtimeService;
use crate::control::recordings::RecordingService;
use crate::control::registrations::RegistrationRegistry;
use crate::control::routing::Routing;
use crate::control::trunking::TrunkingService;
use crate::control::voicemail::VoicemailService;
use crate::control::webhooks::WebhookService;
use crate::introspect::RecentEvents;
use crate::metrics::Metrics;
use crate::store::Store;

/// Cheap-to-clone application state (all fields are `Arc`/handles).
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn Store>,
    pub routing: Routing,
    pub messaging: MessagingService,
    pub realtime: RealtimeService,
    pub queues: QueueService,
    /// Routing programs — versioned CallFlows (publish/rollback) and IVR menu nodes.
    pub call_flows: CallFlowService,
    pub ivrs: IvrService,
    /// PSTN / SIP trunking — carriers, gateways, trunks (outbound), and inbound DIDs.
    pub trunking: TrunkingService,
    /// Directory write path — people, phones, extensions, routes and their lifecycle.
    pub provisioning: Provisioning,
    /// Outbound webhook subscriptions (register/list/delete).
    pub webhooks: WebhookService,
    /// Object storage — recordings, voicemail, exports, diagnostics (blob + metadata).
    pub objects: ObjectService,
    /// Call recordings — list/fetch captured audio and its metadata (Volume 7).
    pub recordings: RecordingService,
    /// Voicemails — list/fetch/mark-read messages left on no-answer (Volume 7).
    pub voicemails: VoicemailService,
    /// Prometheus metrics registry (scraped at `/metrics`).
    pub metrics: Metrics,
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
    /// NTP time server written into provisioned phone configs. `None` → phones are pointed at
    /// the CommOS host (`media_ip`); set to a dedicated internal NTP appliance to override.
    pub ntp_server: Option<String>,
    /// Timezone (POSIX TZ string) written into provisioned phone configs so handsets show the
    /// correct local time. `None` → no timezone directive is emitted.
    pub timezone: Option<String>,
    /// Human-readable description of the system of record (e.g. the SQLite file path), surfaced
    /// to the operator at the end of onboarding so they know where their configuration lives.
    pub storage_location: String,
    pub bus: EventBus,
    pub recent: RecentEvents,
    /// Readiness flag — a node reports not-ready before it can serve and again while
    /// draining (CMOS-14-DEP-033), gating load-balancer membership.
    ready: Arc<AtomicBool>,
    pub started_at: commos_core::common::Timestamp,
}

impl AppState {
    // A wiring constructor called exactly once from `main`; each argument is a distinct
    // shared handle, so bundling them into a struct would add indirection without clarity.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<dyn Store>,
        routing: Routing,
        messaging: MessagingService,
        realtime: RealtimeService,
        queues: QueueService,
        call_flows: CallFlowService,
        ivrs: IvrService,
        trunking: TrunkingService,
        provisioning: Provisioning,
        webhooks: WebhookService,
        objects: ObjectService,
        recordings: RecordingService,
        voicemails: VoicemailService,
        metrics: Metrics,
        agents: AgentRegistry,
        registrations: RegistrationRegistry,
        auth: AuthConfig,
        admin: AdminAuth,
        media_ip: std::net::IpAddr,
        sip_port: u16,
        ntp_server: Option<String>,
        timezone: Option<String>,
        storage_location: String,
        bus: EventBus,
        recent: RecentEvents,
    ) -> Self {
        AppState {
            store,
            routing,
            messaging,
            realtime,
            queues,
            call_flows,
            ivrs,
            trunking,
            provisioning,
            webhooks,
            objects,
            recordings,
            voicemails,
            metrics,
            agents,
            registrations,
            auth,
            admin,
            media_ip,
            sip_port,
            ntp_server,
            timezone,
            storage_location,
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
