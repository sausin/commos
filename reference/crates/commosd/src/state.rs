//! Shared application state — the wiring that every request handler sees.
//!
//! Handlers are stateless (CMOS-03-ARCH-010); this holds only shared, cloneable handles
//! to the store, the control services, and the bus.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::bus::EventBus;
use crate::control::routing::Routing;
use crate::introspect::RecentEvents;
use crate::store::Store;

/// Cheap-to-clone application state (all fields are `Arc`/handles).
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn Store>,
    pub routing: Routing,
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
        bus: EventBus,
        recent: RecentEvents,
    ) -> Self {
        AppState {
            store,
            routing,
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
