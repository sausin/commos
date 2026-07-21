//! IVR (routing control plane) — CRUD for interactive-voice-response menu nodes.
//!
//! An IVR is *configuration*, not an occurrence: like Queue/Extension it carries no lifecycle
//! state machine and has no event in the frozen catalogue, so create/update/delete persist
//! the entity with an empty `events` vec (no `IvrCreated` exists). The menu *runtime* (prompt
//! playback + DTMF collection) is media-plane work; this service owns the durable definition.

use std::sync::Arc;

use commos_core::common::Uuid;
use commos_core::entities::ivr::Ivr;

use crate::relay::RelaySignal;
use crate::store::{Page, Store, StoreError, Tx};

#[derive(Debug, thiserror::Error)]
pub enum IvrError {
    #[error("ivr not found")]
    NotFound,
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// A partial update to an IVR (each `Some` field replaces the stored value).
#[derive(Default)]
pub struct IvrPatch {
    pub prompt_object_id: Option<Uuid>,
    pub options: Option<serde_json::Value>,
    pub timeout_ms: Option<i64>,
    pub invalid_action: Option<String>,
}

/// The IVR service. Stateless between requests — all state lives in the [`Store`].
#[derive(Clone)]
pub struct IvrService {
    store: Arc<dyn Store>,
    signal: RelaySignal,
}

impl IvrService {
    pub fn new(store: Arc<dyn Store>, signal: RelaySignal) -> Self {
        IvrService { store, signal }
    }

    /// Create an IVR from its `options` (digit map) and optional prompt/tuning fields.
    /// Config, not occurrence: persisted WITHOUT an event.
    pub async fn create(
        &self,
        tenant: Uuid,
        options: serde_json::Value,
        prompt_object_id: Option<Uuid>,
        timeout_ms: Option<i64>,
        invalid_action: Option<String>,
    ) -> Result<Ivr, IvrError> {
        let mut ivr = Ivr::new(tenant, options);
        ivr.prompt_object_id = prompt_object_id;
        ivr.timeout_ms = timeout_ms;
        ivr.invalid_action = invalid_action;
        self.store
            .commit(Tx { ivrs: vec![ivr.clone()], ..Default::default() })
            .await?;
        self.signal.wake();
        Ok(ivr)
    }

    pub async fn get(&self, tenant: Uuid, id: Uuid) -> Result<Ivr, IvrError> {
        self.store.get_ivr(tenant, id).await?.ok_or(IvrError::NotFound)
    }

    pub async fn list(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<Ivr>, IvrError> {
        Ok(self.store.list_ivrs(tenant, limit, cursor).await?)
    }

    /// Apply a partial update and version the entity forward.
    pub async fn update(&self, tenant: Uuid, id: Uuid, patch: IvrPatch) -> Result<Ivr, IvrError> {
        let mut ivr = self.get(tenant, id).await?;
        if let Some(p) = patch.prompt_object_id {
            ivr.prompt_object_id = Some(p);
        }
        if let Some(o) = patch.options {
            ivr.options = o;
        }
        if let Some(t) = patch.timeout_ms {
            ivr.timeout_ms = Some(t);
        }
        if let Some(a) = patch.invalid_action {
            ivr.invalid_action = Some(a);
        }
        ivr.base.touch();
        self.store
            .commit(Tx { ivrs: vec![ivr.clone()], ..Default::default() })
            .await?;
        self.signal.wake();
        Ok(ivr)
    }

    /// Hard-delete an IVR (config carries no lifecycle/audit history). Returns whether one was
    /// removed, or `NotFound` if the id did not exist for this tenant.
    pub async fn delete(&self, tenant: Uuid, id: Uuid) -> Result<(), IvrError> {
        if self.store.delete_ivr(tenant, id).await? {
            Ok(())
        } else {
            Err(IvrError::NotFound)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemStore;

    fn svc() -> IvrService {
        IvrService::new(Arc::new(MemStore::new()), RelaySignal::new())
    }

    #[tokio::test]
    async fn create_get_update_delete_no_events() {
        let s = svc();
        let t = Uuid::now_v7();
        let ivr = s
            .create(t, serde_json::json!({"1": "queue:sales"}), None, Some(5000), Some("repeat".into()))
            .await
            .unwrap();
        assert_eq!(ivr.timeout_ms, Some(5000));
        assert_eq!(ivr.base.version, 0);

        let updated = s
            .update(
                t,
                ivr.base.id,
                IvrPatch { options: Some(serde_json::json!({"1": "voicemail"})), ..Default::default() },
            )
            .await
            .unwrap();
        assert_eq!(updated.base.version, 1);
        assert_eq!(updated.options["1"], "voicemail");

        assert_eq!(s.list(t, 50, None).await.unwrap().items.len(), 1);
        s.delete(t, ivr.base.id).await.unwrap();
        assert!(matches!(s.get(t, ivr.base.id).await, Err(IvrError::NotFound)));
        assert!(matches!(s.delete(t, ivr.base.id).await, Err(IvrError::NotFound)));
    }

    #[tokio::test]
    async fn reads_are_tenant_scoped() {
        let s = svc();
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let ivr = s.create(a, serde_json::json!({}), None, None, None).await.unwrap();
        assert!(matches!(s.get(b, ivr.base.id).await, Err(IvrError::NotFound)));
    }
}
