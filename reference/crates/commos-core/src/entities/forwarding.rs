//! `Forwarding` entity — per-extension call forwarding and follow-me.
//!
//! A Forwarding rule redirects calls dialled to one extension `number` onto one or more
//! other destinations, either unconditionally or on a condition (busy, no-answer,
//! unavailable). An ordered list of `targets` expresses **follow-me** — "ring my desk, then
//! my mobile, then reception" — which the control plane walks in order until one answers.
//!
//! It is *configuration*, not an occurrence (CMOS-02-DOM-100): no lifecycle state machine,
//! no creation event. It is keyed by the extension `number` (a small string, the same key
//! the voicemail mailbox uses) rather than a foreign key to an Extension, because a dialled
//! number is what the routing layer has in hand and an Extension carries no owner link
//! today. The `enabled` flag lets a user park a rule (set once, toggled by a `*` feature
//! code later) without deleting it.

use serde::{Deserialize, Serialize};

use crate::common::{EntityBase, Uuid};

/// When a [`Forwarding`] rule fires (`Forwarding.schema.json` `mode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ForwardMode {
    /// Unconditional (CFU / "call forward all"): the extension itself is never rung — the
    /// call goes straight to `targets`.
    Always,
    /// Forward only when the extension is busy (on another call).
    Busy,
    /// Ring the extension first; forward to `targets` only after it fails to answer within
    /// `ring_seconds`. This is the follow-me case.
    NoAnswer,
    /// Forward only when the extension is unavailable (no live registration / offline).
    Unavailable,
}

impl ForwardMode {
    /// Whether the extension itself should be rung *before* the forward targets are tried.
    /// Only [`Always`](Self::Always) bypasses the extension entirely.
    pub fn rings_extension_first(self) -> bool {
        !matches!(self, ForwardMode::Always)
    }
}

/// The Forwarding entity. `EntityBase` is flattened so the wire shape is
/// `allOf: [EntityBase] + Forwarding properties`. `number`, `mode`, and `targets` are
/// required; a rule with an empty `targets` list is inert (nowhere to forward to).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Forwarding {
    #[serde(flatten)]
    pub base: EntityBase,
    /// The extension number this rule applies to (its routing key).
    pub number: String,
    /// Whether the rule is active. A disabled rule is stored but ignored by routing.
    pub enabled: bool,
    /// The condition under which the call is forwarded.
    pub mode: ForwardMode,
    /// Ordered forward destinations (`destination_ref`s). Walked in order for follow-me;
    /// for [`ForwardMode::Always`] the first is the forward target and any remainder is the
    /// follow-me chain after it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
    /// For [`ForwardMode::NoAnswer`], seconds to ring the extension (and each subsequent
    /// target) before advancing. Unset → the server's `no_answer_rings` default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ring_seconds: Option<i64>,
}

impl Forwarding {
    /// Create a new, enabled Forwarding rule for `number` with the given `mode` and no
    /// targets. Callers push onto `targets` and set `ring_seconds` on the returned value.
    pub fn create(tenant: Uuid, number: impl Into<String>, mode: ForwardMode) -> Self {
        Forwarding {
            base: EntityBase::new(tenant),
            number: number.into(),
            enabled: true,
            mode,
            targets: Vec::new(),
            ring_seconds: None,
        }
    }

    /// Whether this rule should currently affect routing: enabled and with somewhere to go.
    pub fn is_active(&self) -> bool {
        self.enabled && !self.targets.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_starts_enabled_v0() {
        let t = Uuid::now_v7();
        let f = Forwarding::create(t, "100", ForwardMode::NoAnswer);
        assert_eq!(f.number, "100");
        assert!(f.enabled);
        assert_eq!(f.mode, ForwardMode::NoAnswer);
        assert_eq!(f.base.version, 0);
        assert_eq!(f.base.tenant_id, t);
        // No targets yet → inert.
        assert!(!f.is_active());
    }

    #[test]
    fn active_requires_enabled_and_targets() {
        let mut f = Forwarding::create(Uuid::now_v7(), "100", ForwardMode::Always);
        f.targets = vec!["external:+14155550100".into()];
        assert!(f.is_active());
        f.enabled = false;
        assert!(!f.is_active(), "disabled rule is inert");
        f.enabled = true;
        f.targets.clear();
        assert!(!f.is_active(), "empty targets is inert");
    }

    #[test]
    fn mode_serialises_screaming_snake_and_round_trips() {
        let mut f = Forwarding::create(Uuid::now_v7(), "200", ForwardMode::Always);
        f.targets = vec!["external:+14155550100".into(), "sip:201".into()];
        f.ring_seconds = Some(15);
        let json = serde_json::to_value(&f).unwrap();
        assert_eq!(json["number"], "200");
        assert_eq!(json["enabled"], true);
        assert_eq!(json["mode"], "ALWAYS");
        assert_eq!(json["targets"][0], "external:+14155550100");
        assert_eq!(json["targets"][1], "sip:201");
        assert_eq!(json["ring_seconds"], 15);

        let back: Forwarding = serde_json::from_value(json).unwrap();
        assert_eq!(back.mode, ForwardMode::Always);
        assert_eq!(back.targets.len(), 2);
        assert_eq!(back.ring_seconds, Some(15));

        let render = |m| {
            serde_json::to_value(Forwarding::create(Uuid::now_v7(), "1", m)).unwrap()["mode"].clone()
        };
        assert_eq!(render(ForwardMode::Busy), "BUSY");
        assert_eq!(render(ForwardMode::NoAnswer), "NO_ANSWER");
        assert_eq!(render(ForwardMode::Unavailable), "UNAVAILABLE");
    }

    #[test]
    fn rings_extension_first_only_false_for_always() {
        assert!(!ForwardMode::Always.rings_extension_first());
        assert!(ForwardMode::Busy.rings_extension_first());
        assert!(ForwardMode::NoAnswer.rings_extension_first());
        assert!(ForwardMode::Unavailable.rings_extension_first());
    }

    #[test]
    fn empty_targets_and_none_ring_seconds_omitted() {
        let f = Forwarding::create(Uuid::now_v7(), "100", ForwardMode::NoAnswer);
        let json = serde_json::to_value(&f).unwrap();
        assert!(json.get("targets").is_none());
        assert!(json.get("ring_seconds").is_none());
    }
}
