//! Routing (control plane) — resolves and originates Calls (Volume 3 components.md:
//! "Routing produces `CallStarted`").
//!
//! This is the vertical slice that proves the spine end-to-end: an API command mutates an
//! entity, the mutation and its event are committed atomically to the outbox, the media
//! plane is commanded over the typed boundary, and the event surfaces on the bus.

use std::sync::Arc;

use commos_core::common::Uuid;
use commos_core::entities::call::{Call, Direction};
use commos_core::event::{Correlation, Envelope};
use commos_core::events::call_started::CallStarted;

use crate::media::{MediaCommand, MediaPlane};
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
    pub fn originate(&self, tenant: Uuid, req: OriginateRequest) -> Result<Call, RoutingError> {
        // Idempotent replay: a repeat of the same key returns the already-created Call.
        if let Some(key) = &req.idempotency_key {
            if let Some(existing_id) = self.store.call_for_idempotency_key(tenant, key) {
                if let Some(existing) = self.store.get_call(tenant, existing_id) {
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
        self.store.commit(Tx {
            calls: vec![call.clone()],
            events: vec![envelope.to_json()],
            idempotency: req
                .idempotency_key
                .map(|k| (tenant, k, call.base.id)),
        })?;
        // Wake the relay so the event surfaces promptly.
        self.signal.wake();

        // Command the media plane over the typed boundary (control decides, media acts).
        let ack = self.media.dispatch(MediaCommand::Originate {
            call_id: call.base.id,
            from_ref: call.from_ref.clone(),
            to_ref: call.to_ref.clone(),
        });
        if let crate::media::MediaAck::Rejected { reason, .. } = ack {
            return Err(RoutingError::MediaRejected(reason));
        }

        Ok(call)
    }
}
