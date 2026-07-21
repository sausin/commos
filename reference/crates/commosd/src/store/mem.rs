//! In-memory binding of [`Store`] (CMOS-14-DEP-021: the single binary provides an embedded
//! equivalent so it needs no external broker/database). A single mutex stands in for the
//! database transaction boundary, so `commit` is genuinely all-or-nothing.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use axum::async_trait;

use commos_core::common::Uuid;
use commos_core::entities::call::Call;
use commos_core::entities::cdr::Cdr;
use commos_core::entities::channel::Channel;
use commos_core::entities::device::Device;
use commos_core::entities::extension::Extension;
use commos_core::entities::message::Message;
use commos_core::entities::object::Object;
use commos_core::entities::presence_state::PresenceState;
use commos_core::entities::queue::Queue;
use commos_core::entities::recording::Recording;
use commos_core::entities::route::Route;
use commos_core::entities::thread::Thread;
use commos_core::entities::user::User;
use commos_core::entities::video_room::VideoRoom;
use commos_core::entities::voicemail::Voicemail;
use commos_core::entities::webhook::Webhook;

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
    /// Real-time (video/presence) workload tables, each with their own insertion-order index.
    video_rooms: HashMap<(Uuid, Uuid), VideoRoom>,
    video_room_order: Vec<(Uuid, Uuid)>,
    presence: HashMap<(Uuid, Uuid), PresenceState>,
    presence_order: Vec<(Uuid, Uuid)>,
    /// Billing (CDR) and contact-centre (Queue) tables.
    cdrs: HashMap<(Uuid, Uuid), Cdr>,
    cdr_order: Vec<(Uuid, Uuid)>,
    queues: HashMap<(Uuid, Uuid), Queue>,
    queue_order: Vec<(Uuid, Uuid)>,
    /// Provisioning (user/extension/device) tables.
    users: HashMap<(Uuid, Uuid), User>,
    user_order: Vec<(Uuid, Uuid)>,
    extensions: HashMap<(Uuid, Uuid), Extension>,
    extension_order: Vec<(Uuid, Uuid)>,
    devices: HashMap<(Uuid, Uuid), Device>,
    device_order: Vec<(Uuid, Uuid)>,
    routes: HashMap<(Uuid, Uuid), Route>,
    route_order: Vec<(Uuid, Uuid)>,
    webhooks: HashMap<(Uuid, Uuid), Webhook>,
    webhook_order: Vec<(Uuid, Uuid)>,
    objects: HashMap<(Uuid, Uuid), Object>,
    object_order: Vec<(Uuid, Uuid)>,
    recordings: HashMap<(Uuid, Uuid), Recording>,
    recording_order: Vec<(Uuid, Uuid)>,
    voicemails: HashMap<(Uuid, Uuid), Voicemail>,
    voicemail_order: Vec<(Uuid, Uuid)>,
    /// SIP shared secrets, keyed by (tenant, username).
    sip_credentials: HashMap<(Uuid, String), String>,
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
        // Real-time entities are created at version 0 → an id collision is a conflict.
        for vr in &tx.video_rooms {
            let key = (vr.base.tenant_id, vr.base.id);
            if g.video_rooms.contains_key(&key) || vr.base.version != 0 {
                return Err(StoreError::VersionConflict {
                    entity: "VideoRoom",
                    id: vr.base.id.to_string(),
                    expected: 0,
                });
            }
        }
        // PresenceState is keyed by its own id and, for the MVP, inserted at version 0 like
        // any other create (an id collision is a conflict). A fuller upsert-by-subject —
        // one live row per (tenant, user_id) that versions forward — is a later refinement.
        for p in &tx.presence {
            let key = (p.base.tenant_id, p.base.id);
            if g.presence.contains_key(&key) || p.base.version != 0 {
                return Err(StoreError::VersionConflict {
                    entity: "PresenceState",
                    id: p.base.id.to_string(),
                    expected: 0,
                });
            }
        }
        for c in &tx.cdrs {
            let key = (c.base.tenant_id, c.base.id);
            if g.cdrs.contains_key(&key) || c.base.version != 0 {
                return Err(StoreError::VersionConflict { entity: "CDR", id: c.base.id.to_string(), expected: 0 });
            }
        }
        // Provisioning entities support in-place update (config re-import reconciles by
        // natural key and bumps the version): a create is v0, an update carries the next
        // version and the stored row must be exactly one behind — mirroring Call.
        for q in &tx.queues {
            let key = (q.base.tenant_id, q.base.id);
            if let Some(existing) = g.queues.get(&key) {
                if q.base.version != existing.base.version + 1 {
                    return Err(StoreError::VersionConflict { entity: "Queue", id: q.base.id.to_string(), expected: existing.base.version + 1 });
                }
            } else if q.base.version != 0 {
                return Err(StoreError::VersionConflict { entity: "Queue", id: q.base.id.to_string(), expected: 0 });
            }
        }
        for u in &tx.users {
            let key = (u.base.tenant_id, u.base.id);
            if let Some(existing) = g.users.get(&key) {
                if u.base.version != existing.base.version + 1 {
                    return Err(StoreError::VersionConflict { entity: "User", id: u.base.id.to_string(), expected: existing.base.version + 1 });
                }
            } else if u.base.version != 0 {
                return Err(StoreError::VersionConflict { entity: "User", id: u.base.id.to_string(), expected: 0 });
            }
        }
        for e in &tx.extensions {
            let key = (e.base.tenant_id, e.base.id);
            if let Some(existing) = g.extensions.get(&key) {
                if e.base.version != existing.base.version + 1 {
                    return Err(StoreError::VersionConflict { entity: "Extension", id: e.base.id.to_string(), expected: existing.base.version + 1 });
                }
            } else if e.base.version != 0 {
                return Err(StoreError::VersionConflict { entity: "Extension", id: e.base.id.to_string(), expected: 0 });
            }
        }
        for d in &tx.devices {
            let key = (d.base.tenant_id, d.base.id);
            if let Some(existing) = g.devices.get(&key) {
                if d.base.version != existing.base.version + 1 {
                    return Err(StoreError::VersionConflict { entity: "Device", id: d.base.id.to_string(), expected: existing.base.version + 1 });
                }
            } else if d.base.version != 0 {
                return Err(StoreError::VersionConflict { entity: "Device", id: d.base.id.to_string(), expected: 0 });
            }
        }
        for r in &tx.routes {
            let key = (r.base.tenant_id, r.base.id);
            if let Some(existing) = g.routes.get(&key) {
                if r.base.version != existing.base.version + 1 {
                    return Err(StoreError::VersionConflict { entity: "Route", id: r.base.id.to_string(), expected: existing.base.version + 1 });
                }
            } else if r.base.version != 0 {
                return Err(StoreError::VersionConflict { entity: "Route", id: r.base.id.to_string(), expected: 0 });
            }
        }
        for w in &tx.webhooks {
            let key = (w.base.tenant_id, w.base.id);
            if let Some(existing) = g.webhooks.get(&key) {
                if w.base.version != existing.base.version + 1 {
                    return Err(StoreError::VersionConflict { entity: "Webhook", id: w.base.id.to_string(), expected: existing.base.version + 1 });
                }
            } else if w.base.version != 0 {
                return Err(StoreError::VersionConflict { entity: "Webhook", id: w.base.id.to_string(), expected: 0 });
            }
        }
        for o in &tx.objects {
            let key = (o.base.tenant_id, o.base.id);
            if let Some(existing) = g.objects.get(&key) {
                if o.base.version != existing.base.version + 1 {
                    return Err(StoreError::VersionConflict { entity: "Object", id: o.base.id.to_string(), expected: existing.base.version + 1 });
                }
            } else if o.base.version != 0 {
                return Err(StoreError::VersionConflict { entity: "Object", id: o.base.id.to_string(), expected: 0 });
            }
        }
        for r in &tx.recordings {
            let key = (r.base.tenant_id, r.base.id);
            if g.recordings.contains_key(&key) || r.base.version != 0 {
                return Err(StoreError::VersionConflict { entity: "Recording", id: r.base.id.to_string(), expected: 0 });
            }
        }
        // Voicemails support in-place update (the `read` flag versions forward): a create is
        // v0, an update carries the next version and the stored row must be exactly one behind.
        for v in &tx.voicemails {
            let key = (v.base.tenant_id, v.base.id);
            if let Some(existing) = g.voicemails.get(&key) {
                if v.base.version != existing.base.version + 1 {
                    return Err(StoreError::VersionConflict { entity: "Voicemail", id: v.base.id.to_string(), expected: existing.base.version + 1 });
                }
            } else if v.base.version != 0 {
                return Err(StoreError::VersionConflict { entity: "Voicemail", id: v.base.id.to_string(), expected: 0 });
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
        for vr in tx.video_rooms {
            let key = (vr.base.tenant_id, vr.base.id);
            g.video_room_order.push(key);
            g.video_rooms.insert(key, vr);
        }
        for p in tx.presence {
            let key = (p.base.tenant_id, p.base.id);
            g.presence_order.push(key);
            g.presence.insert(key, p);
        }
        for c in tx.cdrs {
            let key = (c.base.tenant_id, c.base.id);
            g.cdr_order.push(key);
            g.cdrs.insert(key, c);
        }
        for q in tx.queues {
            let key = (q.base.tenant_id, q.base.id);
            if !g.queues.contains_key(&key) {
                g.queue_order.push(key);
            }
            g.queues.insert(key, q);
        }
        for u in tx.users {
            let key = (u.base.tenant_id, u.base.id);
            if !g.users.contains_key(&key) {
                g.user_order.push(key);
            }
            g.users.insert(key, u);
        }
        for e in tx.extensions {
            let key = (e.base.tenant_id, e.base.id);
            if !g.extensions.contains_key(&key) {
                g.extension_order.push(key);
            }
            g.extensions.insert(key, e);
        }
        for d in tx.devices {
            let key = (d.base.tenant_id, d.base.id);
            if !g.devices.contains_key(&key) {
                g.device_order.push(key);
            }
            g.devices.insert(key, d);
        }
        for r in tx.routes {
            let key = (r.base.tenant_id, r.base.id);
            if !g.routes.contains_key(&key) {
                g.route_order.push(key);
            }
            g.routes.insert(key, r);
        }
        for w in tx.webhooks {
            let key = (w.base.tenant_id, w.base.id);
            if !g.webhooks.contains_key(&key) {
                g.webhook_order.push(key);
            }
            g.webhooks.insert(key, w);
        }
        for o in tx.objects {
            let key = (o.base.tenant_id, o.base.id);
            if !g.objects.contains_key(&key) {
                g.object_order.push(key);
            }
            g.objects.insert(key, o);
        }
        for r in tx.recordings {
            let key = (r.base.tenant_id, r.base.id);
            g.recording_order.push(key);
            g.recordings.insert(key, r);
        }
        for v in tx.voicemails {
            let key = (v.base.tenant_id, v.base.id);
            if !g.voicemails.contains_key(&key) {
                g.voicemail_order.push(key);
            }
            g.voicemails.insert(key, v);
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

    async fn get_video_room(
        &self,
        tenant: Uuid,
        id: Uuid,
    ) -> Result<Option<VideoRoom>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(g.video_rooms.get(&(tenant, id)).cloned())
    }

    async fn list_video_rooms(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<VideoRoom>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(page_from(
            &g.video_room_order,
            |k| g.video_rooms.get(k).cloned(),
            tenant,
            limit,
            cursor,
        ))
    }

    async fn get_presence(
        &self,
        tenant: Uuid,
        id: Uuid,
    ) -> Result<Option<PresenceState>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(g.presence.get(&(tenant, id)).cloned())
    }

    async fn list_presence(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<PresenceState>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(page_from(
            &g.presence_order,
            |k| g.presence.get(k).cloned(),
            tenant,
            limit,
            cursor,
        ))
    }

    async fn get_cdr(&self, tenant: Uuid, id: Uuid) -> Result<Option<Cdr>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(g.cdrs.get(&(tenant, id)).cloned())
    }
    async fn list_cdrs(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Cdr>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(page_from(&g.cdr_order, |k| g.cdrs.get(k).cloned(), tenant, limit, cursor))
    }

    async fn get_queue(&self, tenant: Uuid, id: Uuid) -> Result<Option<Queue>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(g.queues.get(&(tenant, id)).cloned())
    }
    async fn list_queues(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Queue>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(page_from(&g.queue_order, |k| g.queues.get(k).cloned(), tenant, limit, cursor))
    }

    async fn get_user(&self, tenant: Uuid, id: Uuid) -> Result<Option<User>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(g.users.get(&(tenant, id)).cloned())
    }
    async fn list_users(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<User>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(page_from(&g.user_order, |k| g.users.get(k).cloned(), tenant, limit, cursor))
    }

    async fn get_extension(&self, tenant: Uuid, id: Uuid) -> Result<Option<Extension>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(g.extensions.get(&(tenant, id)).cloned())
    }
    async fn list_extensions(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Extension>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(page_from(&g.extension_order, |k| g.extensions.get(k).cloned(), tenant, limit, cursor))
    }

    async fn get_device(&self, tenant: Uuid, id: Uuid) -> Result<Option<Device>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(g.devices.get(&(tenant, id)).cloned())
    }
    async fn list_devices(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Device>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(page_from(&g.device_order, |k| g.devices.get(k).cloned(), tenant, limit, cursor))
    }

    async fn get_route(&self, tenant: Uuid, id: Uuid) -> Result<Option<Route>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(g.routes.get(&(tenant, id)).cloned())
    }
    async fn list_routes(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Route>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(page_from(&g.route_order, |k| g.routes.get(k).cloned(), tenant, limit, cursor))
    }

    async fn delete_extension(&self, tenant: Uuid, id: Uuid) -> Result<bool, StoreError> {
        let mut g = self.inner.lock().expect("store mutex not poisoned");
        let key = (tenant, id);
        let removed = g.extensions.remove(&key).is_some();
        if removed {
            g.extension_order.retain(|k| k != &key);
        }
        Ok(removed)
    }
    async fn delete_route(&self, tenant: Uuid, id: Uuid) -> Result<bool, StoreError> {
        let mut g = self.inner.lock().expect("store mutex not poisoned");
        let key = (tenant, id);
        let removed = g.routes.remove(&key).is_some();
        if removed {
            g.route_order.retain(|k| k != &key);
        }
        Ok(removed)
    }

    async fn get_webhook(&self, tenant: Uuid, id: Uuid) -> Result<Option<Webhook>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(g.webhooks.get(&(tenant, id)).cloned())
    }
    async fn list_webhooks(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Webhook>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(page_from(&g.webhook_order, |k| g.webhooks.get(k).cloned(), tenant, limit, cursor))
    }
    async fn delete_webhook(&self, tenant: Uuid, id: Uuid) -> Result<bool, StoreError> {
        let mut g = self.inner.lock().expect("store mutex not poisoned");
        let key = (tenant, id);
        let removed = g.webhooks.remove(&key).is_some();
        if removed {
            g.webhook_order.retain(|k| k != &key);
        }
        Ok(removed)
    }

    async fn get_object(&self, tenant: Uuid, id: Uuid) -> Result<Option<Object>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(g.objects.get(&(tenant, id)).cloned())
    }
    async fn list_objects(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Object>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(page_from(&g.object_order, |k| g.objects.get(k).cloned(), tenant, limit, cursor))
    }
    async fn delete_object(&self, tenant: Uuid, id: Uuid) -> Result<bool, StoreError> {
        let mut g = self.inner.lock().expect("store mutex not poisoned");
        let key = (tenant, id);
        let removed = g.objects.remove(&key).is_some();
        if removed {
            g.object_order.retain(|k| k != &key);
        }
        Ok(removed)
    }

    async fn get_recording(&self, tenant: Uuid, id: Uuid) -> Result<Option<Recording>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(g.recordings.get(&(tenant, id)).cloned())
    }
    async fn list_recordings(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Recording>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(page_from(&g.recording_order, |k| g.recordings.get(k).cloned(), tenant, limit, cursor))
    }

    async fn get_voicemail(&self, tenant: Uuid, id: Uuid) -> Result<Option<Voicemail>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(g.voicemails.get(&(tenant, id)).cloned())
    }
    async fn list_voicemails(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Voicemail>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(page_from(&g.voicemail_order, |k| g.voicemails.get(k).cloned(), tenant, limit, cursor))
    }

    async fn put_sip_credential(&self, tenant: Uuid, username: &str, secret: &str) -> Result<(), StoreError> {
        let mut g = self.inner.lock().expect("store mutex not poisoned");
        g.sip_credentials.insert((tenant, username.to_string()), secret.to_string());
        Ok(())
    }
    async fn get_sip_credential(&self, tenant: Uuid, username: &str) -> Result<Option<String>, StoreError> {
        let g = self.inner.lock().expect("store mutex not poisoned");
        Ok(g.sip_credentials.get(&(tenant, username.to_string())).cloned())
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
