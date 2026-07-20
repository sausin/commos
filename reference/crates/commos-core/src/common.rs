//! Shared primitives — the Rust projection of `contracts/json-schema/common.schema.json`.
//!
//! Every definition here is normative per `spec/CONVENTIONS.md` §6. The types carry
//! their schema constraints in code so the rest of the implementation cannot construct
//! an out-of-contract value by accident.

use serde::{Deserialize, Serialize};
use std::fmt;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::error::CoreError;

/// UUIDv7, lowercase canonical form (`common.schema.json#/$defs/Uuid`).
///
/// Time-ordered ids are mandated so that natural key order matches creation order
/// (Volume 2 CMOS-02-DOM-005). Wrapping `uuid::Uuid` keeps the version-7 guarantee at
/// the type boundary — a `Uuid` value in this system is always v7.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Uuid(uuid::Uuid);

impl Uuid {
    /// Mint a fresh time-ordered id.
    pub fn now_v7() -> Self {
        Uuid(uuid::Uuid::now_v7())
    }

    /// Parse and validate that the value is a v7 UUID in canonical lowercase form.
    pub fn parse(s: &str) -> Result<Self, CoreError> {
        let parsed = uuid::Uuid::parse_str(s).map_err(|_| CoreError::invalid("Uuid", s))?;
        if parsed.get_version_num() != 7 {
            return Err(CoreError::invalid("Uuid", "not a version-7 UUID"));
        }
        if s != parsed.hyphenated().to_string() {
            return Err(CoreError::invalid("Uuid", "not canonical lowercase"));
        }
        Ok(Uuid(parsed))
    }
}

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `Hyphenated` renders lowercase canonical form.
        write!(f, "{}", self.0.hyphenated())
    }
}

impl fmt::Debug for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Uuid({})", self.0.hyphenated())
    }
}

/// RFC 3339 UTC timestamp with millisecond precision and a trailing `Z`
/// (`common.schema.json#/$defs/Timestamp`). The wire format is pinned so that
/// serialisation is deterministic and diffable (config-as-code, CMOS-14-DEP-081).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp(OffsetDateTime);

impl Timestamp {
    /// Current instant, truncated to millisecond precision (the contract's resolution).
    pub fn now() -> Self {
        let now = OffsetDateTime::now_utc();
        let millis = now.millisecond();
        // Drop sub-millisecond nanoseconds so the value round-trips through the contract.
        let truncated = now.replace_nanosecond(millis as u32 * 1_000_000).expect("in range");
        Timestamp(truncated)
    }

    fn render(&self) -> String {
        // time's Rfc3339 emits offset `+00:00`; the contract requires exactly `.mmmZ`.
        let s = self.0.format(&Rfc3339).expect("valid datetime");
        // Normalise: force 3-digit millis and a `Z` suffix.
        let base = s.split(['+', 'Z']).next().unwrap_or(&s);
        let (secs, frac) = match base.split_once('.') {
            Some((a, b)) => (a, b),
            None => (base, ""),
        };
        let mut millis: String = frac.chars().take(3).collect();
        while millis.len() < 3 {
            millis.push('0');
        }
        format!("{secs}.{millis}Z")
    }

    /// Parse a contract-shaped timestamp.
    pub fn parse(s: &str) -> Result<Self, CoreError> {
        let dt = OffsetDateTime::parse(s, &Rfc3339).map_err(|_| CoreError::invalid("Timestamp", s))?;
        Ok(Timestamp(dt))
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.render())
    }
}

impl fmt::Debug for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Timestamp({})", self.render())
    }
}

impl Serialize for Timestamp {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.render())
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Timestamp::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// ISO 4217 alphabetic currency code (`common.schema.json#/$defs/Currency`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Currency(String);

impl Currency {
    pub fn parse(s: &str) -> Result<Self, CoreError> {
        if s.len() == 3 && s.chars().all(|c| c.is_ascii_uppercase()) {
            Ok(Currency(s.to_string()))
        } else {
            Err(CoreError::invalid("Currency", s))
        }
    }
}

/// Money as integer minor units — never floating point (`common.schema.json#/$defs/Money`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Money {
    pub currency: Currency,
    pub minor_units: i64,
}

/// Fields every persisted entity carries (`common.schema.json#/$defs/EntityBase`,
/// Volume 2 CMOS-02-DOM-001/005). Flattened into each entity so the wire shape matches
/// the schema's `allOf: [EntityBase]` exactly.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EntityBase {
    pub id: Uuid,
    pub tenant_id: Uuid,
    /// Monotonic Digital-Twin version (optimistic-concurrency + Time-Machine history).
    pub version: u64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl EntityBase {
    /// Create the base for a brand-new entity: version 0, timestamps equal.
    pub fn new(tenant_id: Uuid) -> Self {
        let now = Timestamp::now();
        EntityBase {
            id: Uuid::now_v7(),
            tenant_id,
            version: 0,
            created_at: now,
            updated_at: now,
        }
    }

    /// Advance to the next version on mutation (CMOS-02-DOM-005 optimistic concurrency).
    pub fn touch(&mut self) {
        self.version += 1;
        self.updated_at = Timestamp::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex_lite::Regex;

    #[test]
    fn uuid_is_v7_lowercase_canonical() {
        let id = Uuid::now_v7();
        let s = id.to_string();
        // Round-trips through the contract validator.
        assert!(Uuid::parse(&s).is_ok());
        // Matches the schema pattern (version nibble is 7).
        let re = Regex::new(
            r"^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$",
        )
        .unwrap();
        assert!(re.is_match(&s), "{s} must match the Uuid schema pattern");
    }

    #[test]
    fn timestamp_matches_contract_pattern() {
        let ts = Timestamp::now().to_string();
        let re = Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$").unwrap();
        assert!(re.is_match(&ts), "{ts} must match the Timestamp schema pattern");
        // Round-trips.
        assert!(Timestamp::parse(&ts).is_ok());
    }
}
