//! Prometheus / OpenMetrics instrumentation (Volume 15 §OBS-010) — a small, dependency-free
//! metrics registry exposed at `/metrics` in the text exposition format.
//!
//! Deliberately hand-rolled (no `prometheus`/`metrics` crate) to keep the pure-Rust,
//! cross-compiles-everywhere posture intact. Counters are driven two ways: cheap direct
//! increments (HTTP requests) and a **bus subscriber** that derives call/message/event
//! counters from the canonical event stream — so the control services need no metrics
//! plumbing. Gauges that are cheap to read live (uptime, registrations) are computed at
//! scrape time.
//!
//! OpenTelemetry/OTLP *export* (distributed tracing over the wire) is the documented next
//! step; it is deferred here because its exporters pull in a TLS/gRPC stack that would
//! compromise the clean cross-compile guarantee. Correlation is already carried end-to-end
//! by the event envelope's `correlation_id`/`traceparent` (OBS-001/002).

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

/// Cheap-to-clone handle over the metric registers.
#[derive(Clone, Default)]
pub struct Metrics {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    http_requests: AtomicU64,
    calls_started: AtomicU64,
    calls_ended: AtomicU64,
    active_calls: AtomicI64,
    messages_sent: AtomicU64,
    events_relayed: AtomicU64,
    webhook_deliveries: AtomicU64,
    webhook_failures: AtomicU64,
}

impl Metrics {
    pub fn new() -> Self {
        Metrics::default()
    }

    /// Count one handled HTTP request (called from the router middleware).
    pub fn inc_http(&self) {
        self.inner.http_requests.fetch_add(1, Ordering::Relaxed);
    }

    /// Fold one relayed event into the counters, keyed by its canonical `type`. Called by the
    /// metrics collector task for every event the bus relays, so call/message volume is
    /// derived from the event stream rather than instrumented in the control plane.
    pub fn on_event(&self, event_type: &str) {
        self.inner.events_relayed.fetch_add(1, Ordering::Relaxed);
        match event_type {
            "CallStarted" => {
                self.inner.calls_started.fetch_add(1, Ordering::Relaxed);
                self.inner.active_calls.fetch_add(1, Ordering::Relaxed);
            }
            "CallEnded" => {
                self.inner.calls_ended.fetch_add(1, Ordering::Relaxed);
                self.inner.active_calls.fetch_sub(1, Ordering::Relaxed);
            }
            "MessageSent" => {
                self.inner.messages_sent.fetch_add(1, Ordering::Relaxed);
            }
            "WebhookDelivered" => {
                self.inner.webhook_deliveries.fetch_add(1, Ordering::Relaxed);
            }
            "WebhookDeliveryFailed" => {
                self.inner.webhook_failures.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    /// Render the current values in the Prometheus text exposition format (OpenMetrics).
    /// `registrations` and `uptime_secs` are live gauges passed by the scrape handler.
    pub fn render(&self, uptime_secs: u64, version: &str, arch: &str, registrations: u64) -> String {
        let i = &self.inner;
        // active_calls is a signed counter that should never go below zero in practice; clamp
        // defensively so a lost/duplicated event can't surface a negative gauge.
        let active = i.active_calls.load(Ordering::Relaxed).max(0);
        let mut out = String::with_capacity(1024);
        let gauge = |out: &mut String, name: &str, help: &str, val: String| {
            out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n{name} {val}\n"));
        };
        let counter = |out: &mut String, name: &str, help: &str, val: u64| {
            out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n{name} {val}\n"));
        };

        out.push_str("# HELP commos_build_info Build information (constant 1).\n");
        out.push_str("# TYPE commos_build_info gauge\n");
        out.push_str(&format!(
            "commos_build_info{{version=\"{version}\",arch=\"{arch}\"}} 1\n"
        ));
        gauge(&mut out, "commos_uptime_seconds", "Seconds since process start.", uptime_secs.to_string());
        counter(&mut out, "commos_http_requests_total", "HTTP requests handled.", i.http_requests.load(Ordering::Relaxed));
        counter(&mut out, "commos_calls_started_total", "Calls started.", i.calls_started.load(Ordering::Relaxed));
        counter(&mut out, "commos_calls_ended_total", "Calls ended.", i.calls_ended.load(Ordering::Relaxed));
        gauge(&mut out, "commos_active_calls", "Calls currently in progress.", active.to_string());
        counter(&mut out, "commos_messages_sent_total", "Messages sent.", i.messages_sent.load(Ordering::Relaxed));
        counter(&mut out, "commos_events_relayed_total", "Events relayed to the bus.", i.events_relayed.load(Ordering::Relaxed));
        counter(&mut out, "commos_webhook_deliveries_total", "Successful webhook deliveries.", i.webhook_deliveries.load(Ordering::Relaxed));
        counter(&mut out, "commos_webhook_failures_total", "Failed webhook deliveries.", i.webhook_failures.load(Ordering::Relaxed));
        gauge(&mut out, "commos_registrations", "Active SIP registrations.", registrations.to_string());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_drive_counters_and_active_gauge() {
        let m = Metrics::new();
        m.on_event("CallStarted");
        m.on_event("CallStarted");
        m.on_event("CallEnded");
        m.on_event("MessageSent");
        let text = m.render(5, "0.4.0", "x86_64", 3);
        assert!(text.contains("commos_calls_started_total 2"));
        assert!(text.contains("commos_calls_ended_total 1"));
        assert!(text.contains("commos_active_calls 1"));
        assert!(text.contains("commos_messages_sent_total 1"));
        assert!(text.contains("commos_events_relayed_total 4"));
        assert!(text.contains("commos_registrations 3"));
        assert!(text.contains("commos_build_info{version=\"0.4.0\",arch=\"x86_64\"} 1"));
    }

    #[test]
    fn active_calls_never_negative() {
        let m = Metrics::new();
        m.on_event("CallEnded"); // stray end with no start
        let text = m.render(0, "0.4.0", "x86_64", 0);
        assert!(text.contains("commos_active_calls 0"));
    }

    #[test]
    fn http_counter_increments() {
        let m = Metrics::new();
        m.inc_http();
        m.inc_http();
        assert!(m.render(0, "v", "a", 0).contains("commos_http_requests_total 2"));
    }
}
