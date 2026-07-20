//! Routing (control plane) — resolves and originates Calls (Volume 3 components.md:
//! "Routing produces `CallStarted`").
//!
//! This is the vertical slice that proves the spine end-to-end: an API command mutates an
//! entity, the mutation and its event are committed atomically to the outbox, the media
//! plane is commanded over the typed boundary, and the event surfaces on the bus.

use std::sync::Arc;

use commos_core::common::Uuid;
use commos_core::entities::call::{Call, CallState, Direction};
use commos_core::event::{Correlation, Envelope, EventPayload};
use commos_core::events::call_answered::CallAnswered;
use commos_core::events::call_ended::CallEnded;
use commos_core::events::call_held::CallHeld;
use commos_core::events::call_resumed::CallResumed;
use commos_core::events::call_ringing::CallRinging;
use commos_core::events::call_started::CallStarted;
use commos_core::events::call_transferred::{CallTransferred, TransferType};

use crate::media::{MediaCommand, MediaFact, MediaPlane};
use crate::relay::RelaySignal;
use crate::store::{Store, StoreError, Tx};

/// Command to originate a Call (projection of `POST /v1/calls`).
pub struct OriginateRequest {
    pub direction: Direction,
    pub from_ref: String,
    pub to_ref: String,
    /// The API `Idempotency-Key` header, if supplied. A repeat returns the same Call.
    pub idempotency_key: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RoutingError {
    #[error("media plane rejected the call: {0}")]
    MediaRejected(String),
    #[error("call not found")]
    NotFound,
    #[error("illegal call action: {0}")]
    IllegalState(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The Routing service. Stateless between requests — all state lives in the [`Store`]
/// (CMOS-03-ARCH-010), so any node can serve any request.
#[derive(Clone)]
pub struct Routing {
    store: Arc<dyn Store>,
    media: Arc<dyn MediaPlane>,
    signal: RelaySignal,
}

impl Routing {
    pub fn new(store: Arc<dyn Store>, media: Arc<dyn MediaPlane>, signal: RelaySignal) -> Self {
        Routing { store, media, signal }
    }

    /// Originate a Call: create it in `INITIATED`, emit `CallStarted`, command media.
    ///
    /// Atomicity: the Call and its `CallStarted` event are written in one [`Tx`]. The media
    /// command is issued after the commit succeeds, so we never signal media for a Call we
    /// failed to persist.
    pub async fn originate(
        &self,
        tenant: Uuid,
        req: OriginateRequest,
    ) -> Result<Call, RoutingError> {
        // Idempotent replay: a repeat of the same key returns the already-created Call.
        if let Some(key) = &req.idempotency_key {
            if let Some(existing_id) = self.store.call_for_idempotency_key(tenant, key).await? {
                if let Some(existing) = self.store.get_call(tenant, existing_id).await? {
                    return Ok(existing);
                }
            }
        }

        let call = Call::originate(tenant, req.direction, &req.from_ref, &req.to_ref);

        // Build the CallStarted envelope. Its correlation_id mirrors the Call's, tying
        // every downstream event into one causal chain (Volume 5 §3).
        let ctx = Correlation {
            tenant_id: tenant,
            correlation_id: call.correlation_id,
            causation_id: None,
            sequence: Some(0),
            traceparent: None,
        };
        let payload = CallStarted {
            call_id: call.base.id,
            direction: call.direction,
            from_ref: call.from_ref.clone(),
            to_ref: call.to_ref.clone(),
            device_id: call.device_id,
        };
        // Deterministic per-event idempotency key so redelivery dedupes (Volume 5).
        let idem = format!("{}:CallStarted", call.base.id);
        let envelope = Envelope::new(payload, &ctx, idem);

        // One transaction: entity + event + idempotency ledger.
        self.store
            .commit(Tx {
                calls: vec![call.clone()],
                events: vec![envelope.to_json()],
                idempotency: req.idempotency_key.map(|k| (tenant, k, call.base.id)),
                ..Default::default()
            })
            .await?;
        // Wake the relay so the event surfaces promptly.
        self.signal.wake();

        // Command the media plane over the typed boundary (control decides, media acts).
        let ack = self.media.dispatch(MediaCommand::Originate {
            tenant_id: tenant,
            call_id: call.base.id,
            from_ref: call.from_ref.clone(),
            to_ref: call.to_ref.clone(),
        });
        if let crate::media::MediaAck::Rejected { reason, .. } = ack {
            return Err(RoutingError::MediaRejected(reason));
        }

        // The Call starts INITIATED. Ring and answer arrive asynchronously as media facts
        // and are applied by `apply_fact` (media→control, CMOS-03-ARCH-003) — not computed
        // here. A client observes the progression via GET or the event stream.
        Ok(call)
    }

    /// Apply a media fact to Call state and emit the corresponding event
    /// (media→control, CMOS-03-ARCH-003). Driven by the fact loop wired in `main`; a real
    /// media node reports these from actual signalling.
    pub async fn apply_fact(&self, fact: MediaFact) -> Result<(), RoutingError> {
        match fact {
            MediaFact::Rang { tenant_id, call_id } => {
                let mut call = self.load(tenant_id, call_id).await?;
                call.transition(CallState::Ringing)
                    .map_err(|e| RoutingError::IllegalState(e.to_string()))?;
                self.commit_event(tenant_id, &call, true, CallRinging { call_id })
                    .await
            }
            MediaFact::Answered { tenant_id, call_id, answered_at } => {
                let mut call = self.load(tenant_id, call_id).await?;
                call.transition(CallState::Answered)
                    .map_err(|e| RoutingError::IllegalState(e.to_string()))?;
                let answered_at = call.answered_at.unwrap_or(answered_at);
                self.commit_event(
                    tenant_id,
                    &call,
                    true,
                    CallAnswered { call_id, identity_id: None, answered_at },
                )
                .await
            }
        }
    }

    /// Load a Call for this tenant or report it missing (tenant-scoped, CMOS-03-ARCH-050).
    async fn load(&self, tenant: Uuid, call_id: Uuid) -> Result<Call, RoutingError> {
        self.store
            .get_call(tenant, call_id)
            .await?
            .ok_or(RoutingError::NotFound)
    }

    /// Emit an event for a Call and (optionally) persist the mutated entity, atomically.
    /// The event's correlation mirrors the Call's, so every action stays on one causal
    /// chain (Volume 5 §3). Its idempotency key is deterministic per (call, type, version)
    /// so redelivery dedupes.
    async fn commit_event<P: EventPayload>(
        &self,
        tenant: Uuid,
        call: &Call,
        persist_entity: bool,
        payload: P,
    ) -> Result<(), RoutingError> {
        let ctx = Correlation {
            tenant_id: tenant,
            correlation_id: call.correlation_id,
            causation_id: None,
            sequence: None,
            traceparent: None,
        };
        let idem = format!("{}:{}:{}", call.base.id, P::TYPE, call.base.version);
        let envelope = Envelope::new(payload, &ctx, idem);
        let calls = if persist_entity { vec![call.clone()] } else { vec![] };
        self.store
            .commit(Tx {
                calls,
                events: vec![envelope.to_json()],
                ..Default::default()
            })
            .await?;
        self.signal.wake();
        Ok(())
    }

    /// Place an answered Call on hold (`POST /v1/calls/{id}:hold` → `CallHeld`).
    pub async fn hold(&self, tenant: Uuid, call_id: Uuid) -> Result<Call, RoutingError> {
        let mut call = self.load(tenant, call_id).await?;
        call.transition(CallState::Held)
            .map_err(|e| RoutingError::IllegalState(e.to_string()))?;
        self.commit_event(tenant, &call, true, CallHeld { call_id: call.base.id })
            .await?;
        self.media.dispatch(MediaCommand::Hold { call_id: call.base.id });
        Ok(call)
    }

    /// Resume a held Call (`POST /v1/calls/{id}:resume` → `CallResumed`).
    pub async fn resume(&self, tenant: Uuid, call_id: Uuid) -> Result<Call, RoutingError> {
        let mut call = self.load(tenant, call_id).await?;
        call.transition(CallState::Answered)
            .map_err(|e| RoutingError::IllegalState(e.to_string()))?;
        self.commit_event(tenant, &call, true, CallResumed { call_id: call.base.id })
            .await?;
        self.media.dispatch(MediaCommand::Resume { call_id: call.base.id });
        Ok(call)
    }

    /// End a Call (`POST /v1/calls/{id}:hangup` → `CallEnded`). Computes the billed
    /// duration from the Call's own timestamps.
    pub async fn hangup(
        &self,
        tenant: Uuid,
        call_id: Uuid,
        cause: Option<String>,
    ) -> Result<Call, RoutingError> {
        let mut call = self.load(tenant, call_id).await?;
        call.transition(CallState::Ended)
            .map_err(|e| RoutingError::IllegalState(e.to_string()))?;
        let ended_at = call.ended_at.expect("ended_at set on transition to ENDED");
        let start = call.answered_at.unwrap_or(call.base.created_at);
        let duration_ms = (ended_at.into_offset() - start.into_offset())
            .whole_milliseconds()
            .max(0) as u64;
        self.commit_event(
            tenant,
            &call,
            true,
            CallEnded {
                call_id: call.base.id,
                ended_at,
                duration_ms,
                hangup_cause: cause,
            },
        )
        .await?;
        self.media.dispatch(MediaCommand::Hangup { call_id: call.base.id });
        Ok(call)
    }

    /// Transfer a Call to a new target (`POST /v1/calls/{id}:transfer` → `CallTransferred`).
    /// The Call's own lifecycle state is unchanged; the media leg is redirected.
    pub async fn transfer(
        &self,
        tenant: Uuid,
        call_id: Uuid,
        to_ref: String,
        transfer_type: TransferType,
        from_ref: Option<String>,
    ) -> Result<Call, RoutingError> {
        let call = self.load(tenant, call_id).await?;
        self.commit_event(
            tenant,
            &call,
            false,
            CallTransferred {
                call_id: call.base.id,
                transfer_type,
                from_ref,
                to_ref: to_ref.clone(),
            },
        )
        .await?;
        self.media
            .dispatch(MediaCommand::Transfer { call_id: call.base.id, to_ref });
        Ok(call)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::LoopbackMedia;
    use crate::store::{MemStore, Store};

    type FactRx = tokio::sync::mpsc::UnboundedReceiver<crate::media::MediaFact>;

    fn routing() -> (Routing, Arc<dyn Store>, FactRx) {
        let store: Arc<dyn Store> = Arc::new(MemStore::new());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let r = Routing::new(store.clone(), Arc::new(LoopbackMedia::new(tx)), RelaySignal::new());
        (r, store, rx)
    }

    fn originate_req() -> OriginateRequest {
        OriginateRequest {
            direction: Direction::Outbound,
            from_ref: "sip:100".to_string(),
            to_ref: "+14155550100".to_string(),
            idempotency_key: None,
        }
    }

    #[tokio::test]
    async fn full_lifecycle_hold_resume_hangup() {
        let (r, store, mut facts) = routing();
        let t = Uuid::now_v7();

        // Originate starts INITIATED; the loopback media plane queues ring+answer facts.
        let call = r.originate(t, originate_req()).await.unwrap();
        assert_eq!(call.state, CallState::Initiated);

        // Apply the media facts (as the fact loop does in `main`) → the Call reaches ANSWERED.
        while let Ok(fact) = facts.try_recv() {
            r.apply_fact(fact).await.unwrap();
        }
        let answered = store.get_call(t, call.base.id).await.unwrap().unwrap();
        assert_eq!(answered.state, CallState::Answered);

        assert_eq!(r.hold(t, call.base.id).await.unwrap().state, CallState::Held);
        assert_eq!(r.resume(t, call.base.id).await.unwrap().state, CallState::Answered);
        assert_eq!(r.hangup(t, call.base.id, None).await.unwrap().state, CallState::Ended);

        // A terminal Call cannot be held again.
        assert!(matches!(
            r.hold(t, call.base.id).await,
            Err(RoutingError::IllegalState(_))
        ));

        // Every transition emitted an event: CallStarted, Ringing, Answered, Held, Resumed, Ended.
        assert_eq!(store.peek_outbox(100).await.unwrap().len(), 6);
    }

    #[tokio::test]
    async fn action_on_missing_call_is_not_found() {
        let (r, _store, _facts) = routing();
        let t = Uuid::now_v7();
        assert!(matches!(
            r.hold(t, Uuid::now_v7()).await,
            Err(RoutingError::NotFound)
        ));
    }
}
