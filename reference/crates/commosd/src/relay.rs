//! Outbox relay (CMOS-03-ARCH-030/031; CMOS-05-EVT-010/013).
//!
//! A background worker that drains the transactional outbox to the Event Bus and advances
//! a durable cursor. It runs off the hot path (CMOS-03-ARCH-021: blocking/relaying work
//! never blocks a media path). Delivery is at-least-once; subscribers dedupe on the
//! envelope `idempotency_key` (Volume 5). On a crash between publish and ack, the record
//! is simply re-relayed — never lost.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

use crate::bus::EventBus;
use crate::store::Store;

/// Wakes the relay the moment a commit appends to the outbox, so events surface promptly
/// without a busy-poll. A periodic tick backstops any missed notification.
#[derive(Clone, Default)]
pub struct RelaySignal {
    notify: Arc<Notify>,
}

impl RelaySignal {
    pub fn new() -> Self {
        RelaySignal { notify: Arc::new(Notify::new()) }
    }
    /// Called after every successful commit.
    pub fn wake(&self) {
        self.notify.notify_one();
    }
}

/// Run the relay loop until `shutdown` resolves, then drain what remains and exit
/// (graceful drain — flush the outbox before exit, CMOS-14-DEP-051).
pub async fn run(
    store: Arc<dyn Store>,
    bus: EventBus,
    signal: RelaySignal,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    const BATCH: usize = 128;
    loop {
        drain_once(&store, &bus, BATCH);

        tokio::select! {
            _ = signal.notify.notified() => {}
            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    // Final drain so no committed event is left un-relayed on exit.
                    drain_once(&store, &bus, usize::MAX);
                    tracing::info!("outbox relay drained and stopped");
                    return;
                }
            }
        }
    }
}

fn drain_once(store: &Arc<dyn Store>, bus: &EventBus, max: usize) {
    let batch = store.peek_outbox(max);
    if batch.is_empty() {
        return;
    }
    let mut last_seq = 0;
    for rec in &batch {
        bus.publish(Arc::new(rec.event.clone()));
        last_seq = rec.seq;
    }
    // Ack only after publishing — a crash before this re-delivers, never drops.
    store.ack_outbox(last_seq);
    tracing::debug!(count = batch.len(), through = last_seq, "relayed outbox batch");
}
