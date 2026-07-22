//! System of record + transactional outbox (CMOS-00-ENG-007; CMOS-03-ARCH-030;
//! CMOS-05-EVT-010).
//!
//! **The guarantee:** every observable state change is written to the outbox *in the same
//! transaction* as the state change. Either both land or neither does; the relay then
//! delivers from the outbox at-least-once. That is what makes "no state change without its
//! event" true even across a crash.
//!
//! [`Store`] is the abstraction. Two bindings implement it identically:
//! - [`mem::MemStore`] — zero-dependency, lets the single binary boot with no PostgreSQL
//!   (CMOS-14-DEP-021).
//! - [`postgres::PgStore`] — the durable system of record: a real `BEGIN … COMMIT` and a
//!   relay that claims rows with `FOR UPDATE SKIP LOCKED` (CMOS-14-DEP-020).
//!
//! Swapping bindings changes no caller: Routing, the API, and the relay all speak only to
//! this trait (CMOS-14-DEP-042).

pub mod mem;
pub mod postgres;
pub mod sqlite;

use axum::async_trait;

use commos_core::common::Uuid;
use commos_core::entities::call::Call;
use commos_core::entities::call_flow::{CallFlow, CallFlowRevision};
use commos_core::entities::carrier::Carrier;
use commos_core::entities::cdr::Cdr;
use commos_core::entities::channel::Channel;
use commos_core::entities::device::Device;
use commos_core::entities::did::Did;
use commos_core::entities::extension::Extension;
use commos_core::entities::forwarding::Forwarding;
use commos_core::entities::gateway::Gateway;
use commos_core::entities::ivr::Ivr;
use commos_core::entities::message::Message;
use commos_core::entities::trunk::Trunk;
use commos_core::entities::object::Object;
use commos_core::entities::presence_state::PresenceState;
use commos_core::entities::queue::Queue;
use commos_core::entities::recording::Recording;
use commos_core::entities::ring_group::RingGroup;
use commos_core::entities::route::Route;
use commos_core::entities::thread::Thread;
use commos_core::entities::user::User;
use commos_core::entities::video_room::VideoRoom;
use commos_core::entities::voicemail::Voicemail;
use commos_core::entities::webhook::Webhook;

pub use mem::MemStore;
pub use postgres::PgStore;
pub use sqlite::SqliteStore;

/// One durable transaction: entity upserts and the events they produce, committed together.
#[derive(Default)]
pub struct Tx {
    pub calls: Vec<Call>,
    /// Messaging workload entities — peers of `Call` on the same substrate.
    pub channels: Vec<Channel>,
    pub threads: Vec<Thread>,
    pub messages: Vec<Message>,
    /// Real-time workload entities — video/presence peers of `Call` on the same substrate.
    pub video_rooms: Vec<VideoRoom>,
    pub presence: Vec<PresenceState>,
    /// Billing (CDR) and contact-centre (Queue) entities.
    pub cdrs: Vec<Cdr>,
    pub queues: Vec<Queue>,
    /// Multi-destination routing targets — ring groups (fan-out) and per-extension
    /// call-forwarding / follow-me rules.
    pub ring_groups: Vec<RingGroup>,
    pub forwardings: Vec<Forwarding>,
    /// Routing programs — versioned CallFlows and IVR menu nodes.
    pub call_flows: Vec<CallFlow>,
    pub ivrs: Vec<Ivr>,
    /// Immutable published CallFlow snapshots (append-only history; never updated).
    pub call_flow_revisions: Vec<CallFlowRevision>,
    /// PSTN / SIP trunking — carriers, their gateways and trunks, and inbound DIDs.
    pub carriers: Vec<Carrier>,
    pub gateways: Vec<Gateway>,
    pub trunks: Vec<Trunk>,
    pub dids: Vec<Did>,
    /// Provisioning entities — people, extensions, phones, and routes (onboarding).
    pub users: Vec<User>,
    pub extensions: Vec<Extension>,
    pub devices: Vec<Device>,
    pub routes: Vec<Route>,
    /// Integration entities — outbound webhook subscriptions.
    pub webhooks: Vec<Webhook>,
    /// Stored-object metadata (recordings, voicemail, exports, …); bytes live in the ObjectStore.
    pub objects: Vec<Object>,
    /// Call recordings — a Call ↔ audio Object link.
    pub recordings: Vec<Recording>,
    /// Voicemails — a mailbox ↔ audio Object link; the `read` flag versions forward.
    pub voicemails: Vec<Voicemail>,
    pub events: Vec<serde_json::Value>,
    /// Optional idempotency key to record for a create (CMOS-04-API: `Idempotency-Key`).
    pub idempotency: Option<(Uuid, String, Uuid)>, // (tenant, key, call_id)
}

/// A record awaiting relay to the Event Bus.
#[derive(Clone)]
pub struct OutboxRecord {
    pub seq: u64,
    pub event: serde_json::Value,
}

/// A page of a cursor-paginated listing (Volume 4 pagination: `{items, next_cursor}`).
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("optimistic-concurrency conflict on {entity} {id}: expected version {expected}")]
    VersionConflict {
        entity: &'static str,
        id: String,
        expected: u64,
    },
    #[error("storage backend error: {0}")]
    Backend(String),
}

/// The persistence + outbox contract. All reads are tenant-scoped: a caller cannot ask
/// for another tenant's data (CMOS-03-ARCH-050 defence in depth).
///
/// Async because a real backend (PostgreSQL) is; the in-memory binding satisfies it
/// without ever awaiting.
#[async_trait]
pub trait Store: Send + Sync {
    /// Atomically apply a transaction: upsert entities and append their events to the
    /// outbox. This is the single write path (CMOS-03-ARCH-030).
    async fn commit(&self, tx: Tx) -> Result<(), StoreError>;

    async fn get_call(&self, tenant: Uuid, id: Uuid) -> Result<Option<Call>, StoreError>;
    async fn list_calls(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<Call>, StoreError>;

    // Messaging workload reads — tenant-scoped, mirroring the Call ones.
    async fn get_channel(&self, tenant: Uuid, id: Uuid) -> Result<Option<Channel>, StoreError>;
    async fn list_channels(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<Channel>, StoreError>;

    async fn get_thread(&self, tenant: Uuid, id: Uuid) -> Result<Option<Thread>, StoreError>;
    async fn list_threads(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<Thread>, StoreError>;

    async fn get_message(&self, tenant: Uuid, id: Uuid) -> Result<Option<Message>, StoreError>;
    async fn list_messages(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<Message>, StoreError>;

    // Real-time (video/presence) workload reads — tenant-scoped, mirroring the Call ones.
    async fn get_video_room(
        &self,
        tenant: Uuid,
        id: Uuid,
    ) -> Result<Option<VideoRoom>, StoreError>;
    async fn list_video_rooms(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<VideoRoom>, StoreError>;

    async fn get_presence(
        &self,
        tenant: Uuid,
        id: Uuid,
    ) -> Result<Option<PresenceState>, StoreError>;
    async fn list_presence(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<PresenceState>, StoreError>;

    // Billing (CDR) and contact-centre (Queue) reads — tenant-scoped.
    async fn get_cdr(&self, tenant: Uuid, id: Uuid) -> Result<Option<Cdr>, StoreError>;
    async fn list_cdrs(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<Cdr>, StoreError>;

    async fn get_queue(&self, tenant: Uuid, id: Uuid) -> Result<Option<Queue>, StoreError>;
    async fn list_queues(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<Queue>, StoreError>;

    // Ring groups (fan-out targets) — config CRUD, tenant-scoped.
    async fn get_ring_group(&self, tenant: Uuid, id: Uuid) -> Result<Option<RingGroup>, StoreError>;
    async fn list_ring_groups(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<RingGroup>, StoreError>;
    async fn delete_ring_group(&self, tenant: Uuid, id: Uuid) -> Result<bool, StoreError>;

    // Per-extension forwarding / follow-me rules — config CRUD, tenant-scoped.
    async fn get_forwarding(&self, tenant: Uuid, id: Uuid) -> Result<Option<Forwarding>, StoreError>;
    async fn list_forwardings(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<Forwarding>, StoreError>;
    async fn delete_forwarding(&self, tenant: Uuid, id: Uuid) -> Result<bool, StoreError>;

    // Routing programs — CallFlows (versioned) and IVR menu nodes, tenant-scoped.
    async fn get_call_flow(&self, tenant: Uuid, id: Uuid) -> Result<Option<CallFlow>, StoreError>;
    async fn list_call_flows(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<CallFlow>, StoreError>;

    async fn get_ivr(&self, tenant: Uuid, id: Uuid) -> Result<Option<Ivr>, StoreError>;
    async fn list_ivrs(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<Ivr>, StoreError>;
    async fn delete_ivr(&self, tenant: Uuid, id: Uuid) -> Result<bool, StoreError>;

    /// Fetch one immutable published CallFlow revision by `(call_flow_id, version)`.
    async fn get_call_flow_revision(
        &self,
        tenant: Uuid,
        call_flow_id: Uuid,
        version: u64,
    ) -> Result<Option<CallFlowRevision>, StoreError>;
    /// All revisions of a CallFlow, ascending by version (its append-only publish history).
    async fn list_call_flow_revisions(
        &self,
        tenant: Uuid,
        call_flow_id: Uuid,
    ) -> Result<Vec<CallFlowRevision>, StoreError>;

    // PSTN / SIP trunking — carriers, gateways, trunks, DIDs. Config CRUD, tenant-scoped.
    async fn get_carrier(&self, tenant: Uuid, id: Uuid) -> Result<Option<Carrier>, StoreError>;
    async fn list_carriers(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Carrier>, StoreError>;
    async fn delete_carrier(&self, tenant: Uuid, id: Uuid) -> Result<bool, StoreError>;

    async fn get_gateway(&self, tenant: Uuid, id: Uuid) -> Result<Option<Gateway>, StoreError>;
    async fn list_gateways(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Gateway>, StoreError>;
    async fn delete_gateway(&self, tenant: Uuid, id: Uuid) -> Result<bool, StoreError>;

    async fn get_trunk(&self, tenant: Uuid, id: Uuid) -> Result<Option<Trunk>, StoreError>;
    async fn list_trunks(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Trunk>, StoreError>;
    async fn delete_trunk(&self, tenant: Uuid, id: Uuid) -> Result<bool, StoreError>;

    async fn get_did(&self, tenant: Uuid, id: Uuid) -> Result<Option<Did>, StoreError>;
    async fn list_dids(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Did>, StoreError>;
    async fn delete_did(&self, tenant: Uuid, id: Uuid) -> Result<bool, StoreError>;

    // Provisioning (user/extension/device) reads — tenant-scoped.
    async fn get_user(&self, tenant: Uuid, id: Uuid) -> Result<Option<User>, StoreError>;
    async fn list_users(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<User>, StoreError>;

    async fn get_extension(&self, tenant: Uuid, id: Uuid) -> Result<Option<Extension>, StoreError>;
    async fn list_extensions(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<Extension>, StoreError>;

    async fn get_device(&self, tenant: Uuid, id: Uuid) -> Result<Option<Device>, StoreError>;
    async fn list_devices(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<Device>, StoreError>;

    async fn get_route(&self, tenant: Uuid, id: Uuid) -> Result<Option<Route>, StoreError>;
    async fn list_routes(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<Route>, StoreError>;

    /// Hard-delete a config entity (Extension/Route carry no lifecycle state or audit
    /// history, so removing one is a plain delete — unlike Call/CDR, where deletion is a
    /// state transition, CMOS-00-ENG-012). Returns `true` if a row was removed, `false` if
    /// the id did not exist for this tenant.
    async fn delete_extension(&self, tenant: Uuid, id: Uuid) -> Result<bool, StoreError>;
    async fn delete_route(&self, tenant: Uuid, id: Uuid) -> Result<bool, StoreError>;

    async fn get_webhook(&self, tenant: Uuid, id: Uuid) -> Result<Option<Webhook>, StoreError>;
    async fn list_webhooks(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<Webhook>, StoreError>;
    async fn delete_webhook(&self, tenant: Uuid, id: Uuid) -> Result<bool, StoreError>;

    async fn get_object(&self, tenant: Uuid, id: Uuid) -> Result<Option<Object>, StoreError>;
    async fn list_objects(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<Object>, StoreError>;
    async fn delete_object(&self, tenant: Uuid, id: Uuid) -> Result<bool, StoreError>;

    async fn get_recording(&self, tenant: Uuid, id: Uuid) -> Result<Option<Recording>, StoreError>;
    async fn list_recordings(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<Recording>, StoreError>;

    async fn get_voicemail(&self, tenant: Uuid, id: Uuid) -> Result<Option<Voicemail>, StoreError>;
    async fn list_voicemails(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<Voicemail>, StoreError>;
    /// Remove a voicemail's metadata row (its audio Object is deleted separately via
    /// [`Store::delete_object`]). Returns whether a row was removed. Used by dial-in retrieval
    /// (`*97`) when the mailbox owner presses delete.
    async fn delete_voicemail(&self, tenant: Uuid, id: Uuid) -> Result<bool, StoreError>;

    /// SIP shared-secret credentials (Volume 9), keyed by `(tenant, sip username)`. Not a
    /// frozen contract entity — a per-device secret used to authenticate SIP digest and served
    /// (once) to the phone during provisioning. Stored as plaintext because the phone needs it;
    /// a production deployment keeps it in the secrets manager and persists only the HA1.
    async fn put_sip_credential(
        &self,
        tenant: Uuid,
        username: &str,
        secret: &str,
    ) -> Result<(), StoreError>;
    async fn get_sip_credential(
        &self,
        tenant: Uuid,
        username: &str,
    ) -> Result<Option<String>, StoreError>;

    /// Return the call id previously created under this idempotency key, if any.
    async fn call_for_idempotency_key(
        &self,
        tenant: Uuid,
        key: &str,
    ) -> Result<Option<Uuid>, StoreError>;

    /// Relay support: take up to `max` un-relayed records (does not advance the cursor).
    async fn peek_outbox(&self, max: usize) -> Result<Vec<OutboxRecord>, StoreError>;
    /// Mark everything up to and including `seq` as relayed (durable cursor advance).
    async fn ack_outbox(&self, up_to_seq: u64) -> Result<(), StoreError>;
}
