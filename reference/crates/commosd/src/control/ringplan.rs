//! Ring planning (control plane) — the pure spine shared by ring groups, call
//! forwarding/follow-me, and queue overflow.
//!
//! A [`DialPlan`] is the deterministic answer to one question: *"when this number is
//! dialled, whom do we ring, in what order, for how long, what does the caller hear while it
//! rings, and where does the call go if nobody answers?"* Ring groups, follow-me, and
//! forward-on-condition are all just different shapes of the same plan, so the whole family
//! is expressed by one set of types and built by pure functions here. The SIP B2BUA
//! ([`crate::sip`]) is the *executor*: it walks the [`RingStage`]s, forking INVITEs and
//! streaming the caller treatment, and performs the [`FinalAction`] when the stages are
//! exhausted.
//!
//! Keeping the planning **pure** (no store, no sockets, no clock) is deliberate: this is the
//! logic most likely to grow corner cases — empty groups, disabled rules, offline
//! extensions, rotation, dedup, redirect loops — so it is the logic that most needs to be
//! exhaustively unit-testable in isolation. The async resolver that feeds it live entities
//! (loading ring groups, looking up forwarding rules, checking registration state) lives
//! alongside the SIP executor that walks the plan.

use commos_core::entities::forwarding::{ForwardMode, Forwarding};
use commos_core::entities::ring_group::{RingGroup, RingStrategy};

/// What the caller hears while a [`RingStage`] rings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Treatment {
    /// Normal ringback (a bodyless `180 Ringing`; the caller's phone renders its own tone,
    /// or the server streams a ringback cadence for a leg it already owns).
    Ringback,
    /// Music on hold — used while a caller waits in a queue or on a "please hold while I
    /// transfer you" forward. Consumed by the queue-wait treatment loop (a documented
    /// media-plane follow-up); the ring stages produced today all use [`Treatment::Ringback`].
    #[allow(dead_code)]
    MusicOnHold,
}

/// One ring stage: a set of contacts rung **together** for a bounded time.
///
/// A ring-all group is a single stage with every member; a hunt / follow-me chain is one
/// single-contact stage per step. The executor moves to the next stage when the current one
/// fails to connect (no-answer, busy, or decline — all just "did not answer").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RingStage {
    /// Destination references rung in parallel within this stage (`sip:100`, `101`,
    /// `external:+1…`). De-duplicated, never empty.
    pub contacts: Vec<String>,
    /// How long this stage rings before the executor advances (seconds).
    pub ring_seconds: u32,
    /// What the caller hears during this stage.
    pub treatment: Treatment,
    /// A short human-readable label for logs/observability (e.g. `"ring-all"`,
    /// `"follow-me:2"`, `"hunt:sip:101"`).
    pub label: String,
}

/// Where the call goes once every [`RingStage`] is exhausted without an answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FinalAction {
    /// Deposit to the voicemail box of this extension number.
    Voicemail(String),
    /// Re-resolve and route to another `destination_ref` (a queue, another ring group, an
    /// external number, …). The executor resolves it, applying the redirect depth guard.
    Redirect(String),
    /// Nothing more to do — release the call (`480`/`603`). Used when voicemail is disabled
    /// and there is no configured overflow.
    Hangup,
}

/// The full plan for one inbound call: an ordered list of ring stages then a final action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialPlan {
    pub stages: Vec<RingStage>,
    pub final_action: FinalAction,
}

impl DialPlan {
    /// A plan that rings nobody and goes straight to the final action (e.g. an unconditional
    /// forward with no reachable targets, or an empty ring group).
    pub fn immediate(final_action: FinalAction) -> Self {
        DialPlan { stages: Vec::new(), final_action }
    }

    /// Total seconds this plan will ring across all stages before the final action — the
    /// worst-case setup budget the executor should allow.
    #[allow(dead_code)] // used by the executor's overall-budget guard (follow-up) + tests
    pub fn total_ring_seconds(&self) -> u32 {
        self.stages.iter().map(|s| s.ring_seconds).sum()
    }
}

/// Tunables the planner needs that are not on the entities themselves.
#[derive(Clone, Copy, Debug)]
pub struct PlanOpts {
    /// Ring time to use when an entity leaves `ring_seconds` unset (derived from the
    /// server's `no_answer_rings`). Must be ≥ 1.
    pub default_ring_seconds: u32,
    /// Whether voicemail deposit is available as the no-answer fallback.
    pub voicemail_enabled: bool,
}

impl PlanOpts {
    /// Resolve an entity's optional `ring_seconds` against the default, rejecting
    /// non-positive overrides (a `0`/negative stored value falls back to the default rather
    /// than producing a zero-length, un-answerable stage).
    fn ring_seconds(&self, override_secs: Option<i64>) -> u32 {
        match override_secs {
            Some(s) if s >= 1 => s.min(u32::MAX as i64) as u32,
            _ => self.default_ring_seconds.max(1),
        }
    }

    /// The default no-answer terminus for a plain extension: voicemail if enabled, else hang
    /// up.
    fn default_final(&self, number: &str) -> FinalAction {
        if self.voicemail_enabled {
            FinalAction::Voicemail(number.to_string())
        } else {
            FinalAction::Hangup
        }
    }
}

/// Plan a plain dialled extension with no forwarding rule: ring it (if it has a live
/// registration) then fall through to voicemail / hangup.
///
/// An offline extension (`registered == false`) rings nobody — there is no point emitting a
/// stage that cannot possibly answer — and goes straight to the final action, exactly as the
/// legacy single-target path did.
pub fn plan_direct(number: &str, registered: bool, opts: &PlanOpts) -> DialPlan {
    let final_action = opts.default_final(number);
    if !registered {
        return DialPlan::immediate(final_action);
    }
    DialPlan {
        stages: vec![RingStage {
            contacts: vec![sip_contact(number)],
            ring_seconds: opts.ring_seconds(None),
            treatment: Treatment::Ringback,
            label: "direct".to_string(),
        }],
        final_action,
    }
}

/// Plan a dialled extension that has an **active** forwarding rule (already filtered by
/// [`Forwarding::is_active`]). `registered` is the extension's own live-registration state,
/// used to decide whether the extension is rung before the forward targets.
///
/// - [`ForwardMode::Always`] — the extension is never rung; the call follows the target
///   chain (follow-me).
/// - [`ForwardMode::NoAnswer`] / [`ForwardMode::Busy`] — ring the extension first (if it is
///   registered), then walk the targets. (Busy is over-approximated as no-answer here: the
///   executor advances on busy *or* timeout; distinguishing the two so a *busy*-only forward
///   never fires on a plain no-answer is a documented refinement.)
/// - [`ForwardMode::Unavailable`] — if the extension is registered the rule is inert and it
///   rings normally; only when it is offline does the call follow the targets.
pub fn plan_with_forwarding(
    number: &str,
    registered: bool,
    fwd: &Forwarding,
    opts: &PlanOpts,
) -> DialPlan {
    let stage_secs = opts.ring_seconds(fwd.ring_seconds);
    let final_action = opts.default_final(number);

    // Whether the extension itself is rung before the targets, given the mode + reg state.
    let ring_ext_first = match fwd.mode {
        ForwardMode::Always => false,
        // A dead extension is never worth ringing first, regardless of mode.
        ForwardMode::NoAnswer | ForwardMode::Busy => registered,
        // Unavailable only forwards when offline; if registered the rule does not apply.
        ForwardMode::Unavailable => {
            if registered {
                return plan_direct(number, true, opts);
            }
            false
        }
    };

    let mut stages = Vec::new();
    if ring_ext_first {
        stages.push(RingStage {
            contacts: vec![sip_contact(number)],
            ring_seconds: stage_secs,
            treatment: Treatment::Ringback,
            label: "forward:extension".to_string(),
        });
    }
    // Each forward target is its own sequential follow-me step.
    for (i, target) in normalize_members(&fwd.targets).into_iter().enumerate() {
        stages.push(RingStage {
            contacts: vec![target],
            ring_seconds: stage_secs,
            treatment: Treatment::Ringback,
            label: format!("follow-me:{}", i + 1),
        });
    }

    if stages.is_empty() {
        // Active rules always have targets, but be defensive.
        return DialPlan::immediate(final_action);
    }
    DialPlan { stages, final_action }
}

/// Plan a ring group. `rotation` is a per-call counter that spreads load for the rotating
/// strategies; it is ignored by [`RingStrategy::RingAll`] and [`RingStrategy::Sequential`].
///
/// The group's `no_answer_ref` becomes the [`FinalAction::Redirect`] terminus when set,
/// otherwise the call falls through to the dialled number's voicemail / hangup. An empty
/// group (no members, or all duplicates collapsing to nothing) rings no one and goes
/// straight to the final action.
pub fn plan_ring_group(
    number: &str,
    group: &RingGroup,
    opts: &PlanOpts,
    rotation: usize,
) -> DialPlan {
    let stage_secs = opts.ring_seconds(group.ring_seconds);
    let final_action = match &group.no_answer_ref {
        Some(r) if !r.trim().is_empty() => FinalAction::Redirect(r.clone()),
        _ => opts.default_final(number),
    };

    let members = normalize_members(&group.members);
    if members.is_empty() {
        return DialPlan::immediate(final_action);
    }

    let stages = match group.strategy {
        RingStrategy::RingAll => vec![RingStage {
            contacts: members,
            ring_seconds: stage_secs,
            treatment: Treatment::Ringback,
            label: "ring-all".to_string(),
        }],
        RingStrategy::Sequential | RingStrategy::RoundRobin | RingStrategy::Random => {
            let ordered = order_members(group.strategy, members, rotation);
            ordered
                .into_iter()
                .map(|m| RingStage {
                    contacts: vec![m.clone()],
                    ring_seconds: stage_secs,
                    treatment: Treatment::Ringback,
                    label: format!("hunt:{m}"),
                })
                .collect()
        }
    };
    DialPlan { stages, final_action }
}

/// Order the members of a hunt group according to the strategy.
///
/// - [`RingStrategy::Sequential`] — listed order, unchanged.
/// - [`RingStrategy::RoundRobin`] — listed order rotated so member `rotation % len` leads
///   (memory hunt: successive calls start with successive members, spreading load).
/// - [`RingStrategy::Random`] — a deterministic shuffle seeded by `rotation`, so a given call
///   index yields a stable order (testable) while different calls vary.
///
/// Panics never: `members` is assumed non-empty by the caller.
fn order_members(strategy: RingStrategy, members: Vec<String>, rotation: usize) -> Vec<String> {
    let len = members.len();
    match strategy {
        RingStrategy::Sequential => members,
        RingStrategy::RoundRobin => {
            let start = rotation % len;
            let mut out = Vec::with_capacity(len);
            for i in 0..len {
                out.push(members[(start + i) % len].clone());
            }
            out
        }
        RingStrategy::Random => shuffled(members, rotation as u64),
        // `RingAll` never reaches here.
        RingStrategy::RingAll => members,
    }
}

/// A deterministic Fisher–Yates shuffle driven by a small LCG seeded from `seed`. Pure and
/// reproducible (the module forbids `Math.random`-style nondeterminism): the same `seed`
/// always yields the same permutation, so production varies order per call by passing a
/// changing call counter while tests can pin a specific arrangement.
fn shuffled(mut v: Vec<String>, seed: u64) -> Vec<String> {
    // SplitMix64-style state advance — decent bit mixing without a dependency.
    let mut state = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut next = || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    for i in (1..v.len()).rev() {
        let j = (next() % (i as u64 + 1)) as usize;
        v.swap(i, j);
    }
    v
}

/// Normalise a bare extension number into a `sip:` contact ref if it is not already scheme-
/// prefixed. A member already carrying a scheme (`sip:`, `external:`, `ringgroup:`, …) is
/// left untouched so the executor/resolver can interpret it.
fn sip_contact(target: &str) -> String {
    if target.contains(':') {
        target.to_string()
    } else {
        format!("sip:{target}")
    }
}

/// Normalise a reference list for ringing: trim, drop blanks, prefix bare numbers with
/// `sip:`, and de-duplicate on the normalised value (so `100` and `sip:100` collapse to one),
/// preserving first-seen order.
fn normalize_members(refs: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for r in refs {
        let r = r.trim();
        if r.is_empty() {
            continue;
        }
        let normalized = sip_contact(r);
        if seen.insert(normalized.clone()) {
            out.push(normalized);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use commos_core::common::Uuid;
    use commos_core::entities::forwarding::Forwarding;
    use commos_core::entities::ring_group::RingGroup;

    fn opts(vm: bool) -> PlanOpts {
        PlanOpts { default_ring_seconds: 30, voicemail_enabled: vm }
    }

    // ---- plan_direct ----------------------------------------------------------------

    #[test]
    fn direct_registered_rings_then_voicemail() {
        let p = plan_direct("100", true, &opts(true));
        assert_eq!(p.stages.len(), 1);
        assert_eq!(p.stages[0].contacts, vec!["sip:100"]);
        assert_eq!(p.stages[0].ring_seconds, 30);
        assert_eq!(p.stages[0].treatment, Treatment::Ringback);
        assert_eq!(p.final_action, FinalAction::Voicemail("100".into()));
    }

    #[test]
    fn direct_offline_rings_nobody() {
        let p = plan_direct("100", false, &opts(true));
        assert!(p.stages.is_empty());
        assert_eq!(p.final_action, FinalAction::Voicemail("100".into()));
    }

    #[test]
    fn direct_no_voicemail_hangs_up() {
        let p = plan_direct("100", true, &opts(false));
        assert_eq!(p.final_action, FinalAction::Hangup);
        let off = plan_direct("100", false, &opts(false));
        assert_eq!(off, DialPlan::immediate(FinalAction::Hangup));
    }

    // ---- forwarding -----------------------------------------------------------------

    fn fwd(number: &str, mode: ForwardMode, targets: &[&str]) -> Forwarding {
        let mut f = Forwarding::create(Uuid::now_v7(), number, mode);
        f.targets = targets.iter().map(|s| s.to_string()).collect();
        f
    }

    #[test]
    fn forward_always_skips_extension() {
        let f = fwd("100", ForwardMode::Always, &["external:+14155550100"]);
        let p = plan_with_forwarding("100", true, &f, &opts(true));
        assert_eq!(p.stages.len(), 1);
        assert_eq!(p.stages[0].contacts, vec!["external:+14155550100"]);
        assert_eq!(p.stages[0].label, "follow-me:1");
        assert_eq!(p.final_action, FinalAction::Voicemail("100".into()));
    }

    #[test]
    fn forward_always_multi_target_is_follow_me_chain() {
        let f = fwd("100", ForwardMode::Always, &["sip:101", "external:+14155550100", "sip:102"]);
        let p = plan_with_forwarding("100", true, &f, &opts(true));
        assert_eq!(p.stages.len(), 3);
        assert_eq!(p.stages[0].contacts, vec!["sip:101"]);
        assert_eq!(p.stages[1].contacts, vec!["external:+14155550100"]);
        assert_eq!(p.stages[2].contacts, vec!["sip:102"]);
        // Each step is a distinct sequential stage.
        assert_eq!(p.stages[2].label, "follow-me:3");
    }

    #[test]
    fn forward_no_answer_rings_extension_first_then_targets() {
        let f = fwd("100", ForwardMode::NoAnswer, &["external:+14155550100"]);
        let p = plan_with_forwarding("100", true, &f, &opts(true));
        assert_eq!(p.stages.len(), 2);
        assert_eq!(p.stages[0].contacts, vec!["sip:100"]);
        assert_eq!(p.stages[0].label, "forward:extension");
        assert_eq!(p.stages[1].contacts, vec!["external:+14155550100"]);
    }

    #[test]
    fn forward_no_answer_offline_extension_skips_straight_to_targets() {
        let f = fwd("100", ForwardMode::NoAnswer, &["external:+14155550100"]);
        let p = plan_with_forwarding("100", false, &f, &opts(true));
        assert_eq!(p.stages.len(), 1, "offline extension is not rung");
        assert_eq!(p.stages[0].contacts, vec!["external:+14155550100"]);
    }

    #[test]
    fn forward_unavailable_is_inert_when_registered() {
        let f = fwd("100", ForwardMode::Unavailable, &["external:+14155550100"]);
        // Registered → rule does not fire; behaves like a plain direct dial.
        let p = plan_with_forwarding("100", true, &f, &opts(true));
        assert_eq!(p.stages.len(), 1);
        assert_eq!(p.stages[0].contacts, vec!["sip:100"]);
        assert_eq!(p.stages[0].label, "direct");
    }

    #[test]
    fn forward_unavailable_fires_when_offline() {
        let f = fwd("100", ForwardMode::Unavailable, &["external:+14155550100"]);
        let p = plan_with_forwarding("100", false, &f, &opts(true));
        assert_eq!(p.stages.len(), 1);
        assert_eq!(p.stages[0].contacts, vec!["external:+14155550100"]);
    }

    #[test]
    fn forward_uses_rule_ring_seconds_override() {
        let mut f = fwd("100", ForwardMode::NoAnswer, &["sip:101"]);
        f.ring_seconds = Some(12);
        let p = plan_with_forwarding("100", true, &f, &opts(true));
        assert!(p.stages.iter().all(|s| s.ring_seconds == 12));
    }

    #[test]
    fn forward_zero_ring_seconds_falls_back_to_default() {
        let mut f = fwd("100", ForwardMode::NoAnswer, &["sip:101"]);
        f.ring_seconds = Some(0);
        let p = plan_with_forwarding("100", true, &f, &opts(true));
        assert!(p.stages.iter().all(|s| s.ring_seconds == 30));
    }

    #[test]
    fn forward_dedups_targets_preserving_order() {
        let f = fwd("100", ForwardMode::Always, &["sip:101", "sip:101", "sip:102", "sip:101"]);
        let p = plan_with_forwarding("100", true, &f, &opts(true));
        assert_eq!(p.stages.len(), 2);
        assert_eq!(p.stages[0].contacts, vec!["sip:101"]);
        assert_eq!(p.stages[1].contacts, vec!["sip:102"]);
    }

    // ---- ring groups ----------------------------------------------------------------

    fn group(strategy: RingStrategy, members: &[&str]) -> RingGroup {
        let mut g = RingGroup::create(Uuid::now_v7(), strategy);
        g.members = members.iter().map(|s| s.to_string()).collect();
        g
    }

    #[test]
    fn ring_all_is_one_stage_with_all_members() {
        let g = group(RingStrategy::RingAll, &["sip:100", "sip:101", "102"]);
        let p = plan_ring_group("500", &g, &opts(true), 0);
        assert_eq!(p.stages.len(), 1);
        // Bare "102" is normalised to a sip: contact alongside the already-prefixed members.
        assert_eq!(p.stages[0].contacts, vec!["sip:100", "sip:101", "sip:102"]);
        assert_eq!(p.final_action, FinalAction::Voicemail("500".into()));
    }

    #[test]
    fn sequential_is_one_stage_per_member_in_order() {
        let g = group(RingStrategy::Sequential, &["sip:100", "sip:101", "sip:102"]);
        let p = plan_ring_group("500", &g, &opts(true), 7 /* ignored */);
        assert_eq!(p.stages.len(), 3);
        assert_eq!(p.stages[0].contacts, vec!["sip:100"]);
        assert_eq!(p.stages[1].contacts, vec!["sip:101"]);
        assert_eq!(p.stages[2].contacts, vec!["sip:102"]);
    }

    #[test]
    fn round_robin_rotates_start_by_rotation() {
        let g = group(RingStrategy::RoundRobin, &["a", "b", "c"]);
        let order = |rot| {
            plan_ring_group("500", &g, &opts(true), rot)
                .stages
                .into_iter()
                .map(|s| s.contacts[0].clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(order(0), vec!["sip:a", "sip:b", "sip:c"]);
        assert_eq!(order(1), vec!["sip:b", "sip:c", "sip:a"]);
        assert_eq!(order(2), vec!["sip:c", "sip:a", "sip:b"]);
        assert_eq!(order(3), vec!["sip:a", "sip:b", "sip:c"], "wraps around");
    }

    #[test]
    fn random_is_deterministic_per_rotation_and_permutes() {
        let g = group(RingStrategy::Random, &["a", "b", "c", "d", "e"]);
        let order = |rot| {
            plan_ring_group("500", &g, &opts(true), rot)
                .stages
                .into_iter()
                .map(|s| s.contacts[0].clone())
                .collect::<Vec<_>>()
        };
        let o1 = order(1);
        // Deterministic: same seed → same order.
        assert_eq!(o1, order(1));
        // A permutation: every member appears exactly once.
        let mut sorted = o1.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["sip:a", "sip:b", "sip:c", "sip:d", "sip:e"]);
        // Different seeds generally differ (at least one of a few does).
        let differs = (2..6).any(|r| order(r) != o1);
        assert!(differs, "random should vary across rotations");
    }

    #[test]
    fn group_no_answer_ref_becomes_redirect() {
        let mut g = group(RingStrategy::RingAll, &["sip:100"]);
        g.no_answer_ref = Some("queue:abc".into());
        let p = plan_ring_group("500", &g, &opts(true), 0);
        assert_eq!(p.final_action, FinalAction::Redirect("queue:abc".into()));
    }

    #[test]
    fn group_blank_no_answer_ref_falls_back_to_voicemail() {
        let mut g = group(RingStrategy::RingAll, &["sip:100"]);
        g.no_answer_ref = Some("   ".into());
        let p = plan_ring_group("500", &g, &opts(true), 0);
        assert_eq!(p.final_action, FinalAction::Voicemail("500".into()));
    }

    #[test]
    fn empty_group_rings_nobody_and_takes_final_action() {
        let g = group(RingStrategy::RingAll, &[]);
        let p = plan_ring_group("500", &g, &opts(true), 0);
        assert!(p.stages.is_empty());
        assert_eq!(p.final_action, FinalAction::Voicemail("500".into()));

        // A group whose members all collapse to duplicates/blanks is likewise empty.
        let g2 = group(RingStrategy::Sequential, &["sip:100", "sip:100", "  "]);
        let p2 = plan_ring_group("500", &g2, &opts(true), 0);
        assert_eq!(p2.stages.len(), 1, "collapses to the single unique member");
    }

    #[test]
    fn ring_all_dedups_members() {
        let g = group(RingStrategy::RingAll, &["sip:100", "sip:100", "sip:101"]);
        let p = plan_ring_group("500", &g, &opts(true), 0);
        assert_eq!(p.stages[0].contacts, vec!["sip:100", "sip:101"]);
    }

    #[test]
    fn total_ring_seconds_sums_stages() {
        let g = group(RingStrategy::Sequential, &["a", "b", "c"]);
        let p = plan_ring_group("500", &g, &opts(true), 0); // 3 stages × 30s
        assert_eq!(p.total_ring_seconds(), 90);
        // Ring-all is a single stage → just one stage's worth.
        let ga = group(RingStrategy::RingAll, &["a", "b", "c"]);
        assert_eq!(plan_ring_group("500", &ga, &opts(true), 0).total_ring_seconds(), 30);
    }

    #[test]
    fn sip_contact_leaves_scheme_prefixed_refs_untouched() {
        // A member already carrying a scheme passes through; a bare number gets sip:.
        let g = group(RingStrategy::RingAll, &["100", "sip:101", "external:+14155550100", "ringgroup:x"]);
        let p = plan_ring_group("500", &g, &opts(true), 0);
        assert_eq!(
            p.stages[0].contacts,
            vec!["sip:100", "sip:101", "external:+14155550100", "ringgroup:x"]
        );
    }
}
