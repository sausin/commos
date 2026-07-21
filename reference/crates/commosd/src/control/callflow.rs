//! CallFlow (routing control plane) — create, edit, and **version** routing programs.
//!
//! Publishing a CallFlow snapshots its draft `graph` into an immutable
//! [`CallFlowRevision`] and emits `CallFlowPublished` (Volume 5; Routing is the emitting
//! subsystem). Rollback republishes a prior revision's graph as a *new* revision — history is
//! append-only and never mutated (CMOS-00-ENG-012; Volume 2 §CallFlow Time Machine). Every
//! publish/rollback is one atomic [`Tx`]: the mutated CallFlow, the new immutable revision,
//! and the `CallFlowPublished` event land together.

use std::sync::Arc;

use commos_core::common::Uuid;
use commos_core::entities::call_flow::{CallFlow, CallFlowRevision};
use commos_core::event::{Correlation, Envelope};
use commos_core::events::call_flow_published::CallFlowPublished;

use crate::relay::RelaySignal;
use crate::store::{Page, Store, StoreError, Tx};

#[derive(Debug, thiserror::Error)]
pub enum CallFlowError {
    #[error("call flow not found")]
    NotFound,
    #[error("no such published revision")]
    RevisionNotFound,
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The CallFlow service. Stateless between requests — all state lives in the [`Store`].
#[derive(Clone)]
pub struct CallFlowService {
    store: Arc<dyn Store>,
    signal: RelaySignal,
}

impl CallFlowService {
    pub fn new(store: Arc<dyn Store>, signal: RelaySignal) -> Self {
        CallFlowService { store, signal }
    }

    /// Create a DRAFT CallFlow with an optional initial `graph` (defaults to empty).
    ///
    /// No event on create — a DRAFT is configuration, not a published occurrence; the only
    /// CallFlow event in the frozen catalogue is `CallFlowPublished`, emitted on publish.
    pub async fn create(
        &self,
        tenant: Uuid,
        name: impl Into<String>,
        graph: Option<serde_json::Value>,
    ) -> Result<CallFlow, CallFlowError> {
        let mut cf = CallFlow::new(tenant, name);
        if let Some(g) = graph {
            cf.graph = g;
        }
        self.store
            .commit(Tx { call_flows: vec![cf.clone()], ..Default::default() })
            .await?;
        self.signal.wake();
        Ok(cf)
    }

    pub async fn get(&self, tenant: Uuid, id: Uuid) -> Result<CallFlow, CallFlowError> {
        self.store.get_call_flow(tenant, id).await?.ok_or(CallFlowError::NotFound)
    }

    pub async fn list(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<CallFlow>, CallFlowError> {
        Ok(self.store.list_call_flows(tenant, limit, cursor).await?)
    }

    /// The append-only publish history of a CallFlow (revisions ascending by version).
    pub async fn revisions(
        &self,
        tenant: Uuid,
        id: Uuid,
    ) -> Result<Vec<CallFlowRevision>, CallFlowError> {
        // Surface NotFound distinctly from an empty (never-published) history.
        self.get(tenant, id).await?;
        Ok(self.store.list_call_flow_revisions(tenant, id).await?)
    }

    /// Edit a DRAFT's `name` and/or `graph`. Any change returns the flow to `DRAFT`
    /// (unpublished edits) and versions the entity forward.
    pub async fn edit(
        &self,
        tenant: Uuid,
        id: Uuid,
        name: Option<String>,
        graph: Option<serde_json::Value>,
    ) -> Result<CallFlow, CallFlowError> {
        let mut cf = self.get(tenant, id).await?;
        if let Some(n) = name {
            cf.name = n;
        }
        match graph {
            // set_graph touches the version and resets state to DRAFT (unpublished edits).
            Some(g) => cf.set_graph(g),
            // A name-only (or empty) edit still versions the entity forward.
            None => cf.base.touch(),
        }
        self.store
            .commit(Tx { call_flows: vec![cf.clone()], ..Default::default() })
            .await?;
        self.signal.wake();
        Ok(cf)
    }

    /// Publish the current draft: snapshot its `graph` as a new immutable revision, advance
    /// the state machine, and emit `CallFlowPublished` — all atomically.
    pub async fn publish(&self, tenant: Uuid, id: Uuid) -> Result<CallFlow, CallFlowError> {
        let mut cf = self.get(tenant, id).await?;
        let version = cf.mark_published();
        let revision = CallFlowRevision::new(tenant, cf.base.id, version, cf.graph.clone());
        self.commit_publish(tenant, cf.clone(), revision, version).await?;
        Ok(cf)
    }

    /// Roll back to a prior published `version`: republish that revision's graph as a *new*
    /// revision, return the flow to `PUBLISHED`, and emit `CallFlowPublished`. The target
    /// revision is left untouched (append-only history).
    pub async fn rollback(
        &self,
        tenant: Uuid,
        id: Uuid,
        target_version: u64,
    ) -> Result<CallFlow, CallFlowError> {
        let mut cf = self.get(tenant, id).await?;
        let target = self
            .store
            .get_call_flow_revision(tenant, id, target_version)
            .await?
            .ok_or(CallFlowError::RevisionNotFound)?;
        let new_version = cf.mark_rolled_back(target.graph.clone());
        let revision = CallFlowRevision::new(tenant, cf.base.id, new_version, cf.graph.clone());
        self.commit_publish(tenant, cf.clone(), revision, new_version).await?;
        Ok(cf)
    }

    /// Commit a publish/rollback: the mutated CallFlow, the new immutable revision, and the
    /// `CallFlowPublished` event, atomically. The event's idempotency key is deterministic per
    /// (flow, version) so redelivery dedupes.
    async fn commit_publish(
        &self,
        tenant: Uuid,
        cf: CallFlow,
        revision: CallFlowRevision,
        version: u64,
    ) -> Result<(), CallFlowError> {
        let ctx = Correlation::root(tenant);
        let ev = Envelope::new(
            CallFlowPublished { call_flow_id: cf.base.id, published_version: version },
            &ctx,
            format!("{}:CallFlowPublished:{}", cf.base.id, version),
        )
        .to_json();
        self.store
            .commit(Tx {
                call_flows: vec![cf],
                call_flow_revisions: vec![revision],
                events: vec![ev],
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
    use crate::store::MemStore;
    use commos_core::entities::call_flow::CallFlowState;

    fn svc() -> (CallFlowService, Arc<dyn Store>) {
        let store: Arc<dyn Store> = Arc::new(MemStore::new());
        (CallFlowService::new(store.clone(), RelaySignal::new()), store)
    }

    #[tokio::test]
    async fn publish_snapshots_graph_and_emits_event() {
        let (svc, store) = svc();
        let t = Uuid::now_v7();
        let cf = svc
            .create(t, "Main", Some(serde_json::json!({"nodes": [{"id": "a"}]})))
            .await
            .unwrap();
        // No event on create.
        assert_eq!(store.peek_outbox(10).await.unwrap().len(), 0);

        let published = svc.publish(t, cf.base.id).await.unwrap();
        assert_eq!(published.state, CallFlowState::Published);
        assert_eq!(published.published_version, Some(1));

        // Exactly one CallFlowPublished event, and one immutable revision holding the graph.
        let outbox = store.peek_outbox(10).await.unwrap();
        assert_eq!(outbox.len(), 1);
        assert_eq!(outbox[0].event["type"], "CallFlowPublished");
        assert_eq!(outbox[0].event["data"]["published_version"], 1);
        let revs = svc.revisions(t, cf.base.id).await.unwrap();
        assert_eq!(revs.len(), 1);
        assert_eq!(revs[0].graph["nodes"][0]["id"], "a");
    }

    #[tokio::test]
    async fn publish_newer_then_rollback_republishes_prior_graph() {
        let (svc, _store) = svc();
        let t = Uuid::now_v7();
        let cf = svc.create(t, "Flow", Some(serde_json::json!({"v": 1}))).await.unwrap();
        let id = cf.base.id;

        svc.publish(t, id).await.unwrap(); // revision 1: {"v":1}
        svc.edit(t, id, None, Some(serde_json::json!({"v": 2}))).await.unwrap();
        let superseded = svc.publish(t, id).await.unwrap(); // revision 2: {"v":2}
        assert_eq!(superseded.state, CallFlowState::Superseded);
        assert_eq!(superseded.published_version, Some(2));

        // Roll back to revision 1 → republished as revision 3 with v1's graph, PUBLISHED.
        let rolled = svc.rollback(t, id, 1).await.unwrap();
        assert_eq!(rolled.state, CallFlowState::Published);
        assert_eq!(rolled.published_version, Some(3));
        assert_eq!(rolled.graph, serde_json::json!({"v": 1}));

        // History is append-only: three immutable revisions, originals intact.
        let revs = svc.revisions(t, id).await.unwrap();
        assert_eq!(revs.len(), 3);
        assert_eq!(revs[0].graph, serde_json::json!({"v": 1}));
        assert_eq!(revs[1].graph, serde_json::json!({"v": 2}));
        assert_eq!(revs[2].graph, serde_json::json!({"v": 1}));
    }

    #[tokio::test]
    async fn rollback_to_missing_revision_errors() {
        let (svc, _store) = svc();
        let t = Uuid::now_v7();
        let cf = svc.create(t, "Flow", None).await.unwrap();
        svc.publish(t, cf.base.id).await.unwrap();
        assert!(matches!(
            svc.rollback(t, cf.base.id, 99).await,
            Err(CallFlowError::RevisionNotFound)
        ));
    }

    #[tokio::test]
    async fn missing_flow_is_not_found_and_reads_are_tenant_scoped() {
        let (svc, _store) = svc();
        let t = Uuid::now_v7();
        assert!(matches!(svc.get(t, Uuid::now_v7()).await, Err(CallFlowError::NotFound)));

        let cf = svc.create(t, "Flow", None).await.unwrap();
        let other = Uuid::now_v7();
        assert!(matches!(svc.get(other, cf.base.id).await, Err(CallFlowError::NotFound)));
    }
}
