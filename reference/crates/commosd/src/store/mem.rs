//! In-memory binding of [`Store`] (CMOS-14-DEP-021: the single binary provides an embedded
//! equivalent so it needs no external broker/database). A single mutex stands in for the
//! database transaction boundary, so `commit` is genuinely all-or-nothing.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use axum::async_trait;

use commos_core::common::Uuid;
use commos_core::entities::call::Call;
use commos_core::entities::channel::Channel;
use commos_core::entities::message::Message;
use commos_core::entities::thread::Thread;

use super::{OutboxRecord, Page, Store, StoreError, Tx};

/// Shared cursor-paging over an insertion-ordered index, keyed by (tenant, id). Mirrors
/// `list_calls`: resume strictly after the cursor id, tenant-scoped, and offer a next
/// cursor only when more rows remain for the tenant.
fn page_from<T: Clone>(
    order: &[(Uuid, Uuid)],
    lookup: impl Fn(&(Uuid, Uuid)) -> Option<T>,
    tenant: Uuid,
    limit: usize,
    cursor: Option<String>,
) -> Page<T> {
    let start = match cursor.as_deref() {
        None => 0,
        Some(c) => order
            .iter()
            .position(|(t, id)| *t == tenant && id.to_string() == c)
            .map(|p| p + 1)
            .unwrap_or(0),
    };

    let mut items = Vec::new();
    let mut last_key: Option<(Uuid, Uuid)> = None;
    for key in order.iter().skip(start) {
        if key.0 != tenant {
            continue;
        }
        if items.len() == limit {
            break;
        }
        if let Some(item) = lookup(key) {
            items.push(item);
            last_key = Some(*key);
        }
    }

    let more_remain = last_key
        .and_then(|lk| order.iter().position(|k| k == &lk))
        .map(|p| order.iter().skip(p + 1).any(|(t, _)| *t == tenant))
        .unwrap_or(false);
    let next_cursor = if more_remain {
        last_key.map(|(_, id)| id.to_string())
    } else {
        None
    };

    Page { items, next_cursor }
}

/// In-memory system of record + outbox. State is not durable across restarts.
pub struct MemStore {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Entity table, keyed by (tenant, id).
    calls: HashMap<(Uuid, Uuid), Call>,
    /// Insertion order of ids for stable, time-ordered listing (UUIDv7 ≈ creation order).
    order: Vec<(Uuid, Uuid)>,
    /// Messaging workload tables, each with their own insertion-order index.
    channels: HashMap<(Uuid, Uuid), Channel>,
    channel_order: Vec<(Uuid, Uuid)>,
    threads: HashMap<(Uuid, Uuid), Thread>,
    thread_order: Vec<(Uuid, Uuid)>,
    messages: HashMap<(Uuid, Uuid), Message>,
    message_order: Vec<(Uuid, Uuid)>,
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

#[async_trait]
impl Store for MemStore {
    async fn commit(&self, tx: Tx) -> Result<(), StoreError> {
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
        // Messaging entities are created at version 0 → an id collision is a conflict.
        for ch in &tx.channels {
            let key = (ch.base.tenant_id, ch.base.id);
            if g.channels.contains_key(&key) || ch.base.version != 0 {
                return Err(StoreError::VersionConflict {
                    entity: "Channel",
                    id: ch.base.id.to_string(),
                    expected: 0,
                });
            }
        }
        for th in &tx.threads {
            let key = (th.base.tenant_id, th.base.id);
            if g.threads.contains_key(&key) || th.base.version != 0 {
                return Err(StoreError::VersionConflict {
                    entity: "Thread",
                    id: th.base.id.to_string(),
                    expected: 0,
                });
            }
        }
        for m in &tx.messages {
            let key = (m.base.tenant_id, m.base.id);
            if g.messages.contains_key(&key) || m.base.version != 0 {
                return Err(StoreError::VersionConflict {
                    entity: "Message",
                    id: m.base.id.to_string(),
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
        for ch in tx.channels {
            let key = (ch.base.tenant_id, ch.base.id);
            g.channel_order.push(key);
            g.channels.insert(key, ch);
        }
        for th in tx.threads {
            let key = (th.base.tenant_id, th.base.id);
            g.thread_order.push(key);
            g.threads.insert(key, th);
        }
        for m in tx.messages {
            let key = (m.base.tenant_id, m.base.id);
            g.message_order.push(key);
            g.messages.insert(key, m);
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

    async fn get_call(&self, tenant: Uuid, id: Uuid) -> Result<Option<Call>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(g.calls.get(&(tenant, id)).cloned())
    }

    async fn list_calls(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<Call>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        // Cursor is the last id returned; resume strictly after it in insertion order.
        let start = match cursor.as_deref() {
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

        Ok(Page { items, next_cursor })
    }

    async fn get_channel(&self, tenant: Uuid, id: Uuid) -> Result<Option<Channel>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(g.channels.get(&(tenant, id)).cloned())
    }

    async fn list_channels(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<Channel>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(page_from(
            &g.channel_order,
            |k| g.channels.get(k).cloned(),
            tenant,
            limit,
            cursor,
        ))
    }

    async fn get_thread(&self, tenant: Uuid, id: Uuid) -> Result<Option<Thread>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(g.threads.get(&(tenant, id)).cloned())
    }

    async fn list_threads(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<Thread>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(page_from(
            &g.thread_order,
            |k| g.threads.get(k).cloned(),
            tenant,
            limit,
            cursor,
        ))
    }

    async fn get_message(&self, tenant: Uuid, id: Uuid) -> Result<Option<Message>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(g.messages.get(&(tenant, id)).cloned())
    }

    async fn list_messages(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<Message>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(page_from(
            &g.message_order,
            |k| g.messages.get(k).cloned(),
            tenant,
            limit,
            cursor,
        ))
    }

    async fn call_for_idempotency_key(
        &self,
        tenant: Uuid,
        key: &str,
    ) -> Result<Option<Uuid>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(g.idempotency.get(&(tenant, key.to_string())).copied())
    }

    async fn peek_outbox(&self, max: usize) -> Result<Vec<OutboxRecord>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(g.outbox.iter().take(max).cloned().collect())
    }

    async fn ack_outbox(&self, up_to_seq: u64) -> Result<(), StoreError> {
        let mut g = self.inner.lock().expect("store mutex not poisoned");
        while let Some(front) = g.outbox.front() {
            if front.seq <= up_to_seq {
                g.outbox.pop_front();
            } else {
                break;
            }
        }
        g.relayed_through = g.relayed_through.max(up_to_seq);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use commos_core::entities::call::{Call, Direction};

    fn call(tenant: Uuid) -> Call {
        Call::originate(tenant, Direction::Outbound, "sip:100", "+14155550100")
    }

    #[tokio::test]
    async fn commit_persists_and_queues_outbox_together() {
        let store = MemStore::new();
        let tenant = Uuid::now_v7();
        let c = call(tenant);
        let id = c.base.id;
        store
            .commit(Tx {
                calls: vec![c],
                events: vec![serde_json::json!({"type": "CallStarted"})],
                idempotency: None,
                ..Default::default()
            })
            .await
            .unwrap();

        assert!(store.get_call(tenant, id).await.unwrap().is_some());
        assert_eq!(store.peek_outbox(10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn reads_are_tenant_scoped() {
        let store = MemStore::new();
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let c = call(a);
        let id = c.base.id;
        store
            .commit(Tx { calls: vec![c], ..Default::default() })
            .await
            .unwrap();
        // Tenant B cannot see tenant A's call.
        assert!(store.get_call(b, id).await.unwrap().is_none());
        assert_eq!(store.list_calls(b, 50, None).await.unwrap().items.len(), 0);
    }

    #[tokio::test]
    async fn version_conflict_is_rejected() {
        let store = MemStore::new();
        let tenant = Uuid::now_v7();
        let c = call(tenant);
        let id = c.base.id;
        store
            .commit(Tx { calls: vec![c.clone()], ..Default::default() })
            .await
            .unwrap();
        // Re-committing the same v0 (not v1) is a conflict.
        let err = store
            .commit(Tx { calls: vec![c], ..Default::default() })
            .await
            .unwrap_err();
        match err {
            StoreError::VersionConflict { id: got, .. } => assert_eq!(got, id.to_string()),
            other => panic!("expected version conflict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn outbox_ack_advances_cursor() {
        let store = MemStore::new();
        let tenant = Uuid::now_v7();
        store
            .commit(Tx {
                calls: vec![call(tenant)],
                events: vec![serde_json::json!({"n": 0}), serde_json::json!({"n": 1})],
                idempotency: None,
                ..Default::default()
            })
            .await
            .unwrap();
        let batch = store.peek_outbox(10).await.unwrap();
        assert_eq!(batch.len(), 2);
        store.ack_outbox(batch.last().unwrap().seq).await.unwrap();
        assert_eq!(store.peek_outbox(10).await.unwrap().len(), 0);
    }
}
