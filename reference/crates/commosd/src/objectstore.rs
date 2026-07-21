//! Object Storage abstraction (Volume 3 §Object Storage; ADR-0008) — the pluggable blob
//! backend that recordings, voicemail, faxes, firmware, transcripts, exports, and diagnostic
//! bundles all sit on. The *metadata* (an [`Object`](commos_core::entities::object::Object))
//! lives in the durable [`Store`](crate::store::Store); the *bytes* live here.
//!
//! The reference ships a **local-filesystem** binding — zero external dependency, right for a
//! single box / Raspberry Pi. An S3 / MinIO / R2 / GCS binding slots in behind the same trait
//! without touching a caller (the same drop-in-binding discipline as the `Store`).
//!
//! Blobs are addressed by a `local://<tenant>/<id>` URI. Reads and deletes are **tenant
//! scoped** and the `<id>` must be a UUID, so a crafted URI can neither escape the object
//! root (no `..` traversal) nor reach another tenant's bytes.

use std::path::PathBuf;

use axum::async_trait;

use commos_core::common::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum ObjectStoreError {
    #[error("object not found")]
    NotFound,
    #[error("invalid object uri: {0}")]
    InvalidUri(String),
    #[error("storage backend error: {0}")]
    Backend(String),
}

/// The blob backend. Async because a real backend (S3) is; the local binding satisfies it
/// with `tokio::fs`.
#[async_trait]
pub trait ObjectStore: Send + Sync {
    /// Store `bytes` for `(tenant, id)`; returns the addressable URI to persist on the
    /// Object metadata.
    async fn put(&self, tenant: Uuid, id: Uuid, bytes: &[u8]) -> Result<String, ObjectStoreError>;
    /// Fetch the bytes at `uri`, which must belong to `tenant`.
    async fn get(&self, tenant: Uuid, uri: &str) -> Result<Vec<u8>, ObjectStoreError>;
    /// Delete the bytes at `uri`. Returns whether a blob was removed.
    async fn delete(&self, tenant: Uuid, uri: &str) -> Result<bool, ObjectStoreError>;
}

/// Local-filesystem [`ObjectStore`]: one file per blob under `<root>/<tenant>/<id>`.
pub struct LocalObjectStore {
    root: PathBuf,
}

impl LocalObjectStore {
    /// Objects are stored under `<data_dir>/objects`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        LocalObjectStore { root: root.into() }
    }

    /// Build the `local://<tenant>/<id>` URI for a blob.
    fn uri_for(tenant: Uuid, id: Uuid) -> String {
        format!("local://{tenant}/{id}")
    }

    /// Resolve a `local://<tenant>/<id>` URI to its on-disk path, enforcing tenant scope and
    /// rejecting anything that isn't exactly `tenant`/`uuid` (no traversal, no cross-tenant).
    fn path_for(&self, tenant: Uuid, uri: &str) -> Result<PathBuf, ObjectStoreError> {
        let rest = uri
            .strip_prefix("local://")
            .ok_or_else(|| ObjectStoreError::InvalidUri(uri.to_string()))?;
        let (t, id) = rest
            .split_once('/')
            .ok_or_else(|| ObjectStoreError::InvalidUri(uri.to_string()))?;
        let t = Uuid::parse(t).map_err(|_| ObjectStoreError::InvalidUri(uri.to_string()))?;
        let id = Uuid::parse(id).map_err(|_| ObjectStoreError::InvalidUri(uri.to_string()))?;
        if t != tenant {
            // Belongs to another tenant — treat as absent rather than leak its existence.
            return Err(ObjectStoreError::NotFound);
        }
        Ok(self.root.join(t.to_string()).join(id.to_string()))
    }
}

#[async_trait]
impl ObjectStore for LocalObjectStore {
    async fn put(&self, tenant: Uuid, id: Uuid, bytes: &[u8]) -> Result<String, ObjectStoreError> {
        let dir = self.root.join(tenant.to_string());
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| ObjectStoreError::Backend(e.to_string()))?;
        let path = dir.join(id.to_string());
        tokio::fs::write(&path, bytes)
            .await
            .map_err(|e| ObjectStoreError::Backend(e.to_string()))?;
        Ok(Self::uri_for(tenant, id))
    }

    async fn get(&self, tenant: Uuid, uri: &str) -> Result<Vec<u8>, ObjectStoreError> {
        let path = self.path_for(tenant, uri)?;
        match tokio::fs::read(&path).await {
            Ok(b) => Ok(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(ObjectStoreError::NotFound),
            Err(e) => Err(ObjectStoreError::Backend(e.to_string())),
        }
    }

    async fn delete(&self, tenant: Uuid, uri: &str) -> Result<bool, ObjectStoreError> {
        let path = self.path_for(tenant, uri)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(ObjectStoreError::Backend(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root() -> PathBuf {
        // A unique-enough dir under the system temp; UUIDv7 keeps runs from colliding.
        std::env::temp_dir().join(format!("commos-obj-{}", Uuid::now_v7()))
    }

    #[tokio::test]
    async fn put_get_delete_roundtrip() {
        let store = LocalObjectStore::new(tmp_root());
        let t = Uuid::now_v7();
        let id = Uuid::now_v7();
        let uri = store.put(t, id, b"hello").await.unwrap();
        assert_eq!(uri, format!("local://{t}/{id}"));
        assert_eq!(store.get(t, &uri).await.unwrap(), b"hello");
        assert!(store.delete(t, &uri).await.unwrap());
        assert!(matches!(store.get(t, &uri).await, Err(ObjectStoreError::NotFound)));
        // Deleting a missing blob is a clean `false`, not an error.
        assert!(!store.delete(t, &uri).await.unwrap());
    }

    #[tokio::test]
    async fn cross_tenant_and_traversal_are_rejected() {
        let store = LocalObjectStore::new(tmp_root());
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let id = Uuid::now_v7();
        let uri = store.put(a, id, b"secret").await.unwrap();
        // Tenant B cannot read tenant A's blob.
        assert!(matches!(store.get(b, &uri).await, Err(ObjectStoreError::NotFound)));
        // A non-local / malformed URI is rejected.
        assert!(matches!(
            store.get(a, "file:///etc/passwd").await,
            Err(ObjectStoreError::InvalidUri(_))
        ));
        assert!(matches!(
            store.get(a, "local://not-a-uuid/x").await,
            Err(ObjectStoreError::InvalidUri(_))
        ));
    }
}
