//! Routing (control plane) — resolves and originates Calls (Volume 3 components.md:
//! "Routing produces `CallStarted`").
//!
//! This is the vertical slice that proves the spine end-to-end: an API command mutates an
//! entity, the mutation and its event are committed atomically to the outbox, the media
//! plane is commanded over the typed boundary, and the event surfaces on the bus.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

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

use crate::control::billing;
use crate::control::policy::{self, PolicyLimits, PolicyRequest};
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
    #[error("policy denied: {0}")]
    PolicyDenied(String),
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
    /// Origination policy / fraud guardrails (Volume 9): international blocked by default,
    /// optional concurrent-call cap, emergency bypass.
    policy: PolicyLimits,
    /// Home country code (digits) used to classify national vs international destinations.
    default_cc: String,
    /// Live count of in-progress calls per tenant — the input to the velocity guardrail.
    /// In-memory (like registrations): a capacity signal, not durable state.
    active: Arc<Mutex<HashMap<Uuid, u32>>>,
}

impl Routing {
    pub fn new(
        store: Arc<dyn Store>,
        media: Arc<dyn MediaPlane>,
        signal: RelaySignal,
        policy: PolicyLimits,
        default_cc: impl Into<String>,
    ) -> Self {
        Routing {
            store,
            media,
            signal,
            policy,
            default_cc: default_cc.into(),
            active: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Current in-progress call count for a tenant.
    fn active_count(&self, tenant: Uuid) -> u32 {
        *self.active.lock().expect("active-calls mutex").get(&tenant).unwrap_or(&0)
    }
    fn active_inc(&self, tenant: Uuid) {
        *self.active.lock().expect("active-calls mutex").entry(tenant).or_insert(0) += 1;
    }
    fn active_dec(&self, tenant: Uuid) {
        let mut g = self.active.lock().expect("active-calls mutex");
        if let Some(n) = g.get_mut(&tenant) {
            *n = n.saturating_sub(1);
        }
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

        // Fraud/authorization guardrail (Volume 9): classify the destination and enforce
        // policy before any Call is created. `caller_has_identity` is true because the request
        // arrives on an authenticated tenant bearer; per-user capability enforcement is layered
        // on once per-user auth lands (the evaluator already accepts capabilities).
        let pol = policy::evaluate(
            &PolicyRequest {
                to_ref: &req.to_ref,
                caller_has_identity: true,
                caller_capabilities: &[],
                active_calls: self.active_count(tenant),
            },
            &self.policy,
            &self.default_cc,
        );
        if !pol.allowed() {
            return Err(RoutingError::PolicyDenied(pol.reason));
        }
        if !pol.obligations.is_empty() {
            tracing::info!(obligations = ?pol.obligations, reason = %pol.reason,
                "call permitted with obligations");
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
        self.active_inc(tenant);

        // The Call starts INITIATED. Ring and answer arrive asynchronously as media facts
        // and are applied by `apply_fact` (media→control, CMOS-03-ARCH-003) — not computed
        // here. A client observes the progression via GET or the event stream.
        Ok(call)
    }

    /// Create an **inbound** Call from a SIP INVITE and emit `CallStarted`. Unlike
    /// [`Self::originate`], this does NOT dispatch to the loopback media plane — the SIP/RTP
    /// plane *is* the media here, and it reports ring/answer as media facts
    /// ([`Self::apply_fact`]) itself. Returns the created Call (state `INITIATED`).
    pub async fn create_inbound_call(
        &self,
        tenant: Uuid,
        from_ref: impl Into<String>,
        to_ref: impl Into<String>,
    ) -> Result<Call, RoutingError> {
        let call = Call::originate(tenant, Direction::Inbound, from_ref, to_ref);
        let ctx = Correlation {
            tenant_id: tenant,
            correlation_id: call.correlation_id,
            causation_id: None,
            sequence: Some(0),
            traceparent: None,
        };
        let envelope = Envelope::new(
            CallStarted {
                call_id: call.base.id,
                direction: call.direction,
                from_ref: call.from_ref.clone(),
                to_ref: call.to_ref.clone(),
                device_id: None,
            },
            &ctx,
            format!("{}:CallStarted", call.base.id),
        );
        self.store
            .commit(Tx {
                calls: vec![call.clone()],
                events: vec![envelope.to_json()],
                ..Default::default()
            })
            .await?;
        self.signal.wake();
        self.active_inc(tenant);
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

    /// Resolve a dialled `number` to its route's `destination_ref` via the Extension→Route
    /// mapping (Volume 3: an Extension maps a short number onto a routing target). Returns
    /// `None` when no extension carries that number or its route is missing.
    ///
    /// The fleet of extensions is small configuration, so a paged scan is fine here (there is
    /// no per-number index in this reference store).
    pub async fn resolve_extension(&self, tenant: Uuid, number: &str) -> Option<String> {
        let mut cursor = None;
        loop {
            let page = self.store.list_extensions(tenant, 200, cursor).await.ok()?;
            if let Some(ext) = page.items.iter().find(|e| e.number == number) {
                let route = self.store.get_route(tenant, ext.route_id).await.ok()??;
                return Some(route.destination_ref);
            }
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => return None,
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

    /// End a Call (`POST /v1/calls/{id}:hangup`). Emits `CallEnded` **and** assembles the
    /// billable CDR + `BillingGenerated` event, all in one transaction (Billing consumes
    /// `CallEnded`; here the platform produces the CDR synchronously so the record is durable
    /// with the state change — CMOS-03-ARCH-030, Volume 10).
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

        // Assemble the CDR from the completed Call (organisation = tenant for now).
        let cdr = billing::assemble_cdr(&call, tenant);
        let ctx = Correlation {
            tenant_id: tenant,
            correlation_id: call.correlation_id,
            causation_id: None,
            sequence: None,
            traceparent: None,
        };
        let ended = Envelope::new(
            CallEnded {
                call_id: call.base.id,
                ended_at,
                duration_ms: cdr.duration_ms,
                hangup_cause: cause,
            },
            &ctx,
            format!("{}:CallEnded:{}", call.base.id, call.base.version),
        );
        let billing = Envelope::new(
            billing::billing_event(&cdr),
            &ctx,
            format!("{}:BillingGenerated", cdr.base.id),
        );

        // Call (ENDED) + CDR + both events, atomically.
        self.store
            .commit(Tx {
                calls: vec![call.clone()],
                cdrs: vec![cdr],
                events: vec![ended.to_json(), billing.to_json()],
                ..Default::default()
            })
            .await?;
        self.signal.wake();
        self.active_dec(tenant);

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
        let r = Routing::new(
            store.clone(),
            Arc::new(LoopbackMedia::new(tx)),
            RelaySignal::new(),
            PolicyLimits::default(),
            "1",
        );
        (r, store, rx)
    }

    /// A Routing wired with explicit policy limits, for the guardrail tests.
    fn routing_with(limits: PolicyLimits) -> (Routing, Arc<dyn Store>, FactRx) {
        let store: Arc<dyn Store> = Arc::new(MemStore::new());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let r = Routing::new(
            store.clone(),
            Arc::new(LoopbackMedia::new(tx)),
            RelaySignal::new(),
            limits,
            "1",
        );
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

        // Events emitted: CallStarted, CallRinging, CallAnswered, CallHeld, CallResumed,
        // then CallEnded + BillingGenerated on hangup.
        assert_eq!(store.peek_outbox(100).await.unwrap().len(), 7);
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

    fn req_to(to: &str) -> OriginateRequest {
        OriginateRequest {
            direction: Direction::Outbound,
            from_ref: "sip:100".to_string(),
            to_ref: to.to_string(),
            idempotency_key: None,
        }
    }

    #[tokio::test]
    async fn international_is_blocked_by_default_but_national_allowed() {
        let (r, _s, _f) = routing(); // PolicyLimits::default(): allow_international = false
        let t = Uuid::now_v7();
        // National (+1, home cc) is permitted.
        assert!(r.originate(t, req_to("+14155550100")).await.is_ok());
        // International (+44 UK) is denied — the default toll-fraud guardrail.
        assert!(matches!(
            r.originate(t, req_to("+442071234567")).await,
            Err(RoutingError::PolicyDenied(_))
        ));
    }

    #[tokio::test]
    async fn international_allowed_when_opted_in() {
        let (r, _s, _f) = routing_with(PolicyLimits { allow_international: true, ..Default::default() });
        let t = Uuid::now_v7();
        assert!(r.originate(t, req_to("+442071234567")).await.is_ok());
    }

    #[tokio::test]
    async fn concurrency_cap_denies_excess_calls() {
        let (r, _s, _f) = routing_with(PolicyLimits { max_concurrent_calls: Some(1), ..Default::default() });
        let t = Uuid::now_v7();
        assert!(r.originate(t, req_to("+14155550100")).await.is_ok()); // active → 1
        assert!(matches!(
            r.originate(t, req_to("+14155550101")).await,
            Err(RoutingError::PolicyDenied(_))
        ));
    }

    #[tokio::test]
    async fn emergency_bypasses_every_guardrail() {
        // A zero cap would block everything — except an emergency call.
        let (r, _s, _f) = routing_with(PolicyLimits { max_concurrent_calls: Some(0), allow_international: false });
        let t = Uuid::now_v7();
        assert!(r.originate(t, req_to("911")).await.is_ok());
    }
}
