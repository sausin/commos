//! Call recording (control plane) — turns captured RTP audio into a durable, queryable
//! [`Recording`] (Volume 7 recording; CMOS-02-DOM-013).
//!
//! Recording stores the media **as-is** — the negotiated payload (G.711 / PCMU) is written
//! byte-for-byte with **no transcoding** (the pure-Rust, no-codec-libs posture; the consumer
//! decodes, e.g. in the browser). The raw μ-law stream is stored as an `audio/basic` Object
//! (RFC 2046: 8 kHz mono μ-law); the [`Recording`] links it back to its [`Call`].

use std::sync::Arc;

use commos_core::common::Uuid;
use commos_core::entities::object::ObjectKind;
use commos_core::entities::recording::Recording;
use commos_core::event::{Correlation, Envelope};
use commos_core::events::recording_uploaded::RecordingUploaded;

use crate::control::objects::{ObjectError, ObjectService};
use crate::relay::RelaySignal;
use crate::store::{Page, Store, StoreError, Tx};

/// G.711 (PCMU/PCMA) is 8 kHz mono, one byte per sample → 8000 bytes per second. Used to
/// derive a recording's duration from its byte length.
const G711_BYTES_PER_SEC: u64 = 8000;

#[derive(Debug, thiserror::Error)]
pub enum RecordingError {
    #[error("recording not found")]
    NotFound,
    #[error(transparent)]
    Object(#[from] ObjectError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The Recording service. Holds the object service (for the blob) and the store (metadata).
#[derive(Clone)]
pub struct RecordingService {
    objects: ObjectService,
    store: Arc<dyn Store>,
    signal: RelaySignal,
}

impl RecordingService {
    pub fn new(objects: ObjectService, store: Arc<dyn Store>, signal: RelaySignal) -> Self {
        RecordingService { objects, store, signal }
    }

    /// Store captured audio for `call_id`: write the bytes as an `audio/basic` Object, link it
    /// with a [`Recording`], and emit `RecordingUploaded` — all durable.
    pub async fn save(
        &self,
        tenant: Uuid,
        call_id: Uuid,
        audio: &[u8],
    ) -> Result<Recording, RecordingError> {
        // The captured payload is stored verbatim (no transcode). audio/basic == G.711 μ-law.
        let obj = self
            .objects
            .put(tenant, ObjectKind::Recording, Some("audio/basic".into()), audio)
            .await?;

        let mut rec = Recording::new(tenant, obj.base.id);
        rec.call_id = Some(call_id);
        rec.bytes = Some(obj.bytes);
        rec.duration_ms = Some(obj.bytes * 1000 / G711_BYTES_PER_SEC);

        let ctx = Correlation::root(tenant);
        let ev = Envelope::new(
            RecordingUploaded {
                recording_id: rec.base.id,
                call_id,
                object_id: obj.base.id,
                object_uri: Some(obj.uri.clone()),
                bytes: Some(obj.bytes),
            },
            &ctx,
            format!("{}:RecordingUploaded", rec.base.id),
        )
        .to_json();

        self.store
            .commit(Tx { recordings: vec![rec.clone()], events: vec![ev], ..Default::default() })
            .await?;
        self.signal.wake();
        Ok(rec)
    }

    pub async fn get(&self, tenant: Uuid, id: Uuid) -> Result<Recording, RecordingError> {
        self.store.get_recording(tenant, id).await?.ok_or(RecordingError::NotFound)
    }

    pub async fn list(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<Recording>, RecordingError> {
        Ok(self.store.list_recordings(tenant, limit, cursor).await?)
    }

    /// Fetch a recording's metadata and its audio bytes (from the linked Object).
    pub async fn get_audio(
        &self,
        tenant: Uuid,
        id: Uuid,
    ) -> Result<(Recording, Vec<u8>), RecordingError> {
        let rec = self.get(tenant, id).await?;
        let (_obj, bytes) = self.objects.get_bytes(tenant, rec.object_id).await?;
        Ok((rec, bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objectstore::{LocalObjectStore, ObjectStore};
    use crate::store::MemStore;

    fn svc() -> (RecordingService, Arc<dyn Store>) {
        let root = std::env::temp_dir().join(format!("commos-rec-{}", Uuid::now_v7()));
        let blob: Arc<dyn ObjectStore> = Arc::new(LocalObjectStore::new(root));
        let store: Arc<dyn Store> = Arc::new(MemStore::new());
        let objects = ObjectService::new(blob, store.clone(), RelaySignal::new());
        (RecordingService::new(objects, store.clone(), RelaySignal::new()), store)
    }

    #[tokio::test]
    async fn save_links_call_object_and_derives_duration() {
        let (rec_svc, store) = svc();
        let t = Uuid::now_v7();
        let call = Uuid::now_v7();
        // 16000 bytes of "audio" → 2 seconds of G.711.
        let audio = vec![0x7fu8; 16000];
        let rec = rec_svc.save(t, call, &audio).await.unwrap();
        assert_eq!(rec.call_id, Some(call));
        assert_eq!(rec.bytes, Some(16000));
        assert_eq!(rec.duration_ms, Some(2000));

        // The audio round-trips through the object store.
        let (_r, bytes) = rec_svc.get_audio(t, rec.base.id).await.unwrap();
        assert_eq!(bytes.len(), 16000);
        assert_eq!(rec_svc.list(t, 50, None).await.unwrap().items.len(), 1);
        // A RecordingUploaded event was queued.
        assert_eq!(store.peek_outbox(10).await.unwrap().len(), 1);
    }
}
