//! Voicemail (control plane) — turns a caller's captured RTP audio, left when a callee did
//! not answer, into a durable, queryable [`Voicemail`] (Volume 7; CMOS-02-DOM-013,
//! CMOS-07-SIP-044).
//!
//! Like [`RecordingService`](super::recordings::RecordingService), the media is stored
//! **as-is** — the negotiated payload (G.711 / PCMU) written byte-for-byte with **no
//! transcoding** (the pure-Rust, no-codec-libs posture; the consumer decodes). The raw μ-law
//! stream is stored as an `audio/basic` Object of `ObjectKind::Voicemail`; the [`Voicemail`]
//! links it to the mailbox owner and its originating [`Call`], and drives the
//! message-waiting indicator (MWI) the SIP plane pushes to the phone.

use std::sync::Arc;

use commos_core::common::Uuid;
use commos_core::entities::object::ObjectKind;
use commos_core::entities::voicemail::Voicemail;
use commos_core::event::{Correlation, Envelope};
use commos_core::events::voicemail_received::VoicemailReceived;

use crate::control::objects::{ObjectError, ObjectService};
use crate::relay::RelaySignal;
use crate::store::{Page, Store, StoreError, Tx};

/// G.711 (PCMU/PCMA) is 8 kHz mono, one byte per sample → 8000 bytes per second. Used to
/// derive a voicemail's duration from its byte length.
const G711_BYTES_PER_SEC: u64 = 8000;

#[derive(Debug, thiserror::Error)]
pub enum VoicemailError {
    #[error("voicemail not found")]
    NotFound,
    #[error(transparent)]
    Object(#[from] ObjectError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The Voicemail service. Holds the object service (for the blob) and the store (metadata).
#[derive(Clone)]
pub struct VoicemailService {
    objects: ObjectService,
    store: Arc<dyn Store>,
    signal: RelaySignal,
}

impl VoicemailService {
    pub fn new(objects: ObjectService, store: Arc<dyn Store>, signal: RelaySignal) -> Self {
        VoicemailService { objects, store, signal }
    }

    /// Store captured audio as a voicemail for `user_id`'s mailbox: write the bytes as an
    /// `audio/basic` Object of kind `VOICEMAIL`, link it with an unread [`Voicemail`], and emit
    /// `VoicemailReceived` — all durable.
    pub async fn save(
        &self,
        tenant: Uuid,
        call_id: Uuid,
        user_id: Option<Uuid>,
        audio: &[u8],
    ) -> Result<Voicemail, VoicemailError> {
        // The captured payload is stored verbatim (no transcode). audio/basic == G.711 μ-law.
        let obj = self
            .objects
            .put(tenant, ObjectKind::Voicemail, Some("audio/basic".into()), audio)
            .await?;

        let mut vm = Voicemail::new(tenant, obj.base.id);
        vm.user_id = user_id;
        vm.call_id = Some(call_id);
        vm.duration_ms = Some(obj.bytes * 1000 / G711_BYTES_PER_SEC);

        let ctx = Correlation::root(tenant);
        let ev = Envelope::new(
            VoicemailReceived { voicemail_id: vm.base.id, user_id, object_id: obj.base.id },
            &ctx,
            format!("{}:VoicemailReceived", vm.base.id),
        )
        .to_json();

        self.store
            .commit(Tx { voicemails: vec![vm.clone()], events: vec![ev], ..Default::default() })
            .await?;
        self.signal.wake();
        Ok(vm)
    }

    pub async fn get(&self, tenant: Uuid, id: Uuid) -> Result<Voicemail, VoicemailError> {
        self.store.get_voicemail(tenant, id).await?.ok_or(VoicemailError::NotFound)
    }

    pub async fn list(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<Voicemail>, VoicemailError> {
        Ok(self.store.list_voicemails(tenant, limit, cursor).await?)
    }

    /// Fetch a voicemail's metadata and its audio bytes (from the linked Object).
    pub async fn get_audio(
        &self,
        tenant: Uuid,
        id: Uuid,
    ) -> Result<(Voicemail, Vec<u8>), VoicemailError> {
        let vm = self.get(tenant, id).await?;
        let (_obj, bytes) = self.objects.get_bytes(tenant, vm.object_id).await?;
        Ok((vm, bytes))
    }

    /// Mark a voicemail read (the mailbox owner has listened to it). Idempotent: an
    /// already-read voicemail is returned unchanged with no write (SD-card longevity).
    pub async fn mark_read(&self, tenant: Uuid, id: Uuid) -> Result<Voicemail, VoicemailError> {
        let mut vm = self.get(tenant, id).await?;
        if vm.mark_read() {
            self.store
                .commit(Tx { voicemails: vec![vm.clone()], ..Default::default() })
                .await?;
            self.signal.wake();
        }
        Ok(vm)
    }

    /// Summarise a mailbox for the message-waiting indicator (MWI): `(new, old)` = unread and
    /// read voicemail counts for the extension `number` (drives `Voice-Message: new/old`).
    ///
    /// The mailbox is identified by the **originating Call's `to_ref`** (the dialled callee),
    /// matched on its SIP user-part — the reliable association available on the SIP path, where
    /// an inbound INVITE names the callee but not a CommOS `User`. Pages the tenant's voicemails
    /// and, for each, checks the Call it came from. The working set on a single hub is small, so
    /// a paged scan with per-voicemail Call lookups is fine (there is no per-mailbox index in
    /// the reference store).
    pub async fn mailbox_summary(
        &self,
        tenant: Uuid,
        number: &str,
    ) -> Result<(u32, u32), VoicemailError> {
        let mut cursor = None;
        let (mut new, mut old) = (0u32, 0u32);
        loop {
            let page = self.store.list_voicemails(tenant, 200, cursor).await?;
            for vm in &page.items {
                let Some(call_id) = vm.call_id else { continue };
                let Some(call) = self.store.get_call(tenant, call_id).await? else { continue };
                if user_part(&call.to_ref).is_some_and(|u| u.eq_ignore_ascii_case(number)) {
                    if vm.read {
                        old += 1;
                    } else {
                        new += 1;
                    }
                }
            }
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        Ok((new, old))
    }
}

/// The user-part of a SIP URI: `sip:200@example.com` → `200`. Tolerates a leading `<` and the
/// `sip:`/`sips:`/`tel:` schemes; returns `None` for a domain-only URI (no `@`).
fn user_part(uri: &str) -> Option<&str> {
    let s = uri
        .trim()
        .trim_start_matches('<')
        .trim_start_matches("sips:")
        .trim_start_matches("sip:")
        .trim_start_matches("tel:");
    let user = s.split_once('@')?.0.trim();
    (!user.is_empty()).then_some(user)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objectstore::{LocalObjectStore, ObjectStore};
    use crate::store::MemStore;

    fn svc() -> (VoicemailService, Arc<dyn Store>) {
        let root = std::env::temp_dir().join(format!("commos-vm-{}", Uuid::now_v7()));
        let blob: Arc<dyn ObjectStore> = Arc::new(LocalObjectStore::new(root));
        let store: Arc<dyn Store> = Arc::new(MemStore::new());
        let objects = ObjectService::new(blob, store.clone(), RelaySignal::new());
        (VoicemailService::new(objects, store.clone(), RelaySignal::new()), store)
    }

    #[tokio::test]
    async fn save_links_mailbox_object_and_derives_duration() {
        let (vm_svc, store) = svc();
        let t = Uuid::now_v7();
        let call = Uuid::now_v7();
        let user = Uuid::now_v7();
        // 16000 bytes of "audio" → 2 seconds of G.711.
        let audio = vec![0x7fu8; 16000];
        let vm = vm_svc.save(t, call, Some(user), &audio).await.unwrap();
        assert_eq!(vm.call_id, Some(call));
        assert_eq!(vm.user_id, Some(user));
        assert_eq!(vm.duration_ms, Some(2000));
        assert!(!vm.read);

        // The audio round-trips through the object store.
        let (_v, bytes) = vm_svc.get_audio(t, vm.base.id).await.unwrap();
        assert_eq!(bytes.len(), 16000);
        assert_eq!(vm_svc.list(t, 50, None).await.unwrap().items.len(), 1);
        // A VoicemailReceived event was queued.
        assert_eq!(store.peek_outbox(10).await.unwrap().len(), 1);
    }

    /// Persist an inbound Call to `to` so `mailbox_summary` can match voicemails by the
    /// dialled callee's SIP user-part.
    async fn seed_call(store: &Arc<dyn Store>, tenant: Uuid, to: &str) -> Uuid {
        use commos_core::entities::call::{Call, Direction};
        let call = Call::originate(tenant, Direction::Inbound, "sip:caller@x", to);
        let id = call.base.id;
        store.commit(Tx { calls: vec![call], ..Default::default() }).await.unwrap();
        id
    }

    #[tokio::test]
    async fn mark_read_is_idempotent_and_updates_summary() {
        let (vm_svc, store) = svc();
        let t = Uuid::now_v7();
        let call = seed_call(&store, t, "sip:200@host").await;
        let vm = vm_svc.save(t, call, None, &[0u8; 8000]).await.unwrap();
        assert_eq!(vm_svc.mailbox_summary(t, "200").await.unwrap(), (1, 0));

        let read = vm_svc.mark_read(t, vm.base.id).await.unwrap();
        assert!(read.read);
        assert_eq!(read.base.version, 1);
        assert_eq!(vm_svc.mailbox_summary(t, "200").await.unwrap(), (0, 1));

        // A second mark is a no-op: version stays 1.
        let again = vm_svc.mark_read(t, vm.base.id).await.unwrap();
        assert_eq!(again.base.version, 1);
    }

    #[tokio::test]
    async fn summary_is_scoped_to_the_dialled_mailbox() {
        let (vm_svc, store) = svc();
        let t = Uuid::now_v7();
        let call_200a = seed_call(&store, t, "sip:200@host").await;
        let call_201 = seed_call(&store, t, "sip:201@elsewhere").await;
        let call_200b = seed_call(&store, t, "<sip:200@host:5060>").await;
        vm_svc.save(t, call_200a, None, &[0u8; 8000]).await.unwrap();
        vm_svc.save(t, call_201, None, &[0u8; 8000]).await.unwrap();
        vm_svc.save(t, call_200b, None, &[0u8; 8000]).await.unwrap();
        // Two mailbox-200 voicemails (matched across differing hosts/brackets), one for 201.
        assert_eq!(vm_svc.mailbox_summary(t, "200").await.unwrap(), (2, 0));
        assert_eq!(vm_svc.mailbox_summary(t, "201").await.unwrap(), (1, 0));
        assert_eq!(vm_svc.mailbox_summary(t, "999").await.unwrap(), (0, 0));
    }
}
