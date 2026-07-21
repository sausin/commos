//! Object service (control plane) — stores a blob through the [`ObjectStore`] abstraction and
//! records its [`Object`] metadata in the durable [`Store`], atomically from the caller's
//! view. This is the substrate recordings, voicemail, faxes, firmware, transcripts, exports,
//! and diagnostic bundles are built on (Volume 3 §Object Storage; ADR-0008).

use std::sync::Arc;

use commos_core::common::Uuid;
use commos_core::entities::object::{Object, ObjectKind};
use sha2::{Digest, Sha256};

use crate::objectstore::{ObjectStore, ObjectStoreError};
use crate::relay::RelaySignal;
use crate::store::{Page, Store, StoreError, Tx};

#[derive(Debug, thiserror::Error)]
pub enum ObjectError {
    #[error("object not found")]
    NotFound,
    #[error("object storage error: {0}")]
    Blob(#[from] ObjectStoreError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The Object service. Holds a blob backend and the metadata store.
#[derive(Clone)]
pub struct ObjectService {
    blob: Arc<dyn ObjectStore>,
    store: Arc<dyn Store>,
    signal: RelaySignal,
}

/// Lowercase hex of a byte slice.
fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

impl ObjectService {
    pub fn new(blob: Arc<dyn ObjectStore>, store: Arc<dyn Store>, signal: RelaySignal) -> Self {
        ObjectService { blob, store, signal }
    }

    /// Store `bytes`: write the blob, hash it, and commit the Object metadata. The blob is
    /// keyed by the Object's own id, so metadata and bytes always agree.
    pub async fn put(
        &self,
        tenant: Uuid,
        kind: ObjectKind,
        content_type: Option<String>,
        bytes: &[u8],
    ) -> Result<Object, ObjectError> {
        let sha = hex(&Sha256::digest(bytes));
        // Build the metadata first so its id keys the blob; fill the uri once stored.
        let mut obj = Object::new(tenant, kind, String::new(), bytes.len() as u64, sha);
        obj.content_type = content_type;
        let uri = self.blob.put(tenant, obj.base.id, bytes).await?;
        obj.uri = uri;

        self.store
            .commit(Tx { objects: vec![obj.clone()], ..Default::default() })
            .await?;
        self.signal.wake();
        Ok(obj)
    }

    /// Fetch an Object's metadata.
    pub async fn get(&self, tenant: Uuid, id: Uuid) -> Result<Object, ObjectError> {
        self.store.get_object(tenant, id).await?.ok_or(ObjectError::NotFound)
    }

    /// Fetch an Object's metadata **and** its bytes.
    pub async fn get_bytes(&self, tenant: Uuid, id: Uuid) -> Result<(Object, Vec<u8>), ObjectError> {
        let obj = self.get(tenant, id).await?;
        let bytes = self.blob.get(tenant, &obj.uri).await?;
        Ok((obj, bytes))
    }

    pub async fn list(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<Object>, ObjectError> {
        Ok(self.store.list_objects(tenant, limit, cursor).await?)
    }

    /// Delete an Object: remove the blob, then its metadata.
    pub async fn delete(&self, tenant: Uuid, id: Uuid) -> Result<(), ObjectError> {
        let obj = self.get(tenant, id).await?;
        self.blob.delete(tenant, &obj.uri).await?;
        self.store.delete_object(tenant, id).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objectstore::LocalObjectStore;
    use crate::store::MemStore;

    fn svc() -> ObjectService {
        let root = std::env::temp_dir().join(format!("commos-objsvc-{}", Uuid::now_v7()));
        let blob: Arc<dyn ObjectStore> = Arc::new(LocalObjectStore::new(root));
        let store: Arc<dyn Store> = Arc::new(MemStore::new());
        ObjectService::new(blob, store, RelaySignal::new())
    }

    #[tokio::test]
    async fn put_get_list_delete() {
        let s = svc();
        let t = Uuid::now_v7();
        let o = s
            .put(t, ObjectKind::Recording, Some("audio/wav".into()), b"RIFF....")
            .await
            .unwrap();
        assert_eq!(o.bytes, 8);
        assert!(o.uri.starts_with("local://"));
        // sha256 is the hex digest of the content.
        assert_eq!(o.sha256, hex(&Sha256::digest(b"RIFF....")));

        let (meta, bytes) = s.get_bytes(t, o.base.id).await.unwrap();
        assert_eq!(meta.base.id, o.base.id);
        assert_eq!(bytes, b"RIFF....");

        assert_eq!(s.list(t, 50, None).await.unwrap().items.len(), 1);

        s.delete(t, o.base.id).await.unwrap();
        assert!(matches!(s.get(t, o.base.id).await, Err(ObjectError::NotFound)));
        assert!(s.list(t, 50, None).await.unwrap().items.is_empty());
    }

    #[tokio::test]
    async fn reads_are_tenant_scoped() {
        let s = svc();
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let o = s.put(a, ObjectKind::Export, None, b"data").await.unwrap();
        assert!(matches!(s.get(b, o.base.id).await, Err(ObjectError::NotFound)));
    }
}
