//! Contact-centre agents — **ephemeral, in-memory live-state** plus basic ACD
//! (Automatic Call Distribution).
//!
//! An agent's live availability ("who is logged in and ready to take work right now") is
//! high-churn runtime state, exactly like a SIP registration
//! ([`crate::control::registrations`]): reconstructable, short-lived, and never the durable
//! system of record. So the live-state map lives in a plain `Arc<Mutex<HashMap<..>>>`,
//! never on disk.
//!
//! **But** unlike a registration, an agent state *transition* is an observable occurrence
//! with a frozen canonical event — `AgentStateChanged` (Volume 5). So while the live map is
//! in-memory, every [`AgentRegistry::set_state`] still emits that event through the
//! transactional outbox, so wallboards and reporting see the transition. The map is the
//! *cache of current state*; the event stream is the *record of transitions*.
//!
//! ## ACD ([`AgentRegistry::enqueue`])
//! MVP distribution: load the target [`Queue`] from the durable store, pick an `AVAILABLE`
//! agent for the tenant, mark them `BUSY` (emitting `AgentStateChanged`), and return the
//! [`Assignment`]. The Queue's `strategy` (ROUND_ROBIN / RINGALL / …) is respected only
//! *loosely* for this MVP — we always pick the first available agent regardless of strategy.
//! Wiring each strategy to its true dispatch discipline is follow-on work; the seam
//! (`queue.strategy`) is already here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use commos_core::common::{Timestamp, Uuid};
use commos_core::event::{Correlation, Envelope};
use commos_core::events::agent_state_changed::AgentStateChanged;

use crate::relay::RelaySignal;
use crate::store::{Store, StoreError, Tx};

/// Conventional agent availability state used by the ACD picker. Any string is accepted on
/// the wire (the frozen event's `state` is a free `string`); this constant names the one the
/// distributor treats as "ready for work".
pub const STATE_AVAILABLE: &str = "AVAILABLE";
/// State an agent is moved to once assigned work.
pub const STATE_BUSY: &str = "BUSY";

/// An agent's live availability. Ephemeral — never persisted. Keyed in the registry by
/// `(tenant_id, agent_user_id)`.
#[derive(Clone, Debug, Serialize)]
pub struct Agent {
    /// The agent's Identity user id.
    pub agent_user_id: Uuid,
    pub tenant_id: Uuid,
    /// Live availability, e.g. `AVAILABLE` / `BUSY` / `OFFLINE`.
    pub state: String,
    pub updated_at: Timestamp,
}

/// The result of a successful [`AgentRegistry::enqueue`]: which agent got the call.
#[derive(Clone, Debug, Serialize)]
pub struct Assignment {
    pub queue_id: Uuid,
    pub call_id: Uuid,
    pub agent_user_id: Uuid,
}

/// Failure modes of [`AgentRegistry::enqueue`], mapped to Problem-details at the API edge.
#[derive(Debug, thiserror::Error)]
pub enum EnqueueError {
    #[error("no such queue")]
    QueueNotFound,
    #[error("no available agent to take the call")]
    NoAgentAvailable,
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// In-memory, tenant-scoped agent live-state registry plus the MVP ACD distributor.
///
/// Cheap to clone (`Arc` handles). The live-state map is guarded by a single `Mutex` (the
/// working set — agents per hub — is small). It holds the durable [`Store`] + [`RelaySignal`]
/// only to emit `AgentStateChanged` events and to read Queues during distribution; agent
/// live-state itself never hits the store.
#[derive(Clone)]
pub struct AgentRegistry {
    store: Arc<dyn Store>,
    signal: RelaySignal,
    agents: Arc<Mutex<HashMap<(Uuid, Uuid), Agent>>>,
}

impl AgentRegistry {
    pub fn new(store: Arc<dyn Store>, signal: RelaySignal) -> Self {
        AgentRegistry {
            store,
            signal,
            agents: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Upsert the agent's live state, then emit `AgentStateChanged` through the outbox.
    ///
    /// The in-memory map is the cache of current state; the event is the durable record of
    /// the transition. Both an initial login (`OFFLINE`→`AVAILABLE`) and any later change go
    /// through here, so every observable transition is on the event stream.
    pub async fn set_state(
        &self,
        tenant: Uuid,
        agent_user_id: Uuid,
        state: String,
    ) -> Result<Agent, StoreError> {
        let agent = {
            let mut map = self.agents.lock().expect("agent mutex not poisoned");
            let agent = Agent {
                agent_user_id,
                tenant_id: tenant,
                state: state.clone(),
                updated_at: Timestamp::now(),
            };
            map.insert((tenant, agent_user_id), agent.clone());
            agent
        };

        self.emit_state_changed(tenant, agent_user_id, state).await?;
        Ok(agent)
    }

    /// All agents for a tenant (tenant-scoped; other tenants are invisible).
    pub fn list(&self, tenant: Uuid) -> Vec<Agent> {
        let map = self.agents.lock().expect("agent mutex not poisoned");
        map.values()
            .filter(|a| a.tenant_id == tenant)
            .cloned()
            .collect()
    }

    /// Fetch a single agent's live state, scoped to the tenant.
    pub fn get(&self, tenant: Uuid, agent_user_id: Uuid) -> Option<Agent> {
        let map = self.agents.lock().expect("agent mutex not poisoned");
        map.get(&(tenant, agent_user_id)).cloned()
    }

    /// Enqueue `call_id` onto `queue_id` and distribute it to an available agent (MVP ACD).
    ///
    /// Steps: load the Queue (durable), pick an `AVAILABLE` agent for the tenant, mark that
    /// agent `BUSY` (which emits `AgentStateChanged`), and return the [`Assignment`]. The
    /// Queue's `strategy` is respected only loosely here — we always take the first available
    /// agent regardless of ROUND_ROBIN / RINGALL / etc. (documented MVP simplification).
    pub async fn enqueue(
        &self,
        tenant: Uuid,
        queue_id: Uuid,
        call_id: Uuid,
    ) -> Result<Assignment, EnqueueError> {
        // The Queue is durable configuration — load it from the system of record.
        let _queue = self
            .store
            .get_queue(tenant, queue_id)
            .await?
            .ok_or(EnqueueError::QueueNotFound)?;

        // MVP distributor: first AVAILABLE agent for the tenant, ignoring `_queue.strategy`.
        // Deterministic pick (lowest agent id) so the choice is stable across runs.
        let chosen = {
            let map = self.agents.lock().expect("agent mutex not poisoned");
            map.values()
                .filter(|a| a.tenant_id == tenant && a.state == STATE_AVAILABLE)
                .min_by_key(|a| a.agent_user_id.to_string())
                .map(|a| a.agent_user_id)
        };
        let agent_user_id = chosen.ok_or(EnqueueError::NoAgentAvailable)?;

        // Reserve the agent: flip to BUSY, which emits AgentStateChanged.
        self.set_state(tenant, agent_user_id, STATE_BUSY.to_string())
            .await?;

        Ok(Assignment {
            queue_id,
            call_id,
            agent_user_id,
        })
    }

    /// Emit an `AgentStateChanged` event through the transactional outbox and wake the relay.
    async fn emit_state_changed(
        &self,
        tenant: Uuid,
        agent_user_id: Uuid,
        state: String,
    ) -> Result<(), StoreError> {
        let ctx = Correlation::root(tenant);
        let payload = AgentStateChanged {
            agent_user_id,
            state,
        };
        // Fresh idempotency key per transition (agent + emitting event id keep it unique).
        let idem = format!("{}:AgentStateChanged:{}", agent_user_id, Uuid::now_v7());
        let envelope = Envelope::new(payload, &ctx, idem);

        self.store
            .commit(Tx {
                events: vec![envelope.to_json()],
                ..Default::default()
            })
            .await?;
        self.signal.wake();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use commos_core::entities::queue::{Queue, QueueStrategy};
    use crate::store::MemStore;

    fn registry() -> AgentRegistry {
        let store: Arc<dyn Store> = Arc::new(MemStore::new());
        AgentRegistry::new(store, RelaySignal::new())
    }

    fn tenant() -> Uuid {
        Uuid::now_v7()
    }

    #[tokio::test]
    async fn set_state_then_list() {
        let reg = registry();
        let t = tenant();
        let a = Uuid::now_v7();
        let agent = reg.set_state(t, a, "AVAILABLE".into()).await.unwrap();
        assert_eq!(agent.agent_user_id, a);
        assert_eq!(agent.state, "AVAILABLE");

        let items = reg.list(t);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].agent_user_id, a);

        // Upsert transition is reflected in the live cache.
        reg.set_state(t, a, "BUSY".into()).await.unwrap();
        assert_eq!(reg.get(t, a).unwrap().state, "BUSY");
        assert_eq!(reg.list(t).len(), 1, "same agent, not a duplicate");
    }

    #[tokio::test]
    async fn agents_are_tenant_scoped() {
        let reg = registry();
        let a = tenant();
        let b = tenant();
        let agent = Uuid::now_v7();
        reg.set_state(a, agent, "AVAILABLE".into()).await.unwrap();
        assert!(reg.list(b).is_empty(), "other tenant sees nothing");
        assert!(reg.get(b, agent).is_none(), "cannot read across tenants");
    }

    #[tokio::test]
    async fn enqueue_assigns_and_marks_busy() {
        let store: Arc<dyn Store> = Arc::new(MemStore::new());
        let reg = AgentRegistry::new(store.clone(), RelaySignal::new());
        let t = tenant();

        // Durable queue in the store.
        let queue = Queue::create(t, QueueStrategy::RoundRobin);
        store
            .commit(Tx {
                queues: vec![queue.clone()],
                ..Default::default()
            })
            .await
            .unwrap();

        // One available agent.
        let agent = Uuid::now_v7();
        reg.set_state(t, agent, "AVAILABLE".into()).await.unwrap();

        let call_id = Uuid::now_v7();
        let assignment = reg.enqueue(t, queue.base.id, call_id).await.unwrap();
        assert_eq!(assignment.agent_user_id, agent);
        assert_eq!(assignment.call_id, call_id);
        assert_eq!(assignment.queue_id, queue.base.id);

        // The agent was reserved.
        assert_eq!(reg.get(t, agent).unwrap().state, "BUSY");
    }

    #[tokio::test]
    async fn enqueue_no_agent_available() {
        let store: Arc<dyn Store> = Arc::new(MemStore::new());
        let reg = AgentRegistry::new(store.clone(), RelaySignal::new());
        let t = tenant();

        let queue = Queue::create(t, QueueStrategy::Ringall);
        store
            .commit(Tx {
                queues: vec![queue.clone()],
                ..Default::default()
            })
            .await
            .unwrap();

        // Agent is present but BUSY, so not available.
        let agent = Uuid::now_v7();
        reg.set_state(t, agent, "BUSY".into()).await.unwrap();

        let err = reg
            .enqueue(t, queue.base.id, Uuid::now_v7())
            .await
            .unwrap_err();
        assert!(matches!(err, EnqueueError::NoAgentAvailable));
    }

    #[tokio::test]
    async fn enqueue_queue_not_found() {
        let reg = registry();
        let t = tenant();
        let err = reg
            .enqueue(t, Uuid::now_v7(), Uuid::now_v7())
            .await
            .unwrap_err();
        assert!(matches!(err, EnqueueError::QueueNotFound));
    }
}
