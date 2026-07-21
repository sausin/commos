//! `Queue` entity — Rust projection of
//! `contracts/json-schema/entities/Queue.schema.json`.
//!
//! A Queue is the contact-centre workload's distribution surface: it holds a set of member
//! references and a `strategy` selecting how waiting work is dispatched to them. It is a
//! *peer* workload entity to `Call`/`Channel` on the same substrate (CMOS-02-DOM-100),
//! proving the substrate is workload-general. A Queue is *configuration*, not an occurrence:
//! it carries no lifecycle state machine and (unlike Channel/Call) has no creation event in
//! the frozen catalogue.

use serde::{Deserialize, Serialize};

use crate::common::{EntityBase, Uuid};

/// Distribution strategy of a Queue (`Queue.schema.json` `strategy`). Selects how waiting
/// work is dispatched across `members`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum QueueStrategy {
    Ringall,
    LeastRecent,
    FewestCalls,
    RoundRobin,
    Skills,
}

/// The Queue entity. `EntityBase` is flattened so the wire shape is
/// `allOf: [EntityBase] + Queue properties`, matching the schema. Only `strategy` is
/// required; `members` and the tuning fields are optional.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Queue {
    #[serde(flatten)]
    pub base: EntityBase,
    pub strategy: QueueStrategy,
    /// Member references (resolve to Devices/Identities via the attribution chain,
    /// CMOS-02-DOM-113).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sla_seconds: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_wait_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overflow_ref: Option<String>,
}

impl Queue {
    /// Create a new Queue with the given distribution `strategy`, no members, and all
    /// tuning options unset. Callers set `members` / `sla_seconds` / `max_wait_ms` /
    /// `overflow_ref` directly on the returned value.
    pub fn create(tenant: Uuid, strategy: QueueStrategy) -> Self {
        Queue {
            base: EntityBase::new(tenant),
            strategy,
            members: Vec::new(),
            sla_seconds: None,
            max_wait_ms: None,
            overflow_ref: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_starts_v0() {
        let t = Uuid::now_v7();
        let q = Queue::create(t, QueueStrategy::Ringall);
        assert_eq!(q.strategy, QueueStrategy::Ringall);
        assert_eq!(q.base.version, 0);
        assert_eq!(q.base.tenant_id, t);
        assert!(q.members.is_empty());
    }

    #[test]
    fn strategy_serialises_screaming_snake() {
        let mut q = Queue::create(Uuid::now_v7(), QueueStrategy::LeastRecent);
        q.members = vec!["sip:100".into()];
        q.sla_seconds = Some(30);
        q.max_wait_ms = Some(120_000);
        q.overflow_ref = Some("queue:overflow".into());
        let json = serde_json::to_value(&q).unwrap();
        // SCREAMING_SNAKE_CASE, faithful to the schema enum.
        assert_eq!(json["strategy"], "LEAST_RECENT");
        assert_eq!(json["members"][0], "sip:100");
        assert_eq!(json["sla_seconds"], 30);
        assert_eq!(json["max_wait_ms"], 120_000);
        assert_eq!(json["overflow_ref"], "queue:overflow");
        assert!(json.get("id").is_some());
        assert!(json.get("tenant_id").is_some());
        // Round-trips.
        let back: Queue = serde_json::from_value(json).unwrap();
        assert_eq!(back.strategy, QueueStrategy::LeastRecent);
        assert_eq!(back.members, vec!["sip:100".to_string()]);

        // Every variant renders SCREAMING_SNAKE.
        let render = |s| serde_json::to_value(Queue::create(Uuid::now_v7(), s)).unwrap()["strategy"].clone();
        assert_eq!(render(QueueStrategy::Ringall), "RINGALL");
        assert_eq!(render(QueueStrategy::FewestCalls), "FEWEST_CALLS");
        assert_eq!(render(QueueStrategy::RoundRobin), "ROUND_ROBIN");
        assert_eq!(render(QueueStrategy::Skills), "SKILLS");
    }

    #[test]
    fn empty_members_and_none_options_are_omitted() {
        let q = Queue::create(Uuid::now_v7(), QueueStrategy::Skills);
        let json = serde_json::to_value(&q).unwrap();
        assert!(json.get("members").is_none());
        assert!(json.get("sla_seconds").is_none());
        assert!(json.get("max_wait_ms").is_none());
        assert!(json.get("overflow_ref").is_none());
    }
}
