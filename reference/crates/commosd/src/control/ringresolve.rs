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
pub async fn resolve_plan(
    store: &Arc<dyn Store>,
    tenant: Uuid,
    dialled_number: &str,
    target_ref: &str,
    opts: PlanOpts,
    rotation: usize,
    registered: impl Fn(&str) -> bool,
) -> DialPlan {
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

/// Find the first **active** forwarding rule for `number` (a small config scan, like
/// `Routing::resolve_extension`). Errors and misses both yield `None`.
async fn active_forwarding(
    store: &Arc<dyn Store>,
    tenant: Uuid,
    number: &str,
) -> Option<commos_core::entities::forwarding::Forwarding> {
    let mut cursor = None;
    loop {
        let page = store.list_forwardings(tenant, 200, cursor).await.ok()?;
        if let Some(f) = page.items.iter().find(|f| f.number == number && f.is_active()) {
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
    use commos_core::entities::forwarding::{ForwardMode, Forwarding};
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
}
