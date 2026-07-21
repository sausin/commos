//! Messaging (control plane) — creates the messaging workload's entities and emits their
//! canonical events (`workloads.md` §2.3: `ChannelCreated` / `ThreadOpened` / `MessageSent`).
//!
//! This is the messaging *peer* of [`crate::control::routing::Routing`] on the same
//! substrate (CMOS-02-DOM-100): the same commit-entity-with-its-event-atomically spine,
//! proving the platform is workload-general, not voice-only. Scope here is CREATE + READ
//! (MVP) — no state transitions.

use std::sync::Arc;

use commos_core::common::Uuid;
use commos_core::entities::channel::{Channel, ChannelKind};
use commos_core::entities::message::Message;
use commos_core::entities::thread::Thread;
use commos_core::event::{Correlation, Envelope};
use commos_core::events::channel_created::ChannelCreated;
use commos_core::events::message_sent::MessageSent;
use commos_core::events::thread_opened::ThreadOpened;

use crate::relay::RelaySignal;
use crate::store::{Store, StoreError, Tx};

/// The Messaging service. Stateless between requests — all state lives in the [`Store`]
/// (CMOS-03-ARCH-010), so any node can serve any request.
#[derive(Clone)]
pub struct MessagingService {
    store: Arc<dyn Store>,
    signal: RelaySignal,
}

impl MessagingService {
    pub fn new(store: Arc<dyn Store>, signal: RelaySignal) -> Self {
        MessagingService { store, signal }
    }

    /// Create a Channel in `ACTIVE`, emit `ChannelCreated`, commit both atomically.
    pub async fn create_channel(
        &self,
        tenant: Uuid,
        kind: ChannelKind,
        name: Option<String>,
        members: Vec<String>,
    ) -> Result<Channel, StoreError> {
        let mut channel = Channel::create(tenant, kind);
        channel.name = name;
        channel.members = members;

        // Fresh correlation chain rooted at this Channel's creation.
        let ctx = Correlation::root(tenant);
        let payload = ChannelCreated {
            channel_id: channel.base.id,
            kind: channel.kind,
        };
        let idem = format!("{}:ChannelCreated", channel.base.id);
        let envelope = Envelope::new(payload, &ctx, idem);

        self.store
            .commit(Tx {
                channels: vec![channel.clone()],
                events: vec![envelope.to_json()],
                ..Default::default()
            })
            .await?;
        self.signal.wake();

        Ok(channel)
    }

    /// Open a Thread within a Channel, emit `ThreadOpened`, commit both atomically.
    pub async fn open_thread(
        &self,
        tenant: Uuid,
        channel_id: Uuid,
        subject: Option<String>,
    ) -> Result<Thread, StoreError> {
        let mut thread = Thread::open(tenant, channel_id);
        thread.subject = subject;

        let ctx = Correlation::root(tenant);
        let payload = ThreadOpened {
            thread_id: thread.base.id,
            channel_id: thread.channel_id,
        };
        let idem = format!("{}:ThreadOpened", thread.base.id);
        let envelope = Envelope::new(payload, &ctx, idem);

        self.store
            .commit(Tx {
                threads: vec![thread.clone()],
                events: vec![envelope.to_json()],
                ..Default::default()
            })
            .await?;
        self.signal.wake();

        Ok(thread)
    }

    /// Accept a Message for transport in `SENT`, emit `MessageSent`, commit both atomically.
    pub async fn send_message(
        &self,
        tenant: Uuid,
        channel_id: Uuid,
        thread_id: Option<Uuid>,
        sender_ref: String,
        body: Option<String>,
    ) -> Result<Message, StoreError> {
        let mut message = Message::send(tenant, channel_id, sender_ref);
        message.thread_id = thread_id;
        message.body = body;

        let ctx = Correlation::root(tenant);
        let payload = MessageSent {
            message_id: message.base.id,
            channel_id: message.channel_id,
            sender_ref: message.sender_ref.clone(),
        };
        let idem = format!("{}:MessageSent", message.base.id);
        let envelope = Envelope::new(payload, &ctx, idem);

        self.store
            .commit(Tx {
                messages: vec![message.clone()],
                events: vec![envelope.to_json()],
                ..Default::default()
            })
            .await?;
        self.signal.wake();

        Ok(message)
    }
}
