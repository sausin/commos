//! `RingGroup` entity — the "ring the whole team" routing target.
//!
//! A RingGroup fans one inbound call out to a set of member endpoints under a
//! [`RingStrategy`]: ring them all at once, walk them one at a time (hunt), rotate the
//! starting member, or pick at random. Like [`Queue`](super::queue::Queue) and
//! [`Extension`](super::extension::Extension) it is *configuration*, not an occurrence: it
//! carries no lifecycle state machine and has no creation event in the frozen catalogue
//! (CMOS-02-DOM-100). The control plane reaches it through a `ringgroup:<uuid>`
//! `destination_ref` on a [`Route`](super::route::Route), the same scheme convention the
//! other targets use.
//!
//! Members are destination references (`sip:<user>`, a bare extension number, …) resolved to
//! live registrations at call time — the same shape as `Queue::members`. `ring_seconds`
//! bounds how long the group rings before the call falls through to `no_answer_ref` (a
//! voicemail box, another group, a queue, …); when unset the server's `no_answer_rings`
//! default applies.

use serde::{Deserialize, Serialize};

use crate::common::{EntityBase, Uuid};

/// How a [`RingGroup`] distributes an inbound call across its `members`
/// (`RingGroup.schema.json` `strategy`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RingStrategy {
    /// Ring every member simultaneously; first to answer wins (the classic "ring all").
    RingAll,
    /// Ring members one at a time in listed order, each for `ring_seconds`, until one
    /// answers (a hunt group / linear hunt).
    Sequential,
    /// Like [`Sequential`](Self::Sequential) but the starting member rotates per call, so
    /// load spreads evenly across the group (round-robin hunt / "memory hunt").
    RoundRobin,
    /// Ring members one at a time in a per-call random order.
    Random,
}

impl RingStrategy {
    /// Whether this strategy rings members one at a time (a hunt) rather than all at once.
    pub fn is_sequential(self) -> bool {
        !matches!(self, RingStrategy::RingAll)
    }
}

/// The RingGroup entity. `EntityBase` is flattened so the wire shape is
/// `allOf: [EntityBase] + RingGroup properties`, matching the schema. Only `strategy` is
/// required; `members` and the tuning fields are optional.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RingGroup {
    #[serde(flatten)]
    pub base: EntityBase,
    /// How the call is distributed across `members`.
    pub strategy: RingStrategy,
    /// Member destination references (`sip:100`, `101`, …) resolved to registrations at
    /// call time. Order is significant for the sequential strategies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
    /// Seconds to ring before giving up. For the ring-all strategy this bounds the whole
    /// attempt; for the sequential strategies it bounds *each* member's turn. Unset → the
    /// server's `no_answer_rings` default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ring_seconds: Option<i64>,
    /// Where the call goes if no member answers — a `destination_ref` (`sip:<vm>`,
    /// `ringgroup:<uuid>`, `queue:<uuid>`, …). Unset → the platform's normal no-answer
    /// handling (voicemail of the dialled number, if enabled).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_answer_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl RingGroup {
    /// Create a new RingGroup with the given `strategy`, no members, and all tuning options
    /// unset. Callers set `members` / `ring_seconds` / `no_answer_ref` / `label` directly on
    /// the returned value.
    pub fn create(tenant: Uuid, strategy: RingStrategy) -> Self {
        RingGroup {
            base: EntityBase::new(tenant),
            strategy,
            members: Vec::new(),
            ring_seconds: None,
            no_answer_ref: None,
            label: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_starts_v0() {
        let t = Uuid::now_v7();
        let g = RingGroup::create(t, RingStrategy::RingAll);
        assert_eq!(g.strategy, RingStrategy::RingAll);
        assert_eq!(g.base.version, 0);
        assert_eq!(g.base.tenant_id, t);
        assert!(g.members.is_empty());
    }

    #[test]
    fn strategy_serialises_screaming_snake_and_round_trips() {
        let mut g = RingGroup::create(Uuid::now_v7(), RingStrategy::RoundRobin);
        g.members = vec!["sip:100".into(), "101".into()];
        g.ring_seconds = Some(20);
        g.no_answer_ref = Some("queue:overflow".into());
        g.label = Some("Sales".into());
        let json = serde_json::to_value(&g).unwrap();
        assert_eq!(json["strategy"], "ROUND_ROBIN");
        assert_eq!(json["members"][0], "sip:100");
        assert_eq!(json["members"][1], "101");
        assert_eq!(json["ring_seconds"], 20);
        assert_eq!(json["no_answer_ref"], "queue:overflow");
        assert_eq!(json["label"], "Sales");
        assert!(json.get("id").is_some());
        assert!(json.get("tenant_id").is_some());

        let back: RingGroup = serde_json::from_value(json).unwrap();
        assert_eq!(back.strategy, RingStrategy::RoundRobin);
        assert_eq!(back.members, vec!["sip:100".to_string(), "101".to_string()]);
        assert_eq!(back.ring_seconds, Some(20));

        // Every variant renders SCREAMING_SNAKE.
        let render = |s| serde_json::to_value(RingGroup::create(Uuid::now_v7(), s)).unwrap()["strategy"].clone();
        assert_eq!(render(RingStrategy::RingAll), "RING_ALL");
        assert_eq!(render(RingStrategy::Sequential), "SEQUENTIAL");
        assert_eq!(render(RingStrategy::Random), "RANDOM");
    }

    #[test]
    fn empty_members_and_none_options_are_omitted() {
        let g = RingGroup::create(Uuid::now_v7(), RingStrategy::Sequential);
        let json = serde_json::to_value(&g).unwrap();
        assert!(json.get("members").is_none());
        assert!(json.get("ring_seconds").is_none());
        assert!(json.get("no_answer_ref").is_none());
        assert!(json.get("label").is_none());
    }

    #[test]
    fn is_sequential_only_false_for_ring_all() {
        assert!(!RingStrategy::RingAll.is_sequential());
        assert!(RingStrategy::Sequential.is_sequential());
        assert!(RingStrategy::RoundRobin.is_sequential());
        assert!(RingStrategy::Random.is_sequential());
    }
}
