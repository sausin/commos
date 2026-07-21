//! Policy / authorization evaluator for call origination (control plane) — the
//! fraud-guardrail engine (Volume 9 Identity & Security).
//!
//! This is the deterministic projection of the frozen Policy interface
//! (`contracts/json-schema/interfaces/PolicyDecision.schema.json`): a decision is an
//! `effect` (`ALLOW` / `DENY`) plus a list of `obligations` the caller must satisfy.
//! The `REQUIRE_IDENTITY` / `REQUIRE_APPROVAL` semantics ride as **obligations**, never
//! as extra effects — an `ALLOW` with an unmet obligation is a block, but the policy
//! layer models it as "allowed, provided …" so the schema stays two-valued.
//!
//! Everything here is pure logic + `serde`: the same `(request, limits, country code)`
//! always yields the same [`Decision`]. Destination classification reuses
//! [`crate::control::dialplan`] so a policy decision and a rated CDR agree on what a
//! destination *is* (internal extension vs. national vs. international E.164).
//!
//! The defaults are the toll-fraud posture: international calling is **blocked** unless a
//! policy explicitly permits it or the caller carries the `calls.dial.international`
//! capability, and a velocity cap can hard-deny once too many calls are already active.
//! Emergency destinations bypass **everything** (CMOS-09 emergency override), including
//! the concurrency cap.

use serde::{Deserialize, Serialize};

use crate::control::dialplan;

/// The capability that lets a caller place international calls without an approval
/// obligation (and without tripping the `allow_international` policy gate).
const CAP_DIAL_INTERNATIONAL: &str = "calls.dial.international";

/// The small, deduped set of emergency service numbers recognised across regions
/// (NANP `911`, GSM/EU `112`, UK `999`, AU `000`, IN `108`/`112`, …). A destination
/// whose dialled digits match one of these bypasses all policy (CMOS-09).
const EMERGENCY_NUMBERS: &[&str] =
    &["911", "112", "999", "000", "111", "119", "110", "108", "118"];

/// Coarse destination classification used to pick a policy rule. Ordered by escalating
/// fraud/regulatory sensitivity in spirit, though evaluation order (not this order) is
/// what [`evaluate`] applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DestinationClass {
    /// On-net extension / non-dialable reference (`sip:100`, a bare `100`).
    Internal,
    /// An off-net E.164 number in the deployment's own country code.
    National,
    /// An off-net E.164 number in a different country code — or anything that could not
    /// be normalised and is not internal/emergency (conservative: safest for fraud).
    International,
    /// A recognised emergency service number — bypasses all policy.
    Emergency,
}

/// Classify a raw destination reference for policy purposes.
///
/// Detection order is deliberate and deterministic:
///
/// 1. **Emergency** — the dialled digits (after stripping a `sip:`/`tel:` scheme and any
///    `@host`) match [`EMERGENCY_NUMBERS`]. Checked first so an emergency call is never
///    mis-classified as a short internal extension.
/// 2. **Internal** — reuses [`dialplan::is_internal`]: a short all-digit reference or a
///    non-numeric `sip:` user (an on-net extension) that has no E.164 normalisation.
/// 3. **National vs. International** — [`dialplan::normalize_e164`] yields `+<cc>…`; if
///    `<cc>` equals `default_country_code` (digits only) it is National, otherwise
///    International.
/// 4. **Fallback** — a reference that is neither internal, emergency, nor E.164-normalisable
///    is treated as **International** (conservative — the fraud-safe default).
pub fn classify(to_ref: &str, default_country_code: &str) -> DestinationClass {
    // 1. Emergency wins over everything, including the "short digits = internal" rule.
    if is_emergency(to_ref) {
        return DestinationClass::Emergency;
    }

    // 2. Internal / on-net: no E.164 normalisation exists for it.
    if dialplan::is_internal(to_ref) {
        return DestinationClass::Internal;
    }

    // 3. Off-net E.164 → National if it carries our country code, else International.
    match dialplan::normalize_e164(to_ref, default_country_code) {
        Some(e164) => {
            let cc = digits_only(default_country_code);
            let number = e164.trim_start_matches('+');
            if !cc.is_empty() && number.starts_with(&cc) {
                DestinationClass::National
            } else {
                DestinationClass::International
            }
        }
        // 4. Un-normalisable and not internal/emergency → treat as International.
        None => DestinationClass::International,
    }
}

/// A policy effect. Mirrors `PolicyDecision.schema.json` `effect`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Effect {
    Allow,
    Deny,
}

/// An obligation the caller must satisfy for an `ALLOW` to actually place the call.
/// Mirrors `PolicyDecision.schema.json` `obligations`. An unmet obligation is treated
/// as a block by the caller (routing), even though the effect is `ALLOW`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Obligation {
    /// The caller must present a verified identity before the call proceeds.
    RequireIdentity,
    /// The call needs an out-of-band approval (e.g. for international dialling).
    RequireApproval,
}

/// Tenant/site policy limits that shape the decision. The [`Default`] is the toll-fraud
/// posture: international dialling is **blocked** (`allow_international: false`) and there is
/// **no** concurrency cap (`max_concurrent_calls: None`).
#[derive(Clone, Debug, Default)]
pub struct PolicyLimits {
    /// When `false` (default), an international destination is denied unless the caller
    /// carries the `calls.dial.international` capability. When `true`, international is
    /// permitted but still gated behind a `REQUIRE_APPROVAL` obligation.
    pub allow_international: bool,
    /// Velocity guardrail: the maximum number of simultaneously active calls. `None`
    /// means uncapped.
    pub max_concurrent_calls: Option<u32>,
}

/// The inputs to a single origination policy check.
pub struct PolicyRequest<'a> {
    /// The raw destination reference (`Call.to_ref`).
    pub to_ref: &'a str,
    /// Whether the caller has already presented a verified identity.
    pub caller_has_identity: bool,
    /// The caller's granted capabilities (e.g. `"calls.dial.international"`).
    pub caller_capabilities: &'a [String],
    /// How many calls the caller/tenant already has active (velocity input).
    pub active_calls: u32,
}

/// The outcome of a policy check: an [`Effect`] plus any [`Obligation`]s and a
/// human-readable `reason`. Serialises to the shape of `PolicyDecision.schema.json`
/// (`effect` + `obligations` + `reason`).
#[derive(Clone, Debug, Serialize)]
pub struct Decision {
    pub effect: Effect,
    pub obligations: Vec<Obligation>,
    pub reason: String,
}

impl Decision {
    /// `true` when the effect is [`Effect::Allow`]. Note that an allowed decision may
    /// still carry obligations the caller must satisfy before the call proceeds.
    pub fn allowed(&self) -> bool {
        self.effect == Effect::Allow
    }

    /// Construct an `ALLOW` with the given obligations and reason.
    fn allow(obligations: Vec<Obligation>, reason: impl Into<String>) -> Self {
        Decision { effect: Effect::Allow, obligations, reason: reason.into() }
    }

    /// Construct a `DENY` (never carries obligations) with the given reason.
    fn deny(reason: impl Into<String>) -> Self {
        Decision { effect: Effect::Deny, obligations: Vec::new(), reason: reason.into() }
    }
}

/// Evaluate origination policy for `req` against `limits`, deterministically.
///
/// Rules are applied **in this order** — the first that fires decides:
///
/// 1. **Emergency** destination → `ALLOW`, no obligations ("emergency call — policy
///    bypassed"). CMOS-09 emergency override bypasses everything, including the cap.
/// 2. **Concurrency/velocity** — `max_concurrent_calls` is `Some(cap)` and
///    `active_calls >= cap` → `DENY` (hard; fraud velocity guardrail).
/// 3. **International** — class is `International` and the caller lacks the
///    `calls.dial.international` capability: if `allow_international` is `false` → `DENY`
///    ("international calling is not permitted by policy"); otherwise → `ALLOW` with a
///    `REQUIRE_APPROVAL` obligation ("international call requires approval").
/// 4. **External identity** — a `National` or `International` call by a caller without a
///    verified identity → `ALLOW` with a `REQUIRE_IDENTITY` obligation ("external call
///    requires caller identity").
/// 5. Otherwise → `ALLOW`, no obligations ("permitted").
///
/// Emergency and velocity are hard outcomes; the international and identity rules produce
/// obligations (or a `DENY` for international-not-permitted).
pub fn evaluate(
    req: &PolicyRequest,
    limits: &PolicyLimits,
    default_country_code: &str,
) -> Decision {
    let class = classify(req.to_ref, default_country_code);

    // 1. Emergency override — bypasses every other rule (including the cap).
    if class == DestinationClass::Emergency {
        return Decision::allow(Vec::new(), "emergency call — policy bypassed");
    }

    // 2. Velocity / concurrency guardrail — hard deny.
    if let Some(cap) = limits.max_concurrent_calls {
        if req.active_calls >= cap {
            return Decision::deny(format!(
                "concurrency cap reached: {} active call(s) at or above the limit of {}",
                req.active_calls, cap
            ));
        }
    }

    let has_intl_capability = req
        .caller_capabilities
        .iter()
        .any(|c| c == CAP_DIAL_INTERNATIONAL);

    // 3. International gating (for callers without the explicit capability).
    if class == DestinationClass::International && !has_intl_capability {
        if !limits.allow_international {
            return Decision::deny("international calling is not permitted by policy");
        }
        return Decision::allow(
            vec![Obligation::RequireApproval],
            "international call requires approval",
        );
    }

    // 4. Any external call needs a caller identity.
    let is_external =
        matches!(class, DestinationClass::National | DestinationClass::International);
    if is_external && !req.caller_has_identity {
        return Decision::allow(
            vec![Obligation::RequireIdentity],
            "external call requires caller identity",
        );
    }

    // 5. Clean allow.
    Decision::allow(Vec::new(), "permitted")
}

/// `true` when the dialled digits of `to_ref` (scheme + `@host` stripped, visual
/// separators removed) exactly match a recognised emergency number.
///
/// Exposed to the router so it can mirror the emergency **cap bypass** when reserving a
/// concurrency slot (an emergency call must never be denied by the velocity guardrail).
pub(crate) fn is_emergency(to_ref: &str) -> bool {
    // Unwrap a name-addr ("Display" <sip:user@host>) to its addr-spec.
    let addr = match (to_ref.find('<'), to_ref.find('>')) {
        (Some(open), Some(close)) if close > open => to_ref[open + 1..close].trim(),
        _ => to_ref.trim(),
    };
    // Strip a URI scheme and any @host / ;params, leaving the user part.
    let after_scheme = addr
        .strip_prefix("sips:")
        .or_else(|| addr.strip_prefix("sip:"))
        .or_else(|| addr.strip_prefix("tel:"))
        .unwrap_or(addr);
    let user = after_scheme.split('@').next().unwrap_or(after_scheme);
    let user = user.split(';').next().unwrap_or(user);
    // Drop visual separators, then compare the significant characters.
    let cleaned: String = user
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '(' | ')' | '.'))
        .collect();
    EMERGENCY_NUMBERS.contains(&cleaned.as_str())
}

/// Keep only the ASCII digits of `s` (drops a leading `+`, whitespace, etc.).
fn digits_only(s: &str) -> String {
    s.chars().filter(char::is_ascii_digit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn request<'a>(to_ref: &'a str, has_identity: bool, capabilities: &'a [String]) -> PolicyRequest<'a> {
        PolicyRequest {
            to_ref,
            caller_has_identity: has_identity,
            caller_capabilities: capabilities,
            active_calls: 0,
        }
    }

    #[test]
    fn classify_covers_each_class() {
        assert_eq!(classify("911", "1"), DestinationClass::Emergency);
        assert_eq!(classify("sip:112@gw", "1"), DestinationClass::Emergency);
        assert_eq!(classify("sip:100", "1"), DestinationClass::Internal);
        assert_eq!(classify("100", "1"), DestinationClass::Internal);
        assert_eq!(classify("+14155550100", "1"), DestinationClass::National);
        assert_eq!(classify("+442071838750", "1"), DestinationClass::International);
        // Un-normalisable, non-internal alias falls through to International (conservative).
        assert_eq!(classify("sip:alice@host", "1"), DestinationClass::Internal); // sip alias -> internal
    }

    #[test]
    fn emergency_bypasses_even_maxed_concurrency() {
        let caps = caps(&[]);
        let mut req = request("911", false, &caps);
        req.active_calls = 99;
        let limits = PolicyLimits { allow_international: false, max_concurrent_calls: Some(1) };
        let d = evaluate(&req, &limits, "1");
        assert!(d.allowed());
        assert!(d.obligations.is_empty());
        assert_eq!(d.reason, "emergency call — policy bypassed");
    }

    #[test]
    fn international_denied_by_default() {
        let caps = caps(&[]);
        let req = request("+442071838750", true, &caps);
        let d = evaluate(&req, &PolicyLimits::default(), "1");
        assert!(!d.allowed());
        assert_eq!(d.effect, Effect::Deny);
        assert_eq!(d.reason, "international calling is not permitted by policy");
    }

    #[test]
    fn international_allowed_with_approval_when_policy_permits() {
        let caps = caps(&[]);
        let req = request("+442071838750", true, &caps);
        let limits = PolicyLimits { allow_international: true, max_concurrent_calls: None };
        let d = evaluate(&req, &limits, "1");
        assert!(d.allowed());
        assert_eq!(d.obligations, vec![Obligation::RequireApproval]);
        assert_eq!(d.reason, "international call requires approval");
    }

    #[test]
    fn international_allowed_outright_with_capability() {
        let caps = caps(&["calls.dial.international"]);
        // Has identity, so no REQUIRE_IDENTITY obligation either → a clean allow.
        let req = request("+442071838750", true, &caps);
        let d = evaluate(&req, &PolicyLimits::default(), "1");
        assert!(d.allowed());
        assert!(d.obligations.is_empty());
        assert_eq!(d.reason, "permitted");
    }

    #[test]
    fn national_without_identity_requires_identity() {
        let caps = caps(&[]);
        let req = request("+14155550100", false, &caps);
        let d = evaluate(&req, &PolicyLimits::default(), "1");
        assert!(d.allowed());
        assert_eq!(d.obligations, vec![Obligation::RequireIdentity]);
        assert_eq!(d.reason, "external call requires caller identity");
    }

    #[test]
    fn internal_call_is_a_clean_allow() {
        let caps = caps(&[]);
        // No identity, no capabilities — an internal extension needs neither.
        let req = request("sip:100", false, &caps);
        let d = evaluate(&req, &PolicyLimits::default(), "1");
        assert!(d.allowed());
        assert!(d.obligations.is_empty());
        assert_eq!(d.reason, "permitted");
    }

    #[test]
    fn concurrency_cap_denies() {
        let caps = caps(&[]);
        let mut req = request("+14155550100", true, &caps);
        req.active_calls = 3;
        let limits = PolicyLimits { allow_international: false, max_concurrent_calls: Some(3) };
        let d = evaluate(&req, &limits, "1");
        assert!(!d.allowed());
        assert_eq!(d.effect, Effect::Deny);
        assert!(d.reason.contains('3'));
    }

    #[test]
    fn decision_serialises_screaming_snake() {
        let d = Decision::allow(vec![Obligation::RequireApproval], "x");
        let json = serde_json::to_value(&d).unwrap();
        assert_eq!(json["effect"], "ALLOW");
        assert_eq!(json["obligations"][0], "REQUIRE_APPROVAL");
    }
}
