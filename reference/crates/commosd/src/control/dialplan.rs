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
    normalize_dialed(input, &DialProfile::for_country(default_country_code))
}

/// How a trunk/deployment interprets what a user dials: the country the dialling is local
/// to, the national trunk prefix, the international access prefix and an optional
/// outside-line prefix (`9`) stripped before anything else. All fields are digit strings
/// (no `+`); an empty prefix disables that rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialProfile {
    pub country_code: String,
    pub national_prefix: String,
    pub international_prefix: String,
    pub strip_prefix: String,
}

impl DialProfile {
    /// Defaults for a country code (`"1"`, `"+44"`): NANP (`1`) uses national prefix `1` and
    /// international prefix `011`; everywhere else `0` / `00`. No outside-line prefix.
    pub fn for_country(country_code: &str) -> Self {
        let cc: String = country_code.chars().filter(char::is_ascii_digit).collect();
        let nanp = cc == "1";
        DialProfile {
            national_prefix: if nanp { "1" } else { "0" }.into(),
            international_prefix: if nanp { "011" } else { "00" }.into(),
            country_code: cc,
            strip_prefix: String::new(),
        }
    }

    /// Build the effective profile for a gateway: start from `default_cc`, switch to the
    /// gateway's `country_code` (re-deriving the prefix defaults for that country), then apply
    /// any explicit prefix overrides. `None`/blank overrides keep the default.
    pub fn with_overrides(
        default_cc: &str,
        country_code: Option<&str>,
        national_prefix: Option<&str>,
        international_prefix: Option<&str>,
        strip_prefix: Option<&str>,
    ) -> Self {
        fn pick(v: Option<&str>) -> Option<&str> {
            v.map(str::trim).filter(|v| !v.is_empty())
        }
        let mut p = Self::for_country(pick(country_code).unwrap_or(default_cc));
        let digits = |v: &str| v.chars().filter(char::is_ascii_digit).collect::<String>();
        if let Some(v) = pick(national_prefix) {
            p.national_prefix = digits(v);
        }
        if let Some(v) = pick(international_prefix) {
            p.international_prefix = digits(v);
        }
        if let Some(v) = pick(strip_prefix) {
            p.strip_prefix = digits(v);
        }
        p
    }
}

/// Normalise a destination as a *user dials it* to E.164 under `profile`, or `None` when it
/// is not a PSTN number (an internal extension, a feature code, or too short).
///
/// Accepted shapes (after separators are stripped), in order:
/// 1. optional outside-line prefix (`profile.strip_prefix`, e.g. `9`) — stripped only when
///    what follows is clearly a number (starts with `+`, the international or national
///    prefix, or the whole string is 11+ digits), so a bare national number that merely
///    begins with that digit (`9876543210` in India) is left alone;
/// 2. `+<cc><number>` — kept as is;
/// 3. `00<cc><number>` or `<international_prefix><cc><number>` (`011` in NANP) → `+…`;
/// 4. `<national_prefix><number>` (`07700 900123`, `1 415 555 0100`) → `+<cc><number>`;
/// 5. a bare national number — 10 digits in NANP, 8–10 digits elsewhere → `+<cc><number>`;
/// 6. 11+ digits that already carry a country code → `+<digits>`.
///
/// Anything shorter (extensions, 3-digit emergency numbers) is `None`, so internal
/// extensions are never reinterpreted as PSTN numbers.
pub fn normalize_dialed(input: &str, profile: &DialProfile) -> Option<String> {
    let cleaned = clean_user_part(input);
    if cleaned.is_empty() {
        return None;
    }
    let cc = profile.country_code.as_str();
    let (nat, intl) = (
        profile.national_prefix.as_str(),
        profile.international_prefix.as_str(),
    );

    // 1. Outside-line prefix.
    let mut s: &str = &cleaned;
    let sp = profile.strip_prefix.as_str();
    if !sp.is_empty() {
        if let Some(rest) = s.strip_prefix(sp) {
            let digits_rest = rest.trim_start_matches('+');
            let looks_dialable = rest.starts_with('+')
                || rest.starts_with("00")
                || (!intl.is_empty() && rest.starts_with(intl))
                || (!nat.is_empty() && rest.starts_with(nat))
                || s.len() >= 11;
            if digits_rest.len() >= 7 && looks_dialable {
                s = rest;
            }
        }
    }

    // 2. Already E.164.
    if let Some(rest) = s.strip_prefix('+') {
        return leading_digits(rest).map(|d| format!("+{d}"));
    }

    // 3. International access prefix ('00' always; the profile's own too).
    for prefix in ["00", intl] {
        if prefix.is_empty() {
            continue;
        }
        if let Some(rest) = s.strip_prefix(prefix) {
            return leading_digits(rest)
                .filter(|d| d.len() >= 8 && d.len() == rest.len())
                .map(|d| format!("+{d}"));
        }
    }

    // Everything below needs a plain digit string.
    let digits = leading_digits(s)?;
    if digits.len() != s.len() {
        return None; // contains `*`, `#` or letters: a feature code / non-numeric user
    }

    // 4. National trunk prefix (and the legacy "0" trunk for countries configured otherwise).
    for prefix in [nat, "0"] {
        if prefix.is_empty() {
            continue;
        }
        if let Some(national) = digits.strip_prefix(prefix) {
            return (national.len() >= 7 && !cc.is_empty()).then(|| format!("+{cc}{national}"));
        }
    }

    // 6. A full international number that already carries its country code (11+ digits).
    if digits.len() >= 11 {
        return Some(format!("+{digits}"));
    }

    // 5. Bare national number without a trunk prefix.
    let national_len = if cc == "1" {
        digits.len() == 10
    } else {
        (8..=10).contains(&digits.len())
    };
    if national_len && !cc.is_empty() {
        return Some(format!("+{cc}{digits}"));
    }

    // Anything shorter is an on-net extension.
    None
}

/// Unwrap a name-addr / SIP URI to its user part and drop visual separators.
fn clean_user_part(input: &str) -> String {
    let addr = match (input.find('<'), input.find('>')) {
        (Some(open), Some(close)) if close > open => input[open + 1..close].trim(),
        _ => input.trim(),
    };
    strip_scheme_and_host(addr)
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '(' | ')' | '.'))
        .collect()
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

    fn profile(cc: &str, strip: &str) -> DialProfile {
        DialProfile::with_overrides("1", Some(cc), None, None, (!strip.is_empty()).then_some(strip))
    }

    #[test]
    fn dial_table_by_country() {
        // (country code, outside-line prefix, dialled, expected E.164)
        let table: &[(&str, &str, &str, Option<&str>)] = &[
            // UK
            ("44", "", "07700 900123", Some("+447700900123")),
            ("44", "", "020 7183 8750", Some("+442071838750")),
            ("44", "", "00 1 415 555 0100", Some("+14155550100")),
            ("44", "", "+44 20 7183 8750", Some("+442071838750")),
            ("44", "9", "9 07700 900123", Some("+447700900123")),
            ("44", "9", "9 00 33 1 23 45 67 89", Some("+33123456789")),
            ("44", "", "999", None),
            ("44", "", "2001", None),
            // US / NANP
            ("1", "", "(415) 555-0100", Some("+14155550100")),
            ("1", "", "1-415-555-0100", Some("+14155550100")),
            ("1", "", "011 44 20 7183 8750", Some("+442071838750")),
            ("1", "", "+1 415 555 0100", Some("+14155550100")),
            ("1", "9", "9 1 415 555 0100", Some("+14155550100")),
            ("1", "9", "9 415 555 0100", Some("+14155550100")),
            ("1", "9", "9 011 44 20 7183 8750", Some("+442071838750")),
            // a 9xx area code dialled without the outside-line 9 is not mangled
            ("1", "9", "972 555 0100", Some("+19725550100")),
            ("1", "", "911", None),
            ("1", "", "5550100", None), // 7-digit local: no area code to resolve
            // Germany
            ("49", "", "030 12345678", Some("+493012345678")),
            ("49", "", "0049 30 12345678", Some("+493012345678")),
            ("49", "0", "0 0171 2345678", Some("+491712345678")),
            // India
            ("91", "", "098765 43210", Some("+919876543210")),
            ("91", "", "9876543210", Some("+919876543210")),
            ("91", "9", "9 9876543210", Some("+919876543210")),
            ("91", "", "+91 98765 43210", Some("+919876543210")),
            // feature codes / extensions are never PSTN
            ("44", "", "*78", None),
            ("44", "", "*721001", None),
            ("44", "", "1001", None),
        ];
        for (cc, strip, dialled, want) in table {
            assert_eq!(
                normalize_dialed(dialled, &profile(cc, strip)).as_deref(),
                *want,
                "cc={cc} strip={strip:?} dialled={dialled:?}"
            );
        }
    }

    #[test]
    fn sip_uri_dialled_digits_normalise() {
        let uk = profile("44", "");
        assert_eq!(
            normalize_dialed("sip:07700900123@10.0.0.1", &uk).as_deref(),
            Some("+447700900123")
        );
        assert_eq!(normalize_dialed("sip:2001@10.0.0.1", &uk), None);
    }

    #[test]
    fn overrides_rederive_prefix_defaults() {
        let p = DialProfile::with_overrides("44", Some("+1"), None, None, None);
        assert_eq!((p.national_prefix.as_str(), p.international_prefix.as_str()), ("1", "011"));
        let p = DialProfile::with_overrides("1", None, Some("0"), Some("00"), Some("9"));
        assert_eq!(p.country_code, "1");
        assert_eq!(p.national_prefix, "0");
        assert_eq!(p.international_prefix, "00");
        assert_eq!(p.strip_prefix, "9");
    }

    #[test]
    fn default_cc_normalisation_handles_nanp_and_011() {
        assert_eq!(normalize_e164("4155550100", "1").as_deref(), Some("+14155550100"));
        assert_eq!(normalize_e164("011442071838750", "+1").as_deref(), Some("+442071838750"));
    }
}
