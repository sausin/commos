//! Billing (control plane) — assembles CDRs and rates them (Volume 3 components.md:
//! "Billing consumes `CallEnded` and produces the CDR / `BillingGenerated`").
//!
//! Pure, side-effect-free assembly: a completed [`Call`] plus its owning organisation in,
//! a [`Cdr`] and its [`BillingGenerated`] event out. Persistence and event emission are
//! the caller's job (the hub calls these from `routing.hangup`); nothing here touches the
//! store, so the logic stays trivially testable.

use commos_core::common::{Currency, EntityBase, Money, Uuid};
use commos_core::entities::call::Call;
use commos_core::entities::cdr::Cdr;
use commos_core::events::billing_generated::BillingGenerated;

/// Rate a billable duration into a [`Money`] cost.
///
/// **Placeholder rater**: a flat 2 US cents per whole minute. This stands in for the real
/// Rating interface (`contracts/json-schema/interfaces/RatingRequest`), which a production
/// deployment would call out to for tariff lookup, per-destination pricing and currency
/// selection. Kept deterministic and dependency-free so CDR assembly can be tested in
/// isolation.
pub fn rate(billable_ms: u64) -> Money {
    let whole_minutes = billable_ms / 1000 / 60;
    Money {
        // `USD` is a valid ISO-4217 code, so the parse never fails.
        currency: Currency::parse("USD").expect("USD is a valid currency"),
        minor_units: (whole_minutes as i64) * 2,
    }
}

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
/// (`device_id`, `identity_id`) and the negotiated `codec` are copied from the Call when
/// present; `cost` is rated from the billable duration. `duration_ms` and `billable_ms`
/// are both measured from the Call's own timestamps (this reference bills the full
/// answered span — a real rater would subtract non-billable segments).
pub fn assemble_cdr(call: &Call, organisation_id: Uuid) -> Cdr {
    let ms = billed_ms(call);
    let codec = call.media.first().and_then(|m| m.codec.clone());
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
        did: None,
        carrier_id: None,
        duration_ms: ms,
        billable_ms: ms,
        cost: rate(ms),
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
    fn rate_is_two_cents_per_minute() {
        assert_eq!(rate(0).minor_units, 0);
        assert_eq!(rate(59_000).minor_units, 0);
        assert_eq!(rate(60_000).minor_units, 2);
        assert_eq!(rate(150_000).minor_units, 4);
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
        assert_eq!(cdr.cost, rate(cdr.billable_ms));
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
