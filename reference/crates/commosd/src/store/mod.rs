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

use axum::async_trait;

use commos_core::common::Uuid;
use commos_core::entities::call::Call;
use commos_core::entities::channel::Channel;
use commos_core::entities::message::Message;
use commos_core::entities::thread::Thread;

pub use mem::MemStore;
pub use postgres::PgStore;

/// One durable transaction: entity upserts and the events they produce, committed together.
#[derive(Default)]
pub struct Tx {
    pub calls: Vec<Call>,
    /// Messaging workload entities — peers of `Call` on the same substrate.
    pub channels: Vec<Channel>,
    pub threads: Vec<Thread>,
    pub messages: Vec<Message>,
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
