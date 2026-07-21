//! Event Bus (CMOS-03-ARCH-030/031).
//!
//! The bus binding is pluggable behind one interface — NATS JetStream, Redis Streams, or
//! Kafka in a cluster; an in-process broadcast in the single binary (CMOS-14-DEP-021: the
//! single binary provides an embedded equivalent so it needs no external broker). The
//! interface is identical across topologies; only the binding changes.

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::introspect::RecentEvents;

/// A published event is the fully-formed envelope JSON (Volume 5). Cheaply cloneable.
pub type EventJson = Arc<serde_json::Value>;

/// The in-process Event Bus binding.
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<EventJson>,
    /// A bounded ring of recent events for operator/test introspection (non-normative).
    recent: RecentEvents,
}

impl EventBus {
    pub fn new(recent: RecentEvents) -> Self {
        // Capacity bounds in-flight fan-out; slow subscribers lag rather than block the relay.
        let (tx, _rx) = broadcast::channel(1024);
        EventBus { tx, recent }
    }

    /// Publish a relayed event to all subscribers. Called only by the outbox relay, so
    /// every event on the bus has already been durably committed (CMOS-05-EVT-010).
    pub fn publish(&self, event: EventJson) {
        self.recent.push(event.clone());
        // A send error only means "no subscribers right now"; the event is still durable.
        let _ = self.tx.send(event);
    }

    /// Subscribe to the live stream (e.g. Automation, the introspection SSE endpoint).
    pub fn subscribe(&self) -> broadcast::Receiver<EventJson> {
        self.tx.subscribe()
    }
}
