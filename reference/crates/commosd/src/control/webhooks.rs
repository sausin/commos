//! Webhooks — outbound delivery of the canonical event stream to registered HTTP endpoints
//! (Volume 5 §EVT-014). This is what turns the event spine into an integration platform:
//! an operator registers a `Webhook` (url + which event types + an optional signing secret),
//! and every matching event the outbox relays is POSTed to it, HMAC-signed.
//!
//! The dispatcher subscribes to the in-process [`EventBus`] — the same fan-out the relay
//! publishes to — so it inherits the transactional-outbox guarantee upstream (no event is
//! relayed unless its state change committed). Delivery itself is at-least-once best-effort:
//! each attempt emits a `WebhookDelivered` or `WebhookDeliveryFailed` event (which flows back
//! through the outbox), and those delivery-result events are **not** themselves delivered to
//! webhooks, so there is no feedback loop. Retry/back-off and a dead-letter queue are the
//! documented next step; this reference delivers once per event.

use std::sync::Arc;

use commos_core::common::Uuid;
use commos_core::entities::webhook::Webhook;
use commos_core::event::{Correlation, Envelope};
use commos_core::events::webhook_delivered::WebhookDelivered;
use commos_core::events::webhook_delivery_failed::WebhookDeliveryFailed;
use tokio::sync::watch;

use crate::bus::{EventBus, EventJson};
use crate::config::SecretRef;
use crate::control::webhook_delivery::{self, DeliveryError};
use crate::relay::RelaySignal;
use crate::store::{Store, StoreError, Tx};

/// The Webhook management service (create / list / delete). Held on `AppState`.
#[derive(Clone)]
pub struct WebhookService {
    store: Arc<dyn Store>,
    signal: RelaySignal,
}

impl WebhookService {
    pub fn new(store: Arc<dyn Store>, signal: RelaySignal) -> Self {
        WebhookService { store, signal }
    }

    /// Register a webhook subscription.
    pub async fn create(
        &self,
        tenant: Uuid,
        url: String,
        event_types: Vec<String>,
        secret_ref: Option<String>,
    ) -> Result<Webhook, StoreError> {
        let mut w = Webhook::new(tenant, url, event_types);
        w.secret_ref = secret_ref;
        self.store
            .commit(Tx { webhooks: vec![w.clone()], ..Default::default() })
            .await?;
        self.signal.wake();
        Ok(w)
    }

    pub async fn list(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<crate::store::Page<Webhook>, StoreError> {
        self.store.list_webhooks(tenant, limit, cursor).await
    }

    pub async fn delete(&self, tenant: Uuid, id: Uuid) -> Result<bool, StoreError> {
        self.store.delete_webhook(tenant, id).await
    }
}

/// True when this event type is a webhook-delivery *result* — never re-delivered, so a
/// delivery result cannot trigger another delivery.
fn is_delivery_result(event_type: &str) -> bool {
    matches!(event_type, "WebhookDelivered" | "WebhookDeliveryFailed")
}

/// Whether a webhook subscribed to `subscribed` types wants an event of `event_type`.
/// `"*"` subscribes to everything.
fn wants(subscribed: &[String], event_type: &str) -> bool {
    subscribed.iter().any(|t| t == "*" || t == event_type)
}

/// Resolve a webhook's signing secret from its reference (`env://`/`file://`), if set.
/// A reference that fails to resolve yields `None` (the delivery goes out unsigned, logged).
fn resolve_secret(secret_ref: &Option<String>) -> Option<String> {
    let uri = secret_ref.as_ref()?;
    match (SecretRef { ref_uri: uri.clone() }).resolve() {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!(error = %e, "webhook secret reference did not resolve; delivering unsigned");
            None
        }
    }
}

/// Deliver one event to all matching, active webhooks for its tenant, emitting a
/// delivery-result event per attempt.
async fn deliver_event(store: &Arc<dyn Store>, signal: &RelaySignal, event: &EventJson) {
    let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if event_type.is_empty() || is_delivery_result(event_type) {
        return;
    }
    let tenant = match event.get("tenant_id").and_then(|v| v.as_str()).and_then(|s| Uuid::parse(s).ok()) {
        Some(t) => t,
        None => return,
    };
    let delivered_event_id = event
        .get("id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse(s).ok())
        .unwrap_or_else(Uuid::now_v7);

    // The webhook fleet per tenant is small; a single page suffices for the reference.
    let hooks = match store.list_webhooks(tenant, 500, None).await {
        Ok(p) => p.items,
        Err(e) => {
            tracing::warn!(error = %e, "listing webhooks for delivery failed");
            return;
        }
    };

    let body = match serde_json::to_vec(event.as_ref()) {
        Ok(b) => b,
        Err(_) => return,
    };

    for w in hooks.into_iter().filter(|w| w.active && wants(&w.event_types, event_type)) {
        let secret = resolve_secret(&w.secret_ref);
        let result = webhook_delivery::deliver(&w.url, secret.as_deref(), &body).await;
        let ev = build_result_event(tenant, &w, delivered_event_id, &result);
        // Record the delivery outcome through the outbox (at-least-once, EVT-014).
        if let Err(e) = store.commit(Tx { events: vec![ev], ..Default::default() }).await {
            tracing::warn!(error = %e, "recording webhook delivery result failed");
        } else {
            signal.wake();
        }
    }
}

/// Build the WebhookDelivered / WebhookDeliveryFailed envelope for one attempt. A 2xx is a
/// success; any other HTTP status or a transport error is a failure.
fn build_result_event(
    tenant: Uuid,
    w: &Webhook,
    delivered_event_id: Uuid,
    result: &Result<webhook_delivery::Delivered, DeliveryError>,
) -> serde_json::Value {
    let ctx = Correlation::root(tenant);
    match result {
        Ok(d) if (200..300).contains(&d.status_code) => Envelope::new(
            WebhookDelivered {
                webhook_id: w.base.id,
                delivered_event_id,
                status_code: d.status_code,
                attempt: 1,
                duration_ms: d.duration_ms,
            },
            &ctx,
            format!("{}:{}:WebhookDelivered", w.base.id, delivered_event_id),
        )
        .to_json(),
        Ok(d) => Envelope::new(
            WebhookDeliveryFailed {
                webhook_id: w.base.id,
                delivered_event_id,
                attempt: 1,
                error: format!("HTTP {}", d.status_code),
            },
            &ctx,
            format!("{}:{}:WebhookDeliveryFailed", w.base.id, delivered_event_id),
        )
        .to_json(),
        Err(e) => Envelope::new(
            WebhookDeliveryFailed {
                webhook_id: w.base.id,
                delivered_event_id,
                attempt: 1,
                error: e.to_string(),
            },
            &ctx,
            format!("{}:{}:WebhookDeliveryFailed", w.base.id, delivered_event_id),
        )
        .to_json(),
    }
}

/// Run the webhook dispatcher: subscribe to the bus and deliver each event to matching
/// webhooks until shutdown. Spawned once from `main`.
pub async fn run(
    store: Arc<dyn Store>,
    signal: RelaySignal,
    bus: EventBus,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut rx = bus.subscribe();
    tracing::info!("webhook dispatcher started");
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
            }
            recv = rx.recv() => match recv {
                Ok(event) => deliver_event(&store, &signal, &event).await,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "webhook dispatcher lagged; some events not delivered");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }
    tracing::info!("webhook dispatcher stopped");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemStore;

    #[test]
    fn wants_matches_wildcard_and_exact() {
        assert!(wants(&["*".into()], "CallStarted"));
        assert!(wants(&["CallStarted".into(), "CallEnded".into()], "CallEnded"));
        assert!(!wants(&["CallStarted".into()], "MessageSent"));
    }

    #[test]
    fn delivery_results_are_not_redelivered() {
        assert!(is_delivery_result("WebhookDelivered"));
        assert!(is_delivery_result("WebhookDeliveryFailed"));
        assert!(!is_delivery_result("CallStarted"));
    }

    #[tokio::test]
    async fn create_list_delete_roundtrip() {
        let store: Arc<dyn Store> = Arc::new(MemStore::new());
        let svc = WebhookService::new(store.clone(), RelaySignal::new());
        let t = Uuid::now_v7();
        let w = svc
            .create(t, "http://localhost:9/hook".into(), vec!["*".into()], None)
            .await
            .unwrap();
        assert!(w.active);
        assert_eq!(svc.list(t, 50, None).await.unwrap().items.len(), 1);
        assert!(svc.delete(t, w.base.id).await.unwrap());
        assert!(svc.list(t, 50, None).await.unwrap().items.is_empty());
    }
}
