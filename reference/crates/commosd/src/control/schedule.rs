//! Business-hours schedules — pure open/closed evaluation, input validation and the DID
//! destination picker.
//!
//! Everything here is a pure function over an injected clock (`jiff::Timestamp`), so it is
//! unit-testable without a store or a wall clock. Time zones are IANA names resolved through
//! `jiff`'s bundled tzdb (no dependency on `/usr/share/zoneinfo` on minimal images).

use jiff::civil::{Date, Time, Weekday};
use jiff::tz::TimeZone;
use jiff::Timestamp;

use commos_core::entities::did::Did;
use commos_core::entities::schedule::{Holiday, Schedule, WeeklyRange};

/// Parse a lower-case three-letter day name.
pub fn parse_day(s: &str) -> Option<Weekday> {
    Some(match s {
        "mon" => Weekday::Monday,
        "tue" => Weekday::Tuesday,
        "wed" => Weekday::Wednesday,
        "thu" => Weekday::Thursday,
        "fri" => Weekday::Friday,
        "sat" => Weekday::Saturday,
        "sun" => Weekday::Sunday,
        _ => return None,
    })
}

/// Parse a strict `HH:MM` (00:00..=23:59) into minutes since local midnight.
pub fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.split_once(':')?;
    if h.len() != 2 || m.len() != 2 || !h.bytes().all(|b| b.is_ascii_digit()) || !m.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// Parse a strict `YYYY-MM-DD` date.
pub fn parse_date(s: &str) -> Option<Date> {
    if s.len() != 10 {
        return None;
    }
    s.parse::<Date>().ok()
}

/// Whether `name` is a known IANA timezone.
pub fn valid_timezone(name: &str) -> bool {
    TimeZone::get(name).is_ok()
}

/// Validate a schedule's content; `Err` carries a human-readable reason (-> 400 problem).
pub fn validate(name: &str, timezone: &str, weekly: &[WeeklyRange], holidays: &[Holiday]) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("name must not be empty".into());
    }
    if name.len() > 200 {
        return Err("name must be at most 200 characters".into());
    }
    if !valid_timezone(timezone) {
        return Err(format!("unknown IANA timezone: {timezone}"));
    }
    if weekly.len() > 256 || holidays.len() > 4096 {
        return Err("too many weekly ranges or holidays".into());
    }
    for r in weekly {
        if r.days.is_empty() {
            return Err("weekly range needs at least one day".into());
        }
        for d in &r.days {
            if parse_day(d).is_none() {
                return Err(format!("invalid day name '{d}' (expected mon..sun)"));
            }
        }
        let (Some(s), Some(e)) = (parse_hhmm(&r.start), parse_hhmm(&r.end)) else {
            return Err(format!("start/end must be HH:MM (got '{}'-'{}')", r.start, r.end));
        };
        if s == e {
            return Err("start and end must differ".into());
        }
    }
    for h in holidays {
        if parse_date(&h.date).is_none() {
            return Err(format!("invalid holiday date '{}' (expected YYYY-MM-DD)", h.date));
        }
    }
    Ok(())
}

fn range_hits(r: &WeeklyRange, day: Weekday) -> bool {
    r.days.iter().any(|d| parse_day(d) == Some(day))
}

/// Is `schedule` open at the UTC instant `utc_now`?
///
/// Open when the local time falls in any weekly range for its weekday and the local date is
/// not a holiday. A range with `end < start` spans midnight and belongs to its start day.
/// Empty `weekly` = always open (holidays still close). An unresolvable timezone fails open.
pub fn is_open(schedule: &Schedule, utc_now: Timestamp) -> bool {
    let Ok(tz) = TimeZone::get(&schedule.timezone) else {
        return true;
    };
    let local = utc_now.to_zoned(tz);
    let date = local.date();
    if schedule
        .holidays
        .iter()
        .any(|h| parse_date(&h.date) == Some(date))
    {
        return false;
    }
    if schedule.weekly.is_empty() {
        return true;
    }
    let t: Time = local.time();
    let now_min = t.hour() as u32 * 60 + t.minute() as u32;
    let today = local.weekday();
    let yesterday = today.wrapping_sub(1);
    schedule.weekly.iter().any(|r| {
        let (Some(s), Some(e)) = (parse_hhmm(&r.start), parse_hhmm(&r.end)) else {
            return false;
        };
        if s == e {
            return false;
        }
        if s < e {
            return range_hits(r, today) && now_min >= s && now_min < e;
        }
        // Spans midnight: the evening part belongs to today, the early-morning tail to the
        // range that started yesterday.
        (range_hits(r, today) && now_min >= s) || (range_hits(r, yesterday) && now_min < e)
    })
}

/// Local RFC 3339 rendering of `utc_now` in the schedule's timezone (UTC if unresolvable).
pub fn local_now_rfc3339(schedule: &Schedule, utc_now: Timestamp) -> String {
    let tz = TimeZone::get(&schedule.timezone).unwrap_or(TimeZone::UTC);
    utc_now.to_zoned(tz).strftime("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

/// The inbound routing decision for a DID: its `closed_destination_ref` while its schedule is
/// closed, else its `destination_ref`. No schedule (or none loaded), or no closed destination
/// configured, leaves the open destination in force so a half-configured DID never drops calls.
pub fn pick_destination(did: &Did, schedule: Option<&Schedule>, utc_now: Timestamp) -> String {
    if let Some(s) = schedule {
        if !is_open(s, utc_now) {
            if let Some(c) = did.closed_destination_ref.as_deref().filter(|c| !c.is_empty()) {
                return c.to_string();
            }
        }
    }
    did.destination_ref.clone()
}


// ---- Service (CRUD over the Store) --------------------------------------------------------

use std::sync::Arc;

use commos_core::common::Uuid;

use crate::store::{Page, Store, StoreError, Tx};

#[derive(Debug, thiserror::Error)]
pub enum ScheduleError {
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Partial update (PATCH): `None` = leave unchanged.
#[derive(Clone, Debug, Default)]
pub struct SchedulePatch {
    pub name: Option<String>,
    pub timezone: Option<String>,
    pub weekly: Option<Vec<WeeklyRange>>,
    pub holidays: Option<Vec<Holiday>>,
}

/// Schedule config service. `default_timezone` is the pbx.yaml `timezone` (used on create
/// when the request omits one, if it is a valid IANA name), else `UTC`.
#[derive(Clone)]
pub struct ScheduleService {
    store: Arc<dyn Store>,
    default_timezone: Option<String>,
}

impl ScheduleService {
    pub fn new(store: Arc<dyn Store>, default_timezone: Option<String>) -> Self {
        ScheduleService { store, default_timezone }
    }

    fn default_tz(&self) -> String {
        self.default_timezone
            .as_deref()
            .map(str::trim)
            .filter(|z| !z.is_empty() && valid_timezone(z))
            .unwrap_or("UTC")
            .to_string()
    }

    pub async fn create(
        &self,
        tenant: Uuid,
        name: String,
        timezone: Option<String>,
        weekly: Vec<WeeklyRange>,
        holidays: Vec<Holiday>,
    ) -> Result<Schedule, ScheduleError> {
        let tz = timezone.unwrap_or_else(|| self.default_tz());
        validate(&name, &tz, &weekly, &holidays).map_err(ScheduleError::Invalid)?;
        let mut s = Schedule::create(tenant, name, tz);
        s.weekly = weekly;
        s.holidays = holidays;
        self.store.commit(Tx { schedules: vec![s.clone()], ..Default::default() }).await?;
        Ok(s)
    }

    pub async fn get(&self, tenant: Uuid, id: Uuid) -> Result<Schedule, ScheduleError> {
        self.store.get_schedule(tenant, id).await?.ok_or(ScheduleError::NotFound)
    }

    pub async fn list(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Schedule>, ScheduleError> {
        Ok(self.store.list_schedules(tenant, limit, cursor).await?)
    }

    pub async fn patch(&self, tenant: Uuid, id: Uuid, p: SchedulePatch) -> Result<Schedule, ScheduleError> {
        let mut s = self.get(tenant, id).await?;
        if let Some(n) = p.name {
            s.name = n;
        }
        if let Some(z) = p.timezone {
            s.timezone = z;
        }
        if let Some(w) = p.weekly {
            s.weekly = w;
        }
        if let Some(h) = p.holidays {
            s.holidays = h;
        }
        validate(&s.name, &s.timezone, &s.weekly, &s.holidays).map_err(ScheduleError::Invalid)?;
        s.base.touch();
        self.store.commit(Tx { schedules: vec![s.clone()], ..Default::default() }).await?;
        Ok(s)
    }

    pub async fn delete(&self, tenant: Uuid, id: Uuid) -> Result<(), ScheduleError> {
        self.store.delete_schedule(tenant, id).await?.then_some(()).ok_or(ScheduleError::NotFound)
    }

    /// `(open, local-rfc3339-now)` at the given instant.
    pub async fn status(&self, tenant: Uuid, id: Uuid, now: Timestamp) -> Result<(bool, String), ScheduleError> {
        let s = self.get(tenant, id).await?;
        Ok((is_open(&s, now), local_now_rfc3339(&s, now)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use commos_core::common::Uuid;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn sched(tz: &str, weekly: Vec<(&[&str], &str, &str)>, holidays: Vec<&str>) -> Schedule {
        let mut s = Schedule::create(Uuid::now_v7(), "t", tz);
        s.weekly = weekly
            .into_iter()
            .map(|(d, a, b)| WeeklyRange { days: d.iter().map(|x| x.to_string()).collect(), start: a.into(), end: b.into() })
            .collect();
        s.holidays = holidays.into_iter().map(|d| Holiday { date: d.into(), name: None }).collect();
        s
    }

    const WEEKDAYS: &[&str] = &["mon", "tue", "wed", "thu", "fri"];

    #[test]
    fn weekday_range() {
        let s = sched("UTC", vec![(WEEKDAYS, "09:00", "17:30")], vec![]);
        // 2026-03-02 is a Monday.
        assert!(s.weekly.len() == 1);
        assert!(is_open(&s, ts("2026-03-02T09:00:00Z")));
        assert!(is_open(&s, ts("2026-03-02T17:29:59Z")));
        assert!(!is_open(&s, ts("2026-03-02T17:30:00Z")));
        assert!(!is_open(&s, ts("2026-03-02T08:59:59Z")));
        // Saturday.
        assert!(!is_open(&s, ts("2026-03-07T12:00:00Z")));
    }

    #[test]
    fn timezone_offsets_apply() {
        let s = sched("America/New_York", vec![(WEEKDAYS, "09:00", "17:00")], vec![]);
        // 14:00Z in March (EST, UTC-5 before DST on the 8th) = 09:00 local Monday.
        assert!(is_open(&s, ts("2026-03-02T14:00:00Z")));
        assert!(!is_open(&s, ts("2026-03-02T13:59:00Z")));
    }

    #[test]
    fn midnight_spanning_belongs_to_start_day() {
        // Friday 22:00 -> 02:00 (Saturday morning).
        let s = sched("UTC", vec![(&["fri"], "22:00", "02:00")], vec![]);
        // 2026-03-06 Friday, 2026-03-07 Saturday.
        assert!(!is_open(&s, ts("2026-03-06T21:59:00Z")));
        assert!(is_open(&s, ts("2026-03-06T23:00:00Z")));
        assert!(is_open(&s, ts("2026-03-07T01:59:00Z")));
        assert!(!is_open(&s, ts("2026-03-07T02:00:00Z")));
        // Saturday evening is not covered, nor Friday's early morning.
        assert!(!is_open(&s, ts("2026-03-07T23:00:00Z")));
        assert!(!is_open(&s, ts("2026-03-06T01:00:00Z")));
    }

    #[test]
    fn dst_transition_day_europe_london() {
        // 2026-03-29 is the last Sunday of March: clocks go 01:00 GMT -> 02:00 BST.
        let s = sched("Europe/London", vec![(&["sun"], "09:00", "17:00")], vec![]);
        // 08:30Z = 09:30 BST -> open (an offset-naive GMT reading would say 08:30, closed).
        assert!(is_open(&s, ts("2026-03-29T08:30:00Z")));
        // 16:30Z = 17:30 BST -> closed.
        assert!(!is_open(&s, ts("2026-03-29T16:30:00Z")));
        // The day before the change (GMT): 09:30Z = 09:30 local, Saturday -> closed (sun only).
        assert!(!is_open(&s, ts("2026-03-28T09:30:00Z")));
        let l = local_now_rfc3339(&s, ts("2026-03-29T08:30:00Z"));
        assert_eq!(l, "2026-03-29T09:30:00+01:00");
        // Midnight-spanning across the spring-forward gap still behaves.
        let n = sched("Europe/London", vec![(&["sat"], "22:00", "06:00")], vec![]);
        assert!(is_open(&n, ts("2026-03-29T00:30:00Z"))); // 00:30 GMT Sunday
        assert!(is_open(&n, ts("2026-03-29T04:30:00Z"))); // 05:30 BST Sunday
        assert!(!is_open(&n, ts("2026-03-29T05:30:00Z"))); // 06:30 BST
    }

    #[test]
    fn holiday_overrides() {
        let s = sched("Europe/London", vec![(WEEKDAYS, "09:00", "17:00")], vec!["2026-12-25"]);
        // Friday 2026-12-25 noon: would be open, but is a holiday.
        assert!(!is_open(&s, ts("2026-12-25T12:00:00Z")));
        assert!(is_open(&s, ts("2026-12-24T12:00:00Z")));
        // Holiday is judged on the LOCAL date: 23:30Z on the 24th is already the 25th in
        // Auckland.
        let a = sched("Pacific/Auckland", vec![], vec!["2026-12-25"]);
        assert!(!is_open(&a, ts("2026-12-24T12:00:00Z"))); // 25th 01:00 NZDT
        assert!(is_open(&a, ts("2026-12-25T12:00:00Z"))); // 26th 01:00 NZDT
    }

    #[test]
    fn empty_weekly_always_open_except_holidays() {
        let s = sched("UTC", vec![], vec!["2026-01-01"]);
        assert!(is_open(&s, ts("2026-06-06T03:00:00Z")));
        assert!(!is_open(&s, ts("2026-01-01T03:00:00Z")));
    }

    #[test]
    fn validation() {
        let ok = vec![WeeklyRange { days: vec!["mon".into()], start: "09:00".into(), end: "17:00".into() }];
        assert!(validate("a", "Europe/London", &ok, &[]).is_ok());
        assert!(validate("", "UTC", &ok, &[]).is_err());
        assert!(validate("a", "Mars/Base", &ok, &[]).is_err());
        let bad_day = vec![WeeklyRange { days: vec!["monday".into()], start: "09:00".into(), end: "17:00".into() }];
        assert!(validate("a", "UTC", &bad_day, &[]).is_err());
        let bad_t = vec![WeeklyRange { days: vec!["mon".into()], start: "9:00".into(), end: "17:00".into() }];
        assert!(validate("a", "UTC", &bad_t, &[]).is_err());
        let bad_t2 = vec![WeeklyRange { days: vec!["mon".into()], start: "09:00".into(), end: "24:00".into() }];
        assert!(validate("a", "UTC", &bad_t2, &[]).is_err());
        let hol = vec![Holiday { date: "2026-13-40".into(), name: None }];
        assert!(validate("a", "UTC", &[], &hol).is_err());
    }

    #[test]
    fn picks_closed_destination() {
        let t = Uuid::now_v7();
        let mut d = Did::new(t, "+14155550100", Uuid::now_v7(), "sip:200");
        let s = sched("UTC", vec![(WEEKDAYS, "09:00", "17:00")], vec![]);
        let open = ts("2026-03-02T10:00:00Z");
        let closed = ts("2026-03-02T20:00:00Z");
        // No closed destination configured -> open destination always.
        assert_eq!(pick_destination(&d, Some(&s), closed), "sip:200");
        d.closed_destination_ref = Some("voicemail".into());
        assert_eq!(pick_destination(&d, Some(&s), open), "sip:200");
        assert_eq!(pick_destination(&d, Some(&s), closed), "voicemail");
        // No schedule -> unchanged behaviour.
        assert_eq!(pick_destination(&d, None, closed), "sip:200");
    }

    #[tokio::test]
    async fn service_crud_and_validation() {
        use crate::store::MemStore;
        let svc = ScheduleService::new(Arc::new(MemStore::new()), Some("Europe/London".into()));
        let t = Uuid::now_v7();
        let w = vec![WeeklyRange { days: vec!["mon".into(), "tue".into()], start: "09:00".into(), end: "17:30".into() }];
        let h = vec![Holiday { date: "2026-12-25".into(), name: Some("Christmas".into()) }];
        let s = svc.create(t, "Office hours".into(), None, w.clone(), h).await.unwrap();
        assert_eq!(s.timezone, "Europe/London"); // configured default
        assert_eq!(s.base.version, 0);

        // Invalid inputs -> Invalid.
        assert!(matches!(svc.create(t, "x".into(), Some("Nope/Zone".into()), vec![], vec![]).await, Err(ScheduleError::Invalid(_))));
        let bad = vec![WeeklyRange { days: vec!["funday".into()], start: "09:00".into(), end: "10:00".into() }];
        assert!(matches!(svc.create(t, "x".into(), None, bad, vec![]).await, Err(ScheduleError::Invalid(_))));

        // Unconfigured / non-IANA default falls back to UTC.
        let svc2 = ScheduleService::new(Arc::new(MemStore::new()), Some("CET-1CEST".into()));
        assert_eq!(svc2.create(t, "x".into(), None, vec![], vec![]).await.unwrap().timezone, "UTC");

        // PATCH is partial.
        let p = svc.patch(t, s.base.id, SchedulePatch { name: Some("Renamed".into()), ..Default::default() }).await.unwrap();
        assert_eq!(p.name, "Renamed");
        assert_eq!(p.weekly, w);
        assert_eq!(p.base.version, 1);
        assert!(matches!(
            svc.patch(t, s.base.id, SchedulePatch { timezone: Some("bad".into()), ..Default::default() }).await,
            Err(ScheduleError::Invalid(_))
        ));

        // Status: Monday 2026-03-02 10:00Z = 10:00 GMT -> open.
        let (open, now) = svc.status(t, s.base.id, ts("2026-03-02T10:00:00Z")).await.unwrap();
        assert!(open);
        assert_eq!(now, "2026-03-02T10:00:00+00:00");
        assert!(!svc.status(t, s.base.id, ts("2026-03-02T18:00:00Z")).await.unwrap().0);

        assert_eq!(svc.list(t, 10, None).await.unwrap().items.len(), 1);
        svc.delete(t, s.base.id).await.unwrap();
        assert!(matches!(svc.get(t, s.base.id).await, Err(ScheduleError::NotFound)));
        assert!(matches!(svc.delete(t, s.base.id).await, Err(ScheduleError::NotFound)));
    }
}
