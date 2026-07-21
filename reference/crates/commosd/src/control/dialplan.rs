//! Dialplan — destination normalisation (control plane).
//!
//! Turns a raw call destination (`Call.to_ref`: a SIP/`tel` URI, a name-addr, or a bare
//! string) into a canonical E.164 number (`+<digits>`) when the target is an off-net PSTN
//! number, or `None` when the target is internal (an on-net extension / non-dialable ref).
//!
//! This is the single place that decides "is this destination a real, billable phone
//! number, and if so what is its canonical form?". [`crate::control::billing`] rates and
//! records the *normalised* number so that a SIP-URI destination such as
//! `sip:+14155550100@gw` bills like the E.164 `+14155550100` it names, instead of falling
//! through to the internal (0-cost) tariff.
//!
//! The rules are deliberately conservative and deterministic — a given input always maps to
//! the same output — so a CDR is reproducible:
//!
//! * `sip:+14155550100@host`, `tel:+14155550100`, `<sip:+14155550100@host>` (name-addr)
//!   → `+14155550100` — pull the URI user part, keep the leading `+`.
//! * a bare `+14155550100` → unchanged.
//! * a URI/user part that is all digits and already carries a country code
//!   (`sip:14155550100@host`, 11+ digits) → prepend `+` → `+14155550100`.
//! * an international-access-prefixed number (`0014155550100`, `00` + 8+ digits)
//!   → replace the `00` with `+`.
//! * a national number with a leading trunk `0` (`02071838750`, 7+ digits after the `0`)
//!   → strip the `0` and apply `default_country_code` → `+44…`.
//! * a short extension (`sip:100@host`, `100`) → `None` (internal).
//!
//! Visual separators (spaces, `-`, `(`, `)`, `.`) are stripped before classification.

/// Normalise a raw destination `input` to canonical E.164 (`+<digits>`), or `None` when the
/// destination is internal (an on-net extension / non-dialable reference).
///
/// `default_country_code` (E.164 form, e.g. `"+1"`; a bare `"1"` is also accepted) supplies
/// the country code for a national number that carries a leading trunk `0` but no country
/// code. See the module docs for the full rule set. The mapping is total and deterministic.
pub fn normalize_e164(input: &str, default_country_code: &str) -> Option<String> {
    // 1. Unwrap a name-addr ("Display" <sip:user@host>) to its addr-spec.
    let addr = match (input.find('<'), input.find('>')) {
        (Some(open), Some(close)) if close > open => input[open + 1..close].trim(),
        _ => input.trim(),
    };

    // 2. Strip a URI scheme + host, leaving the user part (`sip:`/`sips:`/`tel:`).
    let user = strip_scheme_and_host(addr);

    // 3. Drop visual separators so the classifier sees only significant characters.
    let cleaned: String = user
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '(' | ')' | '.'))
        .collect();
    if cleaned.is_empty() {
        return None;
    }

    // 4a. Already E.164: '+' then digits. Keep only the leading run of digits.
    if let Some(rest) = cleaned.strip_prefix('+') {
        return leading_digits(rest).map(|d| format!("+{d}"));
    }

    // 4b. International access prefix ('00' + country code + number) → '+…'.
    if let Some(rest) = cleaned.strip_prefix("00") {
        return leading_digits(rest)
            .filter(|d| d.len() >= 8)
            .map(|d| format!("+{d}"));
    }

    // 4c. Otherwise it must be an all-digit string to be a candidate number.
    let Some(digits) = leading_digits(&cleaned) else {
        return None;
    };
    if digits.len() != cleaned.len() {
        // Contained a non-digit that was not a separator/scheme — not a plain number.
        return None;
    }

    // National number with a trunk '0' but no country code → apply the default CC.
    if let Some(national) = digits.strip_prefix('0') {
        if national.len() >= 7 {
            let cc = default_country_code.trim().trim_start_matches('+');
            return Some(format!("+{cc}{national}"));
        }
        return None;
    }

    // A full international number that already carries its country code (11+ digits).
    if digits.len() >= 11 {
        return Some(format!("+{digits}"));
    }

    // Anything shorter is an on-net extension.
    None
}

/// `true` when `destination` is internal (on-net): it has no E.164 normalisation.
///
/// Uses `"+1"` as the default country code — internal/extension classification does not
/// depend on the country code (only trunk-`0` expansion does), so the choice is immaterial.
#[allow(dead_code)] // public companion to `normalize_e164`; used by callers/tests as the surface grows.
pub fn is_internal(destination: &str) -> bool {
    normalize_e164(destination, "+1").is_none()
}

/// Strip a `sip:` / `sips:` / `tel:` scheme and any `@host` (and `;params`) from `addr`,
/// returning the user part. A non-URI string is returned unchanged.
fn strip_scheme_and_host(addr: &str) -> &str {
    let after_scheme = addr
        .strip_prefix("sips:")
        .or_else(|| addr.strip_prefix("sip:"))
        .or_else(|| addr.strip_prefix("tel:"))
        .unwrap_or(addr);
    // User part ends at '@' (host) if present, and never includes URI parameters.
    let user = after_scheme.split('@').next().unwrap_or(after_scheme);
    user.split(';').next().unwrap_or(user)
}

/// The leading run of ASCII digits of `s`, or `None` when `s` starts with a non-digit /
/// is empty.
fn leading_digits(s: &str) -> Option<String> {
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        None
    } else {
        Some(digits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sip_uri_with_plus_keeps_e164() {
        assert_eq!(
            normalize_e164("sip:+14155550100@gw", "+1").as_deref(),
            Some("+14155550100")
        );
    }

    #[test]
    fn tel_uri_with_plus() {
        assert_eq!(
            normalize_e164("tel:+14155550100", "+1").as_deref(),
            Some("+14155550100")
        );
    }

    #[test]
    fn name_addr_angle_brackets() {
        assert_eq!(
            normalize_e164("\"Alice\" <sip:+14155550100@host>", "+1").as_deref(),
            Some("+14155550100")
        );
        // Bare angle-bracket addr-spec too.
        assert_eq!(
            normalize_e164("<sip:+14155550100@host>", "+1").as_deref(),
            Some("+14155550100")
        );
    }

    #[test]
    fn bare_e164_unchanged() {
        assert_eq!(
            normalize_e164("+14155550100", "+1").as_deref(),
            Some("+14155550100")
        );
    }

    #[test]
    fn all_digit_full_number_gets_plus() {
        // 11 digits including the country code → prepend '+'.
        assert_eq!(
            normalize_e164("sip:14155550100@host", "+1").as_deref(),
            Some("+14155550100")
        );
    }

    #[test]
    fn short_extension_is_internal() {
        assert_eq!(normalize_e164("sip:100@host", "+1"), None);
        assert_eq!(normalize_e164("100", "+1"), None);
        assert!(is_internal("sip:100@host"));
        assert!(is_internal("100"));
    }

    #[test]
    fn visual_separators_are_stripped() {
        assert_eq!(
            normalize_e164("+1 (415) 555-0100", "+1").as_deref(),
            Some("+14155550100")
        );
    }

    #[test]
    fn international_access_prefix_becomes_plus() {
        // 00 + country code + number → '+…'.
        assert_eq!(
            normalize_e164("sip:0014155550100@gw", "+1").as_deref(),
            Some("+14155550100")
        );
    }

    #[test]
    fn national_trunk_zero_applies_default_cc() {
        // UK national format with trunk 0, default CC +44 → strip 0, prepend +44.
        assert_eq!(
            normalize_e164("02071838750", "+44").as_deref(),
            Some("+442071838750")
        );
        // Bare "1" country code form is accepted too.
        assert_eq!(
            normalize_e164("04155550100", "1").as_deref(),
            Some("+14155550100")
        );
    }

    #[test]
    fn is_internal_matches_normalize() {
        assert!(!is_internal("+14155550100"));
        assert!(!is_internal("sip:+14155550100@gw"));
        assert!(is_internal("sip:alice@host")); // alphanumeric user → not a number
    }

    #[test]
    fn alphanumeric_user_is_internal() {
        assert_eq!(normalize_e164("sip:alice@host", "+1"), None);
        // Digits followed by letters are not a plain number.
        assert_eq!(normalize_e164("sip:100x@host", "+1"), None);
    }
}
