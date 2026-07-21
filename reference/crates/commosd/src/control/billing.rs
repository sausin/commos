//! Billing (control plane) — assembles CDRs and rates them (Volume 3 components.md:
//! "Billing consumes `CallEnded` and produces the CDR / `BillingGenerated`").
//!
//! Pure, side-effect-free assembly: a completed [`Call`] plus its owning organisation in,
//! a [`Cdr`] and its [`BillingGenerated`] event out. Persistence and event emission are
//! the caller's job (the hub calls these from `routing.hangup`); nothing here touches the
//! store, so the logic stays trivially testable.

use commos_core::common::{EntityBase, Uuid};
use commos_core::entities::call::Call;
use commos_core::entities::cdr::Cdr;
use commos_core::events::billing_generated::BillingGenerated;

use crate::control::rating::Rater;

/// Billed duration in milliseconds for a completed Call.
///
/// Measures `answered_at → ended_at`, falling back to `created_at → ended_at` for a Call
/// that ended before it was answered. An unended Call has no billable duration (0).
fn billed_ms(call: &Call) -> u64 {
    let Some(ended_at) = call.ended_at else {
        return 0;
    };
    let start = call.answered_at.unwrap_or(call.base.created_at);
    (ended_at.into_offset() - start.into_offset())
        .whole_milliseconds()
        .max(0) as u64
}

/// Assemble a [`Cdr`] from a completed [`Call`] for the given organisation.
///
/// The CDR gets a fresh [`EntityBase`] scoped to the Call's tenant. Attribution
/// (`device_id`, `identity_id`) and the negotiated `codec` (first media line) are copied
/// from the Call when present; `did` is the destination when it is an E.164. `cost` is
/// rated by the destination-aware [`Rater`] from the billable duration. `duration_ms` and
/// `billable_ms` are both measured from the Call's own timestamps (this reference bills
/// the full answered span — a real rater would subtract non-billable segments).
pub fn assemble_cdr(call: &Call, organisation_id: Uuid) -> Cdr {
    let ms = billed_ms(call);
    let codec = call.media.first().and_then(|m| m.codec.clone());
    // Record the dialled number only when it is E.164 (`+…`); internal `sip:` targets
    // and bare extensions have no DID.
    let did = call.to_ref.starts_with('+').then(|| call.to_ref.clone());
    let cost = Rater::default_table().rate(&call.to_ref, ms);
    Cdr {
        base: EntityBase::new(call.base.tenant_id),
        call_id: call.base.id,
        organisation_id,
        cost_centre_id: None,
        department_id: None,
        user_id: None,
        identity_id: call.identity_id,
        device_id: call.device_id,
        extension: None,
        did,
        carrier_id: None,
        duration_ms: ms,
        billable_ms: ms,
        cost,
        codec,
        recording_object_id: None,
        transcript_object_id: None,
        tags: Vec::new(),
    }
}

/// Map a rated [`Cdr`] to its [`BillingGenerated`] event payload (Volume 5). Attribution is
/// carried through so downstream consumers can bill without re-reading the CDR.
pub fn billing_event(cdr: &Cdr) -> BillingGenerated {
    BillingGenerated {
        cdr_id: cdr.base.id,
        call_id: cdr.call_id,
        user_id: cdr.user_id,
        device_id: cdr.device_id,
        billable_ms: cdr.billable_ms,
        cost: cdr.cost.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use commos_core::common::Currency;
    use commos_core::entities::call::{Call, CallState, Direction};

    fn answered_ended_call() -> Call {
        let mut call = Call::originate(Uuid::now_v7(), Direction::Outbound, "sip:100", "+14155550100");
        call.device_id = Some(Uuid::now_v7());
        call.identity_id = Some(Uuid::now_v7());
        call.transition(CallState::Ringing).unwrap();
        call.transition(CallState::Answered).unwrap();
        call.transition(CallState::Ended).unwrap();
        call
    }

    #[test]
    fn assemble_copies_attribution_and_scopes_tenant() {
        let call = answered_ended_call();
        let org = Uuid::now_v7();
        let cdr = assemble_cdr(&call, org);
        assert_eq!(cdr.call_id, call.base.id);
        assert_eq!(cdr.organisation_id, org);
        assert_eq!(cdr.base.tenant_id, call.base.tenant_id);
        assert_ne!(cdr.base.id, call.base.id, "CDR gets its own identity");
        assert_eq!(cdr.device_id, call.device_id);
        assert_eq!(cdr.identity_id, call.identity_id);
        assert_eq!(cdr.duration_ms, cdr.billable_ms);
        // Cost is what the destination-aware rater charges for this destination/duration.
        assert_eq!(cdr.cost, Rater::default_table().rate(&call.to_ref, cdr.billable_ms));
    }

    #[test]
    fn assemble_rates_by_destination_and_records_did() {
        // Outbound to a +1 destination: the rater prices it (2¢/min) and the E.164 lands
        // in `did`; the codec is copied from the first media line.
        let cdr = assemble_cdr(&answered_ended_call(), Uuid::now_v7());
        assert_eq!(cdr.did.as_deref(), Some("+14155550100"));
        assert_eq!(cdr.cost.currency, Currency::parse("USD").unwrap());
    }

    #[test]
    fn internal_call_has_no_did_and_zero_cost() {
        // sip:200 is on-net: no DID, and the internal tariff rates it free.
        let mut call =
            Call::originate(Uuid::now_v7(), Direction::Internal, "sip:100", "sip:200");
        call.transition(CallState::Ringing).unwrap();
        call.transition(CallState::Answered).unwrap();
        call.transition(CallState::Ended).unwrap();
        let cdr = assemble_cdr(&call, Uuid::now_v7());
        assert!(cdr.did.is_none());
        assert_eq!(cdr.cost.minor_units, 0);
    }

    #[test]
    fn unended_call_bills_zero() {
        let call = Call::originate(Uuid::now_v7(), Direction::Inbound, "sip:100", "sip:200");
        let cdr = assemble_cdr(&call, Uuid::now_v7());
        assert_eq!(cdr.duration_ms, 0);
        assert_eq!(cdr.billable_ms, 0);
        assert_eq!(cdr.cost.minor_units, 0);
    }

    #[test]
    fn billing_event_mirrors_cdr() {
        let cdr = assemble_cdr(&answered_ended_call(), Uuid::now_v7());
        let ev = billing_event(&cdr);
        assert_eq!(ev.cdr_id, cdr.base.id);
        assert_eq!(ev.call_id, cdr.call_id);
        assert_eq!(ev.billable_ms, cdr.billable_ms);
        assert_eq!(ev.device_id, cdr.device_id);
        assert_eq!(ev.cost, cdr.cost);
    }
}
