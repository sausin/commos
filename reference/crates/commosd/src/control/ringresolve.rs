//! Ring resolution — turn a dialled destination into a concrete [`DialPlan`] using live
//! configuration (ring groups, forwarding rules) and registration state.
//!
//! This is the async glue between the pure [`ringplan`] builder and the world: it loads the
//! relevant entities from the [`Store`] and asks a caller-supplied predicate whether a target
//! currently has a live registration, then delegates the actual plan construction to the pure
//! functions (which stay exhaustively unit-tested in isolation). The SIP B2BUA calls
//! [`resolve_plan`] at INVITE time and then *executes* the returned plan.
//!
//! Resolution precedence for a dialled number:
//! 1. An **active forwarding rule** for the number wins (call-forward / follow-me).
//! 2. Otherwise, if the routed destination is a **ring group** (`ringgroup:<uuid>`), fan out
//!    across its members.
//! 3. Otherwise it is a plain extension — ring it (if registered) then voicemail.

use std::sync::Arc;

use commos_core::common::Uuid;
use commos_core::entities::forwarding::{ForwardMode, Forwarding};

use crate::control::ringplan::{self, DialPlan, PlanOpts};
use crate::store::Store;

/// The `ringgroup:` destination-ref scheme (mirrors `queue:` / `external:` conventions).
pub const RING_GROUP_SCHEME: &str = "ringgroup:";

/// Resolve the dial plan for a call to `dialled_number` whose route points at `target_ref`.
///
/// - `registered(ref)` reports whether a destination reference currently has a live SIP
///   registration (the SIP layer supplies this from its registrar; a resolver test supplies a
///   set). It is consulted for the dialled extension and is the reason resolution is not pure.
/// - `rotation` is a per-call counter spreading hunt/round-robin start positions.
///
/// Never fails: a missing/looking-up-empty ring group, or a store error, degrades to the
/// safest plan (ring the plain extension, or go to voicemail), so a config gap can never wedge
/// an inbound call.
#[allow(dead_code)] // the non-DND entry point; production uses `resolve_plan_dnd`
pub async fn resolve_plan(
    store: &Arc<dyn Store>,
    tenant: Uuid,
    dialled_number: &str,
    target_ref: &str,
    opts: PlanOpts,
    rotation: usize,
    registered: impl Fn(&str) -> bool,
) -> DialPlan {
    resolve_plan_dnd(store, tenant, dialled_number, target_ref, opts, rotation, registered, false)
        .await
}

/// [`resolve_plan`] with the dialled extension's **do-not-disturb** state. A DND extension is
/// never rung: an `ALWAYS` forward still applies (it never rings the extension either),
/// otherwise the call takes its `BUSY` rule (then `UNAVAILABLE`), otherwise voicemail.
#[allow(clippy::too_many_arguments)]
pub async fn resolve_plan_dnd(
    store: &Arc<dyn Store>,
    tenant: Uuid,
    dialled_number: &str,
    target_ref: &str,
    opts: PlanOpts,
    rotation: usize,
    registered: impl Fn(&str) -> bool,
    dnd: bool,
) -> DialPlan {
    if dnd {
        if let Some(fwd) = find_forwarding(store, tenant, dialled_number, |f| {
            f.mode == ForwardMode::Always
        })
        .await
        {
            return ringplan::plan_with_forwarding(dialled_number, false, &fwd, &opts);
        }
        if let Some(fwd) = busy_forwarding(store, tenant, dialled_number, true).await {
            return ringplan::plan_busy(dialled_number, &fwd, &opts);
        }
        return DialPlan::immediate(if opts.voicemail_enabled {
            ringplan::FinalAction::Voicemail(dialled_number.to_string())
        } else {
            ringplan::FinalAction::Hangup
        });
    }

    // 1. Forwarding rule takes precedence.
    if let Some(fwd) = active_forwarding(store, tenant, dialled_number).await {
        let ext_registered = registered(dialled_number);
        return ringplan::plan_with_forwarding(dialled_number, ext_registered, &fwd, &opts);
    }

    // 2. Ring group.
    if let Some(id) = target_ref.strip_prefix(RING_GROUP_SCHEME) {
        if let Ok(gid) = Uuid::parse(id.trim()) {
            if let Ok(Some(group)) = store.get_ring_group(tenant, gid).await {
                return ringplan::plan_ring_group(dialled_number, &group, &opts, rotation);
            }
        }
        // A dangling ringgroup: ref → nothing to ring; fall through to voicemail/hangup.
        return DialPlan::immediate(if opts.voicemail_enabled {
            ringplan::FinalAction::Voicemail(dialled_number.to_string())
        } else {
            ringplan::FinalAction::Hangup
        });
    }

    // 3. Plain extension.
    ringplan::plan_direct(dialled_number, registered(dialled_number), &opts)
}

/// The plan for a call whose callee answered **busy** (`486`): the number's active `BUSY`
/// forwarding rule's targets, or `None` when there is no such rule (the caller then applies its
/// normal decline handling).
pub async fn resolve_busy_plan(
    store: &Arc<dyn Store>,
    tenant: Uuid,
    dialled_number: &str,
    opts: PlanOpts,
) -> Option<DialPlan> {
    let fwd = busy_forwarding(store, tenant, dialled_number, false).await?;
    Some(ringplan::plan_busy(dialled_number, &fwd, &opts))
}

/// The active `BUSY` rule for `number` (and, with `include_unavailable`, failing that its active
/// `UNAVAILABLE` rule).
async fn busy_forwarding(
    store: &Arc<dyn Store>,
    tenant: Uuid,
    number: &str,
    include_unavailable: bool,
) -> Option<Forwarding> {
    if let Some(f) =
        find_forwarding(store, tenant, number, |f| f.mode == ForwardMode::Busy).await
    {
        return Some(f);
    }
    if include_unavailable {
        return find_forwarding(store, tenant, number, |f| f.mode == ForwardMode::Unavailable)
            .await;
    }
    None
}

/// Find the first **active** forwarding rule for `number` that applies *before/while ringing*
/// (a small config scan, like `Routing::resolve_extension`). `BUSY` rules are excluded — they
/// only fire on a busy outcome ([`resolve_busy_plan`]) or DND. Errors and misses both yield
/// `None`. Public so the SIP layer can cheaply decide whether a dialled number needs the
/// multi-destination plan path at all (keeping the plain single-extension bridge path untouched
/// when it does not).
pub async fn active_forwarding(
    store: &Arc<dyn Store>,
    tenant: Uuid,
    number: &str,
) -> Option<Forwarding> {
    find_forwarding(store, tenant, number, |f| f.mode != ForwardMode::Busy).await
}

/// The first active rule for `number` satisfying `pred`.
async fn find_forwarding(
    store: &Arc<dyn Store>,
    tenant: Uuid,
    number: &str,
    pred: impl Fn(&Forwarding) -> bool,
) -> Option<Forwarding> {
    let mut cursor = None;
    loop {
        let page = store.list_forwardings(tenant, 200, cursor).await.ok()?;
        if let Some(f) = page
            .items
            .iter()
            .find(|f| f.number == number && f.is_active() && pred(f))
        {
            return Some(f.clone());
        }
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::ringplan::{FinalAction, Treatment};
    use crate::store::{MemStore, Tx};
    use commos_core::entities::ring_group::{RingGroup, RingStrategy};
    use std::collections::HashSet;

    fn opts() -> PlanOpts {
        PlanOpts { default_ring_seconds: 30, voicemail_enabled: true }
    }

    fn store() -> (Arc<dyn Store>, Uuid) {
        (Arc::new(MemStore::new()), Uuid::now_v7())
    }

    /// A registration predicate over a fixed set of registered numbers/refs.
    fn regset(refs: &[&str]) -> impl Fn(&str) -> bool {
        let set: HashSet<String> = refs.iter().map(|s| s.to_string()).collect();
        move |r: &str| set.contains(r)
    }

    #[tokio::test]
    async fn plain_registered_extension_rings_then_voicemail() {
        let (s, t) = store();
        let plan = resolve_plan(&s, t, "100", "sip:100@host", opts(), 0, regset(&["100"])).await;
        assert_eq!(plan.stages.len(), 1);
        assert_eq!(plan.stages[0].contacts, vec!["sip:100"]);
        assert_eq!(plan.final_action, FinalAction::Voicemail("100".into()));
    }

    #[tokio::test]
    async fn ring_group_target_fans_out_to_members() {
        let (s, t) = store();
        let mut g = RingGroup::create(t, RingStrategy::RingAll);
        g.members = vec!["sip:100".into(), "sip:101".into()];
        let gid = g.base.id;
        s.commit(Tx { ring_groups: vec![g], ..Default::default() }).await.unwrap();

        let target = format!("{RING_GROUP_SCHEME}{gid}");
        let plan = resolve_plan(&s, t, "500", &target, opts(), 0, regset(&[])).await;
        assert_eq!(plan.stages.len(), 1, "ring-all is a single stage");
        assert_eq!(plan.stages[0].contacts, vec!["sip:100", "sip:101"]);
    }

    #[tokio::test]
    async fn dangling_ring_group_ref_goes_to_voicemail() {
        let (s, t) = store();
        let target = format!("{RING_GROUP_SCHEME}{}", Uuid::now_v7());
        let plan = resolve_plan(&s, t, "500", &target, opts(), 0, regset(&[])).await;
        assert!(plan.stages.is_empty());
        assert_eq!(plan.final_action, FinalAction::Voicemail("500".into()));
    }

    #[tokio::test]
    async fn forwarding_rule_takes_precedence_over_direct() {
        let (s, t) = store();
        let mut f = Forwarding::create(t, "100", ForwardMode::Always);
        f.targets = vec!["external:+14155550100".into()];
        s.commit(Tx { forwardings: vec![f], ..Default::default() }).await.unwrap();

        // Even though 100 is registered, an ALWAYS forward skips it.
        let plan = resolve_plan(&s, t, "100", "sip:100@host", opts(), 0, regset(&["100"])).await;
        assert_eq!(plan.stages.len(), 1);
        assert_eq!(plan.stages[0].contacts, vec!["external:+14155550100"]);
    }

    #[tokio::test]
    async fn no_answer_forward_rings_registered_extension_first() {
        let (s, t) = store();
        let mut f = Forwarding::create(t, "100", ForwardMode::NoAnswer);
        f.targets = vec!["external:+14155550100".into()];
        s.commit(Tx { forwardings: vec![f], ..Default::default() }).await.unwrap();

        // Registered → ring the extension first, then the forward target.
        let plan = resolve_plan(&s, t, "100", "sip:100@host", opts(), 0, regset(&["100"])).await;
        assert_eq!(plan.stages.len(), 2);
        assert_eq!(plan.stages[0].contacts, vec!["sip:100"]);
        assert_eq!(plan.stages[1].contacts, vec!["external:+14155550100"]);

        // Offline → skip the extension, go straight to the target.
        let plan_off = resolve_plan(&s, t, "100", "sip:100@host", opts(), 0, regset(&[])).await;
        assert_eq!(plan_off.stages.len(), 1);
        assert_eq!(plan_off.stages[0].contacts, vec!["external:+14155550100"]);
    }

    #[tokio::test]
    async fn disabled_forwarding_is_ignored() {
        let (s, t) = store();
        let mut f = Forwarding::create(t, "100", ForwardMode::Always);
        f.targets = vec!["external:+14155550100".into()];
        f.enabled = false;
        s.commit(Tx { forwardings: vec![f], ..Default::default() }).await.unwrap();

        // Disabled rule ignored → plain extension behaviour.
        let plan = resolve_plan(&s, t, "100", "sip:100@host", opts(), 0, regset(&["100"])).await;
        assert_eq!(plan.stages[0].contacts, vec!["sip:100"]);
        assert_eq!(plan.stages[0].treatment, Treatment::Ringback);
    }

    #[tokio::test]
    async fn dnd_goes_to_busy_rule_then_unavailable_then_voicemail() {
        let (s, t) = store();
        // No rules: voicemail, extension never rung even though registered.
        let p = resolve_plan_dnd(&s, t, "100", "sip:100@h", opts(), 0, regset(&["100"]), true).await;
        assert!(p.stages.is_empty());
        assert_eq!(p.final_action, FinalAction::Voicemail("100".into()));

        // An UNAVAILABLE rule is used when there is no BUSY rule.
        let mut u = Forwarding::create(t, "100", ForwardMode::Unavailable);
        u.targets = vec!["sip:200".into()];
        s.commit(Tx { forwardings: vec![u], ..Default::default() }).await.unwrap();
        let p = resolve_plan_dnd(&s, t, "100", "sip:100@h", opts(), 0, regset(&["100"]), true).await;
        assert_eq!(p.stages[0].contacts, vec!["sip:200"]);

        // BUSY wins over UNAVAILABLE.
        let mut b = Forwarding::create(t, "100", ForwardMode::Busy);
        b.targets = vec!["external:+14155550100".into()];
        s.commit(Tx { forwardings: vec![b], ..Default::default() }).await.unwrap();
        let p = resolve_plan_dnd(&s, t, "100", "sip:100@h", opts(), 0, regset(&["100"]), true).await;
        assert_eq!(p.stages.len(), 1);
        assert_eq!(p.stages[0].contacts, vec!["external:+14155550100"]);
        // ...and without DND the BUSY rule is inert for ringing: plain direct plan.
        let p = resolve_plan_dnd(&s, t, "100", "sip:100@h", opts(), 0, regset(&["100"]), false).await;
        assert_eq!(p.stages[0].contacts, vec!["sip:100"]);
    }

    #[tokio::test]
    async fn dnd_does_not_override_an_always_forward() {
        let (s, t) = store();
        let mut f = Forwarding::create(t, "100", ForwardMode::Always);
        f.targets = vec!["external:+14155550100".into()];
        s.commit(Tx { forwardings: vec![f], ..Default::default() }).await.unwrap();
        let p = resolve_plan_dnd(&s, t, "100", "sip:100@h", opts(), 0, regset(&["100"]), true).await;
        assert_eq!(p.stages[0].contacts, vec!["external:+14155550100"]);
    }

    #[tokio::test]
    async fn busy_plan_only_exists_with_an_active_busy_rule() {
        let (s, t) = store();
        assert!(resolve_busy_plan(&s, t, "100", opts()).await.is_none());
        let mut na = Forwarding::create(t, "100", ForwardMode::NoAnswer);
        na.targets = vec!["sip:300".into()];
        s.commit(Tx { forwardings: vec![na], ..Default::default() }).await.unwrap();
        assert!(resolve_busy_plan(&s, t, "100", opts()).await.is_none(), "no-answer is not busy");
        let mut b = Forwarding::create(t, "100", ForwardMode::Busy);
        b.targets = vec!["sip:301".into()];
        s.commit(Tx { forwardings: vec![b], ..Default::default() }).await.unwrap();
        let p = resolve_busy_plan(&s, t, "100", opts()).await.unwrap();
        assert_eq!(p.stages[0].contacts, vec!["sip:301"]);
        // A BUSY rule does not trigger the up-front forwarding path.
        assert_eq!(active_forwarding(&s, t, "100").await.unwrap().mode, ForwardMode::NoAnswer);
    }
}
