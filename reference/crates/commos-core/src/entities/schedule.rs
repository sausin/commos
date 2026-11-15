//! `Schedule` entity — business hours + holidays for open/closed routing.
//!
//! A schedule is configuration (no lifecycle). It is evaluated in its IANA `timezone`; the
//! evaluation lives in the daemon (`control/schedule.rs`) so this crate stays tz-free.
//! A [`Did`](super::did::Did) may reference one via `schedule_id`.

use serde::{Deserialize, Serialize};

use crate::common::{EntityBase, Uuid};

/// One weekly opening range: `days` x `[start, end)` local time (`HH:MM`). `end < start`
/// spans midnight and belongs to the start day.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WeeklyRange {
    /// Lower-case three-letter day names (`mon` ... `sun`).
    pub days: Vec<String>,
    pub start: String,
    pub end: String,
}

/// A holiday: the whole local `date` (`YYYY-MM-DD`) is closed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holiday {
    pub date: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Schedule {
    #[serde(flatten)]
    pub base: EntityBase,
    pub name: String,
    /// IANA timezone name.
    pub timezone: String,
    /// Empty = always open (except holidays).
    #[serde(default)]
    pub weekly: Vec<WeeklyRange>,
    #[serde(default)]
    pub holidays: Vec<Holiday>,
}

impl Schedule {
    pub fn create(tenant: Uuid, name: impl Into<String>, timezone: impl Into<String>) -> Self {
        Schedule {
            base: EntityBase::new(tenant),
            name: name.into(),
            timezone: timezone.into(),
            weekly: Vec::new(),
            holidays: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let mut s = Schedule::create(Uuid::now_v7(), "Office", "Europe/London");
        s.weekly.push(WeeklyRange { days: vec!["mon".into()], start: "09:00".into(), end: "17:00".into() });
        s.holidays.push(Holiday { date: "2026-12-25".into(), name: Some("Christmas".into()) });
        let j = serde_json::to_value(&s).unwrap();
        assert_eq!(j["timezone"], "Europe/London");
        assert!(j.get("id").is_some());
        let back: Schedule = serde_json::from_value(j).unwrap();
        assert_eq!(back.weekly.len(), 1);
        assert_eq!(back.holidays[0].date, "2026-12-25");
    }
}
