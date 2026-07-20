//! Queue (contact-centre control plane) — creates the contact-centre workload's Queue
//! entity.
//!
//! This is a contact-centre *peer* of [`crate::control::messaging::MessagingService`] and
//! [`crate::control::routing::Routing`] on the same substrate (CMOS-02-DOM-100): the same
//! commit-through-the-[`Store`] spine, proving the platform is workload-general. Scope here
//! is CREATE + READ (MVP) — no state transitions.
//!
//! **No event on create.** A Queue is *configuration*, not an occurrence, and there is no
//! `QueueCreated` in the frozen event catalogue. So — unlike Channel/Call creation, which
//! commit an entity together with its canonical event atomically — this create persists the
//! Queue *without* emitting any event. The commit therefore carries an empty `events` vec.

use std::sync::Arc;

use commos_core::common::Uuid;
use commos_core::entities::queue::{Queue, QueueStrategy};

use crate::relay::RelaySignal;
use crate::store::{Store, StoreError, Tx};

/// The Queue service. Stateless between requests — all state lives in the [`Store`]
/// (CMOS-03-ARCH-010), so any node can serve any request.
#[derive(Clone)]
pub struct QueueService {
    store: Arc<dyn Store>,
    signal: RelaySignal,
}

impl QueueService {
    pub fn new(store: Arc<dyn Store>, signal: RelaySignal) -> Self {
        QueueService { store, signal }
    }

    /// Create a Queue and persist it.
    ///
    /// Note: no `QueueCreated` event is emitted — a Queue is configuration, not an
    /// occurrence, and no such event exists in the frozen catalogue. The commit carries the
    /// Queue alone (empty `events`). We still `signal.wake()` to keep the relay loop's
    /// liveness contract uniform across services.
    pub async fn create_queue(
        &self,
        tenant: Uuid,
        strategy: QueueStrategy,
        members: Vec<String>,
        sla_seconds: Option<i64>,
        max_wait_ms: Option<i64>,
        overflow_ref: Option<String>,
    ) -> Result<Queue, StoreError> {
        let mut queue = Queue::create(tenant, strategy);
        queue.members = members;
        queue.sla_seconds = sla_seconds;
        queue.max_wait_ms = max_wait_ms;
        queue.overflow_ref = overflow_ref;

        // Config, not occurrence: persist WITHOUT an event (no `QueueCreated` exists).
        self.store
            .commit(Tx {
                queues: vec![queue.clone()],
                ..Default::default()
            })
            .await?;
        self.signal.wake();

        Ok(queue)
    }
}
