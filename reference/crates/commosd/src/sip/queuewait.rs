//! Queue-wait treatment policy — the pure timing/selection decisions the queue-wait SIP
//! driver ([`super::server`]) makes while a caller holds for an agent.
//!
//! A caller who reaches a `queue:<uuid>` target is answered immediately and then, instead of
//! silence, hears a greeting, music-on-hold, and periodic "please continue to hold"
//! announcements while CommOS repeatedly tries to place them with an available member. Two
//! decisions drive that loop and are worth isolating from the socket plumbing so they can be
//! unit-tested: *when has the caller waited long enough to overflow?* and *when is the next
//! announcement due?* — plus *which member to try next* (a round-robin skip over the members
//! that currently have a live registration).

use std::time::Duration;

/// Default cadence for the periodic "you are still in a queue" announcement.
const DEFAULT_ANNOUNCE_EVERY: Duration = Duration::from_secs(30);
/// How often the driver breaks out of music-on-hold to try placing the caller with a member.
const DEFAULT_POLL_EVERY: Duration = Duration::from_secs(5);

/// Timing knobs for one queue's wait treatment, derived from the [`Queue`] config.
///
/// [`Queue`]: commos_core::entities::queue::Queue
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitConfig {
    /// Maximum time a caller waits before overflowing (from `Queue.max_wait_ms`). `None` → wait
    /// indefinitely (until an agent frees or the caller hangs up).
    pub max_wait: Option<Duration>,
    /// How often to play the "still holding" announcement.
    pub announce_every: Duration,
    /// How often to break out of hold music to attempt a delivery to a member.
    pub poll_every: Duration,
}

impl WaitConfig {
    /// Build from a queue's `max_wait_ms` (a non-positive value means "no limit"), using the
    /// default announcement/poll cadences.
    pub fn from_max_wait_ms(max_wait_ms: Option<i64>) -> Self {
        let max_wait = match max_wait_ms {
            Some(ms) if ms > 0 => Some(Duration::from_millis(ms as u64)),
            _ => None,
        };
        WaitConfig {
            max_wait,
            announce_every: DEFAULT_ANNOUNCE_EVERY,
            poll_every: DEFAULT_POLL_EVERY,
        }
    }

    /// Whether the caller has now waited past `max_wait` and should be overflowed.
    pub fn overflow_due(&self, elapsed: Duration) -> bool {
        self.max_wait.is_some_and(|m| elapsed >= m)
    }

    /// Whether the next announcement is due, given how many have already played. The `n`-th
    /// announcement (1-indexed) plays once `elapsed` reaches `n * announce_every`.
    pub fn announce_due(&self, elapsed: Duration, played: u32) -> bool {
        match self.announce_every.checked_mul(played + 1) {
            Some(threshold) => elapsed >= threshold,
            None => false, // overflowed the multiply → effectively never
        }
    }
}

/// Pick the index of the next member to try, round-robin from `start`, skipping any member
/// without a live registration. Returns `None` when the list is empty or no member is
/// currently registered (the caller then just keeps hearing hold music).
pub fn next_registered(
    members: &[String],
    is_registered: impl Fn(&str) -> bool,
    start: usize,
) -> Option<usize> {
    if members.is_empty() {
        return None;
    }
    (0..members.len())
        .map(|off| (start + off) % members.len())
        .find(|&i| is_registered(&members[i]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_max_wait_ms_treats_nonpositive_as_no_limit() {
        assert_eq!(WaitConfig::from_max_wait_ms(Some(120_000)).max_wait, Some(Duration::from_secs(120)));
        assert_eq!(WaitConfig::from_max_wait_ms(Some(0)).max_wait, None);
        assert_eq!(WaitConfig::from_max_wait_ms(Some(-5)).max_wait, None);
        assert_eq!(WaitConfig::from_max_wait_ms(None).max_wait, None);
    }

    #[test]
    fn overflow_due_only_after_max_wait() {
        let w = WaitConfig::from_max_wait_ms(Some(60_000));
        assert!(!w.overflow_due(Duration::from_secs(59)));
        assert!(w.overflow_due(Duration::from_secs(60)));
        assert!(w.overflow_due(Duration::from_secs(61)));
        // No limit → never overflows, even after a long wait.
        let none = WaitConfig::from_max_wait_ms(None);
        assert!(!none.overflow_due(Duration::from_secs(100_000)));
    }

    #[test]
    fn announce_due_follows_cadence() {
        let w = WaitConfig::from_max_wait_ms(None); // 30s announce cadence
        // Nothing played yet: first announcement due at 30s.
        assert!(!w.announce_due(Duration::from_secs(29), 0));
        assert!(w.announce_due(Duration::from_secs(30), 0));
        // One played: next due at 60s.
        assert!(!w.announce_due(Duration::from_secs(59), 1));
        assert!(w.announce_due(Duration::from_secs(60), 1));
    }

    #[test]
    fn next_registered_rotates_and_skips_offline() {
        let members = vec!["sip:100".to_string(), "sip:101".to_string(), "sip:102".to_string()];
        // Only 101 and 102 are registered.
        let reg = |m: &str| m == "sip:101" || m == "sip:102";
        // From 0: first registered at or after index 0 is 101 (index 1).
        assert_eq!(next_registered(&members, reg, 0), Some(1));
        // From 2: 102 (index 2) is registered.
        assert_eq!(next_registered(&members, reg, 2), Some(2));
        // From 3 (wraps to 0): 100 offline → 101.
        assert_eq!(next_registered(&members, reg, 3), Some(1));
    }

    #[test]
    fn next_registered_none_when_empty_or_all_offline() {
        assert_eq!(next_registered(&[], |_| true, 0), None);
        let members = vec!["sip:100".to_string(), "sip:101".to_string()];
        assert_eq!(next_registered(&members, |_| false, 0), None);
    }
}
