//! Rating engine (control plane) — turns a destination + billable duration into a
//! [`Money`] cost that feeds the CDR (Volume 10).
//!
//! This is the reference projection of the frozen Rating interface
//! (`contracts/json-schema/interfaces/RatingRequest.schema.json` /
//! `RatingResult.schema.json`): a `RatingRequest` carries a `destination` (E.164) and
//! `billable_ms`; a `RatingResult` carries the rated `cost` plus the `increment_ms`,
//! `minimum_ms` and `rounding` that produced it. In production those parameters come
//! from a versioned Rating profile (`rating_profile_id` / `rating_profile_version`) so a
//! CDR is reproducible; here they live in a small built-in prefix table.
//!
//! Everything is deterministic integer arithmetic — no floating point — so a given
//! `(destination, billable_ms)` always rates to the same minor-unit cost.

use commos_core::common::{Currency, Money};

/// How a billable duration is rounded to the nearest rating `increment_ms`
/// (`RatingResult.schema.json` `rounding`). All three modes are modelled to match the
/// contract; the built-in table uses `Up`, so the others are constructed by custom profiles.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rounding {
    /// Always round the duration up to the next whole increment.
    Up,
    /// Round to the nearest increment (ties round up).
    Nearest,
    /// Always round the duration down to a whole increment.
    Down,
}

/// A single tariff: the per-minute price plus the increment/minimum/rounding that shape
/// the billed duration. Mirrors the parameters a `RatingResult` reports back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rate {
    /// Price per whole minute, in currency minor units (e.g. `2` = 2 US cents/min).
    pub minor_units_per_minute: i64,
    /// Billing granularity: the duration is rounded to a multiple of this many ms.
    pub increment_ms: i64,
    /// Minimum billed duration in ms — a connected call bills at least this long.
    pub minimum_ms: i64,
    /// How the duration is rounded to the increment.
    pub rounding: Rounding,
}

impl Rate {
    /// A zero-cost tariff (internal / on-net destinations).
    const fn free() -> Self {
        Rate { minor_units_per_minute: 0, increment_ms: 60_000, minimum_ms: 0, rounding: Rounding::Up }
    }

    /// A standard per-minute tariff: 60-second increments, 60-second minimum, round up.
    const fn per_minute(minor_units_per_minute: i64) -> Self {
        Rate {
            minor_units_per_minute,
            increment_ms: 60_000,
            minimum_ms: 60_000,
            rounding: Rounding::Up,
        }
    }
}

/// Destination-aware rating engine: an E.164 prefix → [`Rate`] table with a default
/// (unknown international) tariff and an internal (on-net) tariff.
pub struct Rater {
    /// Longest-prefix-match table, keyed on E.164 dialling prefixes (e.g. `"+1"`).
    table: Vec<(&'static str, Rate)>,
    /// Tariff for an E.164 destination matching no table prefix.
    default_rate: Rate,
    /// Tariff for internal / non-E.164 destinations (`sip:` URIs, extensions).
    internal_rate: Rate,
}

impl Rater {
    /// The built-in reference table.
    ///
    /// **Illustrative placeholders only.** A real deployment resolves these from a
    /// versioned Rating profile (`RatingRequest.rating_profile_id` /
    /// `rating_profile_version`) rather than hard-coding tariffs. Every entry bills in
    /// 60-second increments with a 60-second minimum, rounding **UP** (see [`Rate::per_minute`]):
    ///
    /// | Prefix   | Destination            | Price      |
    /// |----------|------------------------|------------|
    /// | `+1900`  | US premium-rate        | 50¢/min    |
    /// | `+1`     | US / Canada            | 2¢/min     |
    /// | `+44`    | United Kingdom         | 5¢/min     |
    /// | `+91`    | India                  | 8¢/min     |
    /// | *(default)* | unknown international | 15¢/min |
    /// | *(internal)* | `sip:` / non-E.164 | 0          |
    ///
    /// `+1900` is longer and more specific than `+1`, so a `+1900…` premium number rates
    /// at 50¢/min — longest-prefix match wins over the shorter `+1` entry and the default.
    pub fn default_table() -> Self {
        Rater {
            table: vec![
                ("+1900", Rate::per_minute(50)),
                ("+1", Rate::per_minute(2)),
                ("+44", Rate::per_minute(5)),
                ("+91", Rate::per_minute(8)),
            ],
            default_rate: Rate::per_minute(15),
            internal_rate: Rate::free(),
        }
    }

    /// Rate `billable_ms` for `destination` into a [`Money`] cost (USD).
    ///
    /// The destination is normalised to a leading `+digits` E.164; anything else
    /// (`sip:` URIs, bare extensions) is treated as internal → 0. The tariff is the
    /// longest matching prefix (falling back to the default international rate). The raw
    /// duration is rounded to the tariff `increment_ms`, clamped up to `minimum_ms`, and
    /// priced as `ceil(billed_ms × price_per_minute / 60_000)` in pure integer math.
    pub fn rate(&self, destination: &str, billable_ms: u64) -> Money {
        let tariff = match normalise_destination(destination) {
            Some(e164) => self.lookup(&e164),
            None => self.internal_rate,
        };

        // A call with no billable time (never answered / unended) is always free — the
        // minimum charge only floors a call that actually connected (billable_ms > 0).
        let billed_ms = if billable_ms == 0 {
            0
        } else {
            let rounded = round_to_increment(billable_ms, tariff.increment_ms, tariff.rounding);
            rounded.max(tariff.minimum_ms.max(0) as u64)
        };

        // ceil(billed_ms * price / 60_000) via integer arithmetic (i128 avoids overflow
        // for any realistic call length). price is non-negative, so this never underflows.
        let numerator = billed_ms as i128 * tariff.minor_units_per_minute as i128;
        let minor_units = ceil_div(numerator, 60_000) as i64;

        Money {
            // `USD` is a valid ISO-4217 code, so the parse never fails.
            currency: Currency::parse("USD").expect("USD is a valid currency"),
            minor_units,
        }
    }

    /// Longest-prefix match against the table, or the default international tariff.
    fn lookup(&self, e164: &str) -> Rate {
        self.table
            .iter()
            .filter(|(prefix, _)| e164.starts_with(prefix))
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(_, rate)| *rate)
            .unwrap_or(self.default_rate)
    }
}

/// Extract a leading `+digits` E.164 from a destination reference.
///
/// Returns `Some("+<digits>")` when the reference begins with `+` followed by at least
/// one digit (stopping at the first non-digit); returns `None` for `sip:` URIs, bare
/// extensions, or anything else that is not dialable E.164 — those are billed as internal.
fn normalise_destination(destination: &str) -> Option<String> {
    let rest = destination.trim().strip_prefix('+')?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        None
    } else {
        Some(format!("+{digits}"))
    }
}

/// Round `ms` to a whole multiple of `increment_ms` using `rounding`.
///
/// A non-positive increment means "no rounding" (the raw value passes through).
fn round_to_increment(ms: u64, increment_ms: i64, rounding: Rounding) -> u64 {
    if increment_ms <= 0 {
        return ms;
    }
    let inc = increment_ms as u64;
    let remainder = ms % inc;
    if remainder == 0 {
        return ms;
    }
    let floor = ms - remainder;
    match rounding {
        Rounding::Down => floor,
        Rounding::Up => floor + inc,
        // Ties (exactly half an increment) round up.
        Rounding::Nearest => {
            if remainder * 2 >= inc {
                floor + inc
            } else {
                floor
            }
        }
    }
}

/// Ceiling division for non-negative integers (`numerator, denominator >= 0`,
/// `denominator > 0`).
fn ceil_div(numerator: i128, denominator: i128) -> i128 {
    (numerator + denominator - 1) / denominator
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plus_one_ninety_seconds_rates_two_minutes_up() {
        // +1 at 2¢/min, 60s increments rounding UP: 90s -> 120s -> 2 min -> 4¢.
        let cost = Rater::default_table().rate("+14155550100", 90_000);
        assert_eq!(cost.minor_units, 4);
        assert_eq!(cost.currency, Currency::parse("USD").unwrap());
    }

    #[test]
    fn minimum_charge_applies_to_short_call() {
        // 5s connected still bills the 60s minimum -> 1 min -> 2¢ on +1.
        assert_eq!(Rater::default_table().rate("+14155550100", 5_000).minor_units, 2);
    }

    #[test]
    fn internal_sip_destination_is_free() {
        assert_eq!(Rater::default_table().rate("sip:200", 600_000).minor_units, 0);
        // A bare extension is also internal.
        assert_eq!(Rater::default_table().rate("100", 600_000).minor_units, 0);
    }

    #[test]
    fn longest_prefix_beats_shorter_and_default() {
        let rater = Rater::default_table();
        // +1900 (premium, 50¢/min) is longer than +1 (2¢/min): 1 min -> 50¢, not 2¢.
        assert_eq!(rater.rate("+19005551234", 60_000).minor_units, 50);
        // Plain +1 still picks the 2¢ tariff.
        assert_eq!(rater.rate("+14155550100", 60_000).minor_units, 2);
        // An unknown country code falls back to the 15¢/min default.
        assert_eq!(rater.rate("+9995550100", 60_000).minor_units, 15);
    }

    #[test]
    fn other_countries_use_their_tariff() {
        let rater = Rater::default_table();
        // UK 5¢/min, 120s -> 2 min -> 10¢.
        assert_eq!(rater.rate("+442071838750", 120_000).minor_units, 10);
        // India 8¢/min, exactly 1 min -> 8¢.
        assert_eq!(rater.rate("+919812345678", 60_000).minor_units, 8);
    }

    #[test]
    fn rounding_modes_are_deterministic() {
        // 90s at 60s increment: DOWN -> 60s, NEAREST -> 120s (tie rounds up), UP -> 120s.
        assert_eq!(round_to_increment(90_000, 60_000, Rounding::Down), 60_000);
        assert_eq!(round_to_increment(90_000, 60_000, Rounding::Nearest), 120_000);
        assert_eq!(round_to_increment(90_000, 60_000, Rounding::Up), 120_000);
        // 80s: NEAREST rounds down (under half an increment past 60s).
        assert_eq!(round_to_increment(80_000, 60_000, Rounding::Nearest), 60_000);
        // Exact multiples pass through untouched.
        assert_eq!(round_to_increment(120_000, 60_000, Rounding::Up), 120_000);
    }

    #[test]
    fn zero_billable_is_always_free() {
        // An unended / never-answered call has no billable time: the minimum charge does
        // not apply, so even a paid prefix rates to 0.
        assert_eq!(Rater::default_table().rate("+14155550100", 0).minor_units, 0);
    }
}
