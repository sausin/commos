//! The canonical event envelope — Rust projection of
//! `contracts/json-schema/envelope.schema.json` (Volume 5 §2: CloudEvents 1.0 with
//! CommOS-required extensions).
//!
//! Events are the platform's highest-leverage integration surface (Volume 5). This
//! module makes the envelope a *typed* value: a payload declares its `TYPE` and
//! `SOURCE` via the [`EventPayload`] trait, and [`Envelope::new`] fills the envelope so
//! a caller cannot emit an event whose `type` disagrees with its `data` shape.

use serde::{Deserialize, Serialize};

use crate::common::{Timestamp, Uuid};

/// CommOS event-spec version carried in every envelope (`specversion`, pattern `^\d+\.\d+$`).
pub const SPEC_VERSION: &str = "0.4";

/// A typed event payload. Each canonical event (Volume 5 catalogue) implements this so
/// its envelope `type`/`source` are derived from the type, never hand-written.
pub trait EventPayload: Serialize {
    /// PascalCase event type, e.g. `CallStarted` (envelope `type`, pattern `^[A-Z][A-Za-z0-9]+$`).
    const TYPE: &'static str;
    /// Emitting subsystem URI, e.g. `/routing` (envelope `source`).
    const SOURCE: &'static str;

    /// The primary entity id this event is about (envelope `subject`).
    fn subject(&self) -> String;
}

/// The event envelope. Field names and required-ness mirror `envelope.schema.json`
/// exactly; optional fields are `Option` and skipped when absent so the serialised
/// form validates against the schema.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub id: Uuid,
    pub specversion: String,
    /// PascalCase event type. `type` is a Rust keyword, hence the rename.
    #[serde(rename = "type")]
    pub event_type: String,
    pub source: String,
    pub time: Timestamp,
    pub tenant_id: Uuid,
    pub subject: String,
    pub correlation_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<Uuid>,
    pub idempotency_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub traceparent: Option<String>,
    /// Const `application/json` per the contract.
    pub datacontenttype: String,
    pub data: T,
}

/// Correlation context threaded through a causal chain of events (Volume 5 §3:
/// `correlation_id` groups a workflow; `causation_id` names the direct parent).
#[derive(Clone, Debug)]
pub struct Correlation {
    pub tenant_id: Uuid,
    pub correlation_id: Uuid,
    pub causation_id: Option<Uuid>,
    pub sequence: Option<u64>,
    pub traceparent: Option<String>,
}

impl Correlation {
    /// Start a new correlation chain rooted at a fresh id.
    pub fn root(tenant_id: Uuid) -> Self {
        Correlation {
            tenant_id,
            correlation_id: Uuid::now_v7(),
            causation_id: None,
            sequence: Some(0),
            traceparent: None,
        }
    }
}

impl<T: EventPayload> Envelope<T> {
    /// Build a fully-populated envelope for `data` within a correlation context.
    ///
    /// `type`, `source`, `subject`, `specversion` and `datacontenttype` are all derived,
    /// so the envelope is internally consistent by construction (CMOS-05-EVT envelope).
    pub fn new(data: T, ctx: &Correlation, idempotency_key: impl Into<String>) -> Self {
        Envelope {
            id: Uuid::now_v7(),
            specversion: SPEC_VERSION.to_string(),
            event_type: T::TYPE.to_string(),
            source: T::SOURCE.to_string(),
            time: Timestamp::now(),
            tenant_id: ctx.tenant_id,
            subject: data.subject(),
            correlation_id: ctx.correlation_id,
            causation_id: ctx.causation_id,
            idempotency_key: idempotency_key.into(),
            sequence: ctx.sequence,
            traceparent: ctx.traceparent.clone(),
            datacontenttype: "application/json".to_string(),
            data,
        }
    }
}

impl<T: Serialize> Envelope<T> {
    /// Serialise to a `serde_json::Value` — the shape published to the Event Bus.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("envelope is always serialisable")
    }
}
