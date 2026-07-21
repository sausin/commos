//! DTMF (touch-tone) decoding for the IVR runtime (Volume 7; CMOS-07-SIP-042).
//!
//! Two transports are normalised into a single digit representation:
//! - **RFC 4733 / RFC 2833 telephone-event** carried in RTP (the in-band path negotiated in
//!   SDP as `telephone-event/8000` at a dynamic payload type). This is the primary transport
//!   and is decoded from RTP packets here.
//! - **SIP INFO** with an `application/dtmf-relay` (or `application/dtmf`) body — the
//!   signalling-path fallback some phones use.
//!
//! A single key-press is transmitted as a *burst* of RTP events (one every packet interval,
//! the last three with the End bit). [`DtmfCollector`] debounces that burst into exactly one
//! digit, so callers see one digit per press.

/// The dynamic RTP payload type this reference advertises for `telephone-event/8000`.
pub const TELEPHONE_EVENT_PT: u8 = 101;

/// Fixed RTP header length (no CSRC/extension — the common case for desk phones).
const RTP_HEADER_LEN: usize = 12;

/// One decoded telephone-event RTP payload (RFC 4733 §2.3): the event code, whether this
/// packet marks the End of the event, and the event duration in timestamp units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TelephoneEvent {
    pub event: u8,
    pub end: bool,
    pub duration: u16,
}

/// Map an RFC 4733 event code to its DTMF character (0-9, `*`, `#`, A-D). Returns `None` for
/// non-DTMF events (e.g. 16 = hook-flash).
pub fn event_to_digit(event: u8) -> Option<char> {
    match event {
        0..=9 => Some((b'0' + event) as char),
        10 => Some('*'),
        11 => Some('#'),
        12..=15 => Some((b'A' + (event - 12)) as char),
        _ => None,
    }
}

/// The RTP payload type of a packet: header byte 1, low 7 bits (RFC 3550). `None` if the
/// datagram is too short to be an RTP packet.
pub fn rtp_payload_type(packet: &[u8]) -> Option<u8> {
    (packet.len() >= RTP_HEADER_LEN).then(|| packet[1] & 0x7f)
}

/// Decode a telephone-event RTP packet whose payload type is `pt` (the negotiated
/// `telephone-event` PT). Returns `None` when the packet is not that payload type or is too
/// short to carry the 4-byte event body.
pub fn decode_telephone_event(packet: &[u8], pt: u8) -> Option<TelephoneEvent> {
    if rtp_payload_type(packet)? != pt {
        return None;
    }
    let payload = packet.get(RTP_HEADER_LEN..)?;
    if payload.len() < 4 {
        return None;
    }
    Some(TelephoneEvent {
        event: payload[0],
        end: payload[1] & 0x80 != 0,
        duration: u16::from_be_bytes([payload[2], payload[3]]),
    })
}

/// Debounces a burst of telephone-event packets into one digit per key-press.
///
/// A press produces many RTP packets carrying the same `event`; this emits the digit once, at
/// the *start* of a new event, and re-arms on the End packet so the same key pressed again is
/// captured. Feed every telephone-event you decode; act on each `Some(char)`.
#[derive(Default)]
pub struct DtmfCollector {
    /// The event currently in progress (already emitted), or `None` between events.
    active: Option<u8>,
}

impl DtmfCollector {
    pub fn new() -> Self {
        DtmfCollector::default()
    }

    /// Feed one decoded telephone-event; returns the digit exactly once per key-press (on the
    /// first packet of a new event), else `None`.
    pub fn push(&mut self, ev: TelephoneEvent) -> Option<char> {
        let digit = if self.active != Some(ev.event) {
            // A new event begins → emit its digit once.
            self.active = Some(ev.event);
            event_to_digit(ev.event)
        } else {
            None
        };
        if ev.end {
            // Event finished; re-arm so a repeat of the same digit is captured next time.
            self.active = None;
        }
        digit
    }
}

/// Parse a DTMF digit from a SIP `INFO` body. Handles the two common shapes:
/// `application/dtmf-relay` (`Signal=5\r\nDuration=250`) and `application/dtmf` (a bare digit).
/// Returns the digit character, or `None` if none is present.
pub fn parse_info_dtmf(body: &str) -> Option<char> {
    let body = body.trim();
    // application/dtmf-relay: a `Signal=<digit>` line (case-insensitive key).
    for line in body.lines() {
        let line = line.trim();
        if let Some((key, val)) = line.split_once('=') {
            if key.trim().eq_ignore_ascii_case("signal") {
                return normalise_digit(val.trim());
            }
        }
    }
    // application/dtmf: the whole body is a single digit/token.
    normalise_digit(body)
}

/// Normalise a textual DTMF token (`"5"`, `"*"`, `"#"`, `"11"` for `#`, `"10"` for `*`,
/// `"A".."D"`) to its digit character.
fn normalise_digit(tok: &str) -> Option<char> {
    let tok = tok.trim();
    match tok {
        "10" => return Some('*'),
        "11" => return Some('#'),
        _ => {}
    }
    if let Ok(n) = tok.parse::<u8>() {
        return event_to_digit(n);
    }
    let mut chars = tok.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if matches!(c, '0'..='9' | '*' | '#' | 'A'..='D' | 'a'..='d') => {
            Some(c.to_ascii_uppercase())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a telephone-event RTP packet: 12-byte header (with `pt`) + 4-byte event body.
    fn te_packet(pt: u8, event: u8, end: bool, duration: u16) -> Vec<u8> {
        let mut p = vec![0u8; RTP_HEADER_LEN];
        p[0] = 0x80; // version 2
        p[1] = pt & 0x7f; // marker clear, payload type
        p.push(event);
        p.push(if end { 0x80 } else { 0x00 }); // End bit + volume
        p.extend_from_slice(&duration.to_be_bytes());
        p
    }

    #[test]
    fn event_codes_map_to_digits() {
        assert_eq!(event_to_digit(0), Some('0'));
        assert_eq!(event_to_digit(9), Some('9'));
        assert_eq!(event_to_digit(10), Some('*'));
        assert_eq!(event_to_digit(11), Some('#'));
        assert_eq!(event_to_digit(12), Some('A'));
        assert_eq!(event_to_digit(15), Some('D'));
        assert_eq!(event_to_digit(16), None); // hook-flash, not a digit
    }

    #[test]
    fn decodes_only_the_negotiated_payload_type() {
        let pkt = te_packet(TELEPHONE_EVENT_PT, 5, false, 160);
        let ev = decode_telephone_event(&pkt, TELEPHONE_EVENT_PT).unwrap();
        assert_eq!(ev.event, 5);
        assert!(!ev.end);
        assert_eq!(ev.duration, 160);
        // A PCMU (PT 0) audio packet is not a telephone-event.
        let audio = vec![0x80u8, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3];
        assert_eq!(decode_telephone_event(&audio, TELEPHONE_EVENT_PT), None);
    }

    #[test]
    fn collector_emits_one_digit_per_burst() {
        let mut c = DtmfCollector::new();
        // A "7" key-press: several packets, the last with End.
        let digits: Vec<char> = [false, false, false, true]
            .into_iter()
            .filter_map(|end| c.push(TelephoneEvent { event: 7, end, duration: 160 }))
            .collect();
        assert_eq!(digits, vec!['7'], "exactly one digit per key-press burst");

        // The same digit pressed again after End is captured again.
        assert_eq!(c.push(TelephoneEvent { event: 7, end: false, duration: 160 }), Some('7'));
    }

    #[test]
    fn collector_handles_distinct_consecutive_digits() {
        let mut c = DtmfCollector::new();
        assert_eq!(c.push(TelephoneEvent { event: 1, end: false, duration: 0 }), Some('1'));
        assert_eq!(c.push(TelephoneEvent { event: 1, end: true, duration: 320 }), None);
        assert_eq!(c.push(TelephoneEvent { event: 2, end: false, duration: 0 }), Some('2'));
        assert_eq!(c.push(TelephoneEvent { event: 2, end: true, duration: 320 }), None);
    }

    #[test]
    fn parses_sip_info_dtmf_bodies() {
        // application/dtmf-relay
        assert_eq!(parse_info_dtmf("Signal=5\r\nDuration=250"), Some('5'));
        assert_eq!(parse_info_dtmf("signal=#\r\nDuration=100"), Some('#'));
        assert_eq!(parse_info_dtmf("Signal=10"), Some('*')); // numeric * encoding
        assert_eq!(parse_info_dtmf("Signal=11"), Some('#')); // numeric # encoding
        // application/dtmf (bare digit)
        assert_eq!(parse_info_dtmf("7"), Some('7'));
        assert_eq!(parse_info_dtmf("*"), Some('*'));
        assert_eq!(parse_info_dtmf("D"), Some('D'));
        // Nothing usable.
        assert_eq!(parse_info_dtmf("Duration=250"), None);
        assert_eq!(parse_info_dtmf(""), None);
    }
}
