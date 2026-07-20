//! System of record + transactional outbox (CMOS-00-ENG-007; CMOS-03-ARCH-030;
//! CMOS-05-EVT-010).
//!
//! **The guarantee:** every observable state change is written to the outbox *in the same
//! transaction* as the state change. Either both land or neither does; the relay then
//! delivers from the outbox at-least-once. That is what makes "no state change without its
//! event" true even across a crash.
//!
//! [`Store`] is the abstraction; [`MemStore`] is the zero-dependency binding that lets the
//! single binary boot with no PostgreSQL (CMOS-14-DEP-021). A PostgreSQL binding
//! implements the same trait with a real `BEGIN … COMMIT` and a `SELECT … FOR UPDATE SKIP
//! LOCKED` relay — no caller changes (CMOS-14-DEP-042).

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use commos_core::common::Uuid;
use commos_core::entities::call::Call;

/// One durable transaction: entity upserts and the events they produce, committed together.
#[derive(Default)]
pub struct Tx {
    pub calls: Vec<Call>,
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
}

/// The persistence + outbox contract. All reads are tenant-scoped: a caller cannot ask
/// for another tenant's data (CMOS-03-ARCH-050 defence in depth).
pub trait Store: Send + Sync {
    /// Atomically apply a transaction: upsert entities and append their events to the
    /// outbox. This is the single write path (CMOS-03-ARCH-030).
    fn commit(&self, tx: Tx) -> Result<(), StoreError>;

    fn get_call(&self, tenant: Uuid, id: Uuid) -> Option<Call>;
    fn list_calls(&self, tenant: Uuid, limit: usize, cursor: Option<&str>) -> Page<Call>;

    /// Return the call id previously created under this idempotency key, if any.
    fn call_for_idempotency_key(&self, tenant: Uuid, key: &str) -> Option<Uuid>;

    /// Relay support: take up to `max` un-relayed records (does not advance the cursor).
    fn peek_outbox(&self, max: usize) -> Vec<OutboxRecord>;
    /// Mark everything up to and including `seq` as relayed (durable cursor advance).
    fn ack_outbox(&self, up_to_seq: u64);
}

/// In-memory binding of [`Store`]. A single mutex stands in for the database
/// transaction boundary, so `commit` is genuinely all-or-nothing.
pub struct MemStore {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Entity table, keyed by (tenant, id).
    calls: HashMap<(Uuid, Uuid), Call>,
    /// Insertion order of ids for stable, time-ordered listing (UUIDv7 ≈ creation order).
    order: Vec<(Uuid, Uuid)>,
    /// Idempotency ledger: (tenant, key) -> call id.
    idempotency: HashMap<(Uuid, String), Uuid>,
    /// The outbox, in commit order.
    outbox: VecDeque<OutboxRecord>,
    /// Monotonic outbox sequence.
    next_seq: u64,
    /// Highest relayed seq (durable cursor).
    relayed_through: u64,
}

impl MemStore {
    pub fn new() -> Self {
        MemStore {
            inner: Mutex::new(Inner::default()),
        }
    }
}

impl Default for MemStore {
    fn default() -> Self {
        Self::new()
    }
}

impl Store for MemStore {
    fn commit(&self, tx: Tx) -> Result<(), StoreError> {
        let mut g = self.inner.lock().expect("store mutex not poisoned");

        // 1) Validate optimistic concurrency for every upsert before mutating anything,
        //    so the transaction stays all-or-nothing on conflict.
        for call in &tx.calls {
            let key = (call.base.tenant_id, call.base.id);
            if let Some(existing) = g.calls.get(&key) {
                // A new call carries the next version; the stored one must be exactly one behind.
                if call.base.version != existing.base.version + 1 {
                    return Err(StoreError::VersionConflict {
                        entity: "Call",
                        id: call.base.id.to_string(),
                        expected: existing.base.version + 1,
                    });
                }
            } else if call.base.version != 0 {
                return Err(StoreError::VersionConflict {
                    entity: "Call",
                    id: call.base.id.to_string(),
                    expected: 0,
                });
            }
        }

        // 2) Apply. From here nothing can fail, so state + outbox land together.
        for call in tx.calls {
            let key = (call.base.tenant_id, call.base.id);
            if !g.calls.contains_key(&key) {
                g.order.push(key);
            }
            g.calls.insert(key, call);
        }
        if let Some((tenant, key, call_id)) = tx.idempotency {
            g.idempotency.insert((tenant, key), call_id);
        }
        for event in tx.events {
            let seq = g.next_seq;
            g.next_seq += 1;
            g.outbox.push_back(OutboxRecord { seq, event });
        }
        Ok(())
    }

    fn get_call(&self, tenant: Uuid, id: Uuid) -> Option<Call> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        g.calls.get(&(tenant, id)).cloned()
    }

    fn list_calls(&self, tenant: Uuid, limit: usize, cursor: Option<&str>) -> Page<Call> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        // Cursor is the last id returned; resume strictly after it in insertion order.
        let start = match cursor {
            None => 0,
            Some(c) => g
                .order
                .iter()
                .position(|(t, id)| *t == tenant && id.to_string() == c)
                .map(|p| p + 1)
                .unwrap_or(0),
        };

        let mut items = Vec::new();
        let mut last_key: Option<(Uuid, Uuid)> = None;
        for key in g.order.iter().skip(start) {
            if key.0 != tenant {
                continue;
            }
            if items.len() == limit {
                break;
            }
            if let Some(call) = g.calls.get(key) {
                items.push(call.clone());
                last_key = Some(*key);
            }
        }

        // Only offer a cursor if more remain for this tenant beyond what we returned.
        let more_remain = last_key
            .and_then(|lk| g.order.iter().position(|k| k == &lk))
            .map(|p| g.order.iter().skip(p + 1).any(|(t, _)| *t == tenant))
            .unwrap_or(false);
        let next_cursor = if more_remain {
            last_key.map(|(_, id)| id.to_string())
        } else {
            None
        };

        Page { items, next_cursor }
    }

    fn call_for_idempotency_key(&self, tenant: Uuid, key: &str) -> Option<Uuid> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        g.idempotency.get(&(tenant, key.to_string())).copied()
    }

    fn peek_outbox(&self, max: usize) -> Vec<OutboxRecord> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        g.outbox.iter().take(max).cloned().collect()
    }

    fn ack_outbox(&self, up_to_seq: u64) {
        let mut g = self.inner.lock().expect("store mutex not poisoned");
        while let Some(front) = g.outbox.front() {
            if front.seq <= up_to_seq {
                g.outbox.pop_front();
            } else {
                break;
            }
        }
        g.relayed_through = g.relayed_through.max(up_to_seq);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use commos_core::entities::call::{Call, Direction};

    fn call(tenant: Uuid) -> Call {
        Call::originate(tenant, Direction::Outbound, "sip:100", "+14155550100")
    }

    #[test]
    fn commit_persists_and_queues_outbox_together() {
        let store = MemStore::new();
        let tenant = Uuid::now_v7();
        let c = call(tenant);
        let id = c.base.id;
        store
            .commit(Tx {
                calls: vec![c],
                events: vec![serde_json::json!({"type": "CallStarted"})],
                idempotency: None,
            })
            .unwrap();

        assert!(store.get_call(tenant, id).is_some());
        assert_eq!(store.peek_outbox(10).len(), 1);
    }

    #[test]
    fn reads_are_tenant_scoped() {
        let store = MemStore::new();
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let c = call(a);
        let id = c.base.id;
        store
            .commit(Tx { calls: vec![c], ..Default::default() })
            .unwrap();
        // Tenant B cannot see tenant A's call.
        assert!(store.get_call(b, id).is_none());
        assert_eq!(store.list_calls(b, 50, None).items.len(), 0);
    }

    #[test]
    fn version_conflict_is_rejected() {
        let store = MemStore::new();
        let tenant = Uuid::now_v7();
        let c = call(tenant);
        let id = c.base.id;
        store
            .commit(Tx { calls: vec![c.clone()], ..Default::default() })
            .unwrap();
        // Re-committing the same v0 (not v1) is a conflict.
        let err = store
            .commit(Tx { calls: vec![c], ..Default::default() })
            .unwrap_err();
        match err {
            StoreError::VersionConflict { id: got, .. } => assert_eq!(got, id.to_string()),
        }
    }

    #[test]
    fn outbox_ack_advances_cursor() {
        let store = MemStore::new();
        let tenant = Uuid::now_v7();
        store
            .commit(Tx {
                calls: vec![call(tenant)],
                events: vec![serde_json::json!({"n": 0}), serde_json::json!({"n": 1})],
                idempotency: None,
            })
            .unwrap();
        let batch = store.peek_outbox(10);
        assert_eq!(batch.len(), 2);
        store.ack_outbox(batch.last().unwrap().seq);
        assert_eq!(store.peek_outbox(10).len(), 0);
    }
}
