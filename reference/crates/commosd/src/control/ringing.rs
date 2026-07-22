//! Ring groups + call forwarding (control plane) — CRUD for the two multi-destination
//! routing-config entities [`RingGroup`] and [`Forwarding`].
//!
//! Both are *configuration*, not occurrences (CMOS-02-DOM-100): like [`Queue`] they have no
//! creation event in the frozen catalogue, so a create/update commit carries the entity
//! alone (empty `events`). We still `signal.wake()` to keep the relay-loop liveness contract
//! uniform across services.
//!
//! Create is a v0 insert; update loads the current row, applies the caller's changes,
//! [`touch`](commos_core::common::EntityBase::touch)es it to the next version, and commits —
//! the same optimistic-concurrency shape the store enforces for every mutable config entity.
//!
//! [`Queue`]: commos_core::entities::queue::Queue

use std::sync::Arc;

use commos_core::common::Uuid;
use commos_core::entities::forwarding::{ForwardMode, Forwarding};
use commos_core::entities::ring_group::{RingGroup, RingStrategy};

use crate::relay::RelaySignal;
use crate::store::{Store, StoreError, Tx};

#[derive(Debug, thiserror::Error)]
pub enum RingingError {
    #[error("not found")]
    NotFound,
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Fields a caller may set when creating/updating a [`RingGroup`].
#[derive(Clone, Debug)]
pub struct RingGroupInput {
    pub strategy: RingStrategy,
    pub members: Vec<String>,
    pub ring_seconds: Option<i64>,
    pub no_answer_ref: Option<String>,
    pub label: Option<String>,
}

/// Fields a caller may set when creating/updating a [`Forwarding`] rule.
#[derive(Clone, Debug)]
pub struct ForwardingInput {
    pub number: String,
    pub enabled: bool,
    pub mode: ForwardMode,
    pub targets: Vec<String>,
    pub ring_seconds: Option<i64>,
}

/// The ring-group + forwarding config service. Stateless between requests — all state lives
/// in the [`Store`] (CMOS-03-ARCH-010).
#[derive(Clone)]
pub struct RingingService {
    store: Arc<dyn Store>,
    signal: RelaySignal,
}

impl RingingService {
    pub fn new(store: Arc<dyn Store>, signal: RelaySignal) -> Self {
        RingingService { store, signal }
    }

    // ---- Ring groups -----------------------------------------------------------------

    /// Create a ring group (v0 insert). No event — a RingGroup is configuration.
    pub async fn create_ring_group(
        &self,
        tenant: Uuid,
        input: RingGroupInput,
    ) -> Result<RingGroup, StoreError> {
        let mut g = RingGroup::create(tenant, input.strategy);
        g.members = input.members;
        g.ring_seconds = input.ring_seconds;
        g.no_answer_ref = input.no_answer_ref;
        g.label = input.label;
        self.commit_ring_group(g.clone()).await?;
        Ok(g)
    }

    /// Replace a ring group's fields, advancing its version (optimistic concurrency).
    pub async fn update_ring_group(
        &self,
        tenant: Uuid,
        id: Uuid,
        input: RingGroupInput,
    ) -> Result<RingGroup, RingingError> {
        let mut g = self
            .store
            .get_ring_group(tenant, id)
            .await?
            .ok_or(RingingError::NotFound)?;
        g.strategy = input.strategy;
        g.members = input.members;
        g.ring_seconds = input.ring_seconds;
        g.no_answer_ref = input.no_answer_ref;
        g.label = input.label;
        g.base.touch();
        self.commit_ring_group(g.clone()).await?;
        Ok(g)
    }

    pub async fn delete_ring_group(&self, tenant: Uuid, id: Uuid) -> Result<bool, StoreError> {
        let removed = self.store.delete_ring_group(tenant, id).await?;
        if removed {
            self.signal.wake();
        }
        Ok(removed)
    }

    async fn commit_ring_group(&self, g: RingGroup) -> Result<(), StoreError> {
        self.store
            .commit(Tx { ring_groups: vec![g], ..Default::default() })
            .await?;
        self.signal.wake();
        Ok(())
    }

    // ---- Forwarding ------------------------------------------------------------------

    /// Create a forwarding rule (v0 insert). No event — a Forwarding is configuration.
    pub async fn create_forwarding(
        &self,
        tenant: Uuid,
        input: ForwardingInput,
    ) -> Result<Forwarding, StoreError> {
        let mut f = Forwarding::create(tenant, input.number, input.mode);
        f.enabled = input.enabled;
        f.targets = input.targets;
        f.ring_seconds = input.ring_seconds;
        self.commit_forwarding(f.clone()).await?;
        Ok(f)
    }

    /// Replace a forwarding rule's fields, advancing its version (optimistic concurrency).
    pub async fn update_forwarding(
        &self,
        tenant: Uuid,
        id: Uuid,
        input: ForwardingInput,
    ) -> Result<Forwarding, RingingError> {
        let mut f = self
            .store
            .get_forwarding(tenant, id)
            .await?
            .ok_or(RingingError::NotFound)?;
        f.number = input.number;
        f.enabled = input.enabled;
        f.mode = input.mode;
        f.targets = input.targets;
        f.ring_seconds = input.ring_seconds;
        f.base.touch();
        self.commit_forwarding(f.clone()).await?;
        Ok(f)
    }

    pub async fn delete_forwarding(&self, tenant: Uuid, id: Uuid) -> Result<bool, StoreError> {
        let removed = self.store.delete_forwarding(tenant, id).await?;
        if removed {
            self.signal.wake();
        }
        Ok(removed)
    }

    async fn commit_forwarding(&self, f: Forwarding) -> Result<(), StoreError> {
        self.store
            .commit(Tx { forwardings: vec![f], ..Default::default() })
            .await?;
        self.signal.wake();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemStore;

    fn svc() -> (RingingService, Uuid) {
        let store: Arc<dyn Store> = Arc::new(MemStore::new());
        (RingingService::new(store, RelaySignal::new()), Uuid::now_v7())
    }

    fn rg_input() -> RingGroupInput {
        RingGroupInput {
            strategy: RingStrategy::RingAll,
            members: vec!["sip:100".into(), "sip:101".into()],
            ring_seconds: Some(20),
            no_answer_ref: Some("sip:200".into()),
            label: Some("Sales".into()),
        }
    }

    #[tokio::test]
    async fn ring_group_create_update_delete_roundtrip() {
        let (s, t) = svc();
        let g = s.create_ring_group(t, rg_input()).await.unwrap();
        assert_eq!(g.base.version, 0);
        assert_eq!(g.members.len(), 2);

        // Update bumps the version and replaces fields.
        let mut input = rg_input();
        input.strategy = RingStrategy::RoundRobin;
        input.members = vec!["sip:100".into()];
        let updated = s.update_ring_group(t, g.base.id, input).await.unwrap();
        assert_eq!(updated.base.version, 1);
        assert_eq!(updated.strategy, RingStrategy::RoundRobin);
        assert_eq!(updated.members, vec!["sip:100".to_string()]);

        assert!(s.delete_ring_group(t, g.base.id).await.unwrap());
        assert!(!s.delete_ring_group(t, g.base.id).await.unwrap(), "second delete is a no-op");
    }

    #[tokio::test]
    async fn update_missing_ring_group_is_not_found() {
        let (s, t) = svc();
        assert!(matches!(
            s.update_ring_group(t, Uuid::now_v7(), rg_input()).await,
            Err(RingingError::NotFound)
        ));
    }

    #[tokio::test]
    async fn forwarding_update_toggles_enabled() {
        let (s, t) = svc();
        let f = s.create_forwarding(t, ForwardingInput {
            number: "100".into(),
            enabled: true,
            mode: ForwardMode::Always,
            targets: vec!["sip:200".into()],
            ring_seconds: None,
        }).await.unwrap();
        assert!(f.is_active(), "enabled rule with a target is active");

        // Disable via update → the rule is no longer active (routing skips it).
        let disabled = s.update_forwarding(t, f.base.id, ForwardingInput {
            number: "100".into(),
            enabled: false,
            mode: ForwardMode::Always,
            targets: vec!["sip:200".into()],
            ring_seconds: None,
        }).await.unwrap();
        assert_eq!(disabled.base.version, 1);
        assert!(!disabled.is_active(), "disabled rule is inert");
    }
}
