//! Non-normative introspection helpers.
//!
//! The frozen API surface (Volume 4) is the contract; nothing here is part of it. This
//! module keeps a bounded ring of recently-published events so an operator (or a test)
//! can *see* the event flow during bring-up, exposed under `/_introspect/*` — deliberately
//! outside the versioned `/v1` contract so it can never be mistaken for a stable API.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::bus::EventJson;

const RING_CAPACITY: usize = 256;

/// A bounded, thread-safe ring of recent event envelopes.
#[derive(Clone)]
pub struct RecentEvents {
    inner: Arc<Mutex<VecDeque<EventJson>>>,
}

impl RecentEvents {
    pub fn new() -> Self {
        RecentEvents {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAPACITY))),
        }
    }

    pub fn push(&self, event: EventJson) {
        let mut ring = self.inner.lock().expect("introspection ring not poisoned");
        if ring.len() == RING_CAPACITY {
            ring.pop_front();
        }
        ring.push_back(event);
    }

    /// Snapshot newest-last.
    pub fn snapshot(&self) -> Vec<serde_json::Value> {
        let ring = self.inner.lock().expect("introspection ring not poisoned");
        ring.iter().map(|e| (**e).clone()).collect()
    }
}

impl Default for RecentEvents {
    fn default() -> Self {
        Self::new()
    }
}
