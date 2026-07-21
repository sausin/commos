//! SDP codec negotiation (Volume 7; CMOS-07-SIP-041).
//!
//! Parses the audio media of an SDP offer into a preference-ordered codec list, and selects a
//! codec for two cases:
//! - **Endpoint** paths (echo / IVR / voicemail), where CommOS itself synthesises and consumes
//!   audio: it must pick a codec it can encode/decode — G.711 μ-law (PCMU) or A-law (PCMA).
//! - **Pass-through** paths (a bridge to a registered callee, or a carrier trunk), where CommOS
//!   only relays bytes: any codec works, so it offers the caller's whole list to the far end and
//!   answers the caller with whatever the far end selects — transparent to G.722, Opus, etc., with
//!   no transcoding (the pure-Rust, no-codec-libs posture).
//!
//! The negotiated `telephone-event` payload type (for DTMF, RFC 4733) is picked up here too.

use std::collections::HashMap;

/// Static RTP payload-type → (name, clock) for well-known audio codecs (RFC 3551), used when an
/// offer omits the `a=rtpmap` for a static PT (phones routinely do for PCMU/PCMA).
fn static_codec(pt: u8) -> Option<(&'static str, u32)> {
    match pt {
        0 => Some(("PCMU", 8000)),
        3 => Some(("GSM", 8000)),
        8 => Some(("PCMA", 8000)),
        9 => Some(("G722", 8000)),
        18 => Some(("G729", 8000)),
        _ => None,
    }
}

/// A negotiated codec: its RTP payload type and `<name>/<clock>` rtpmap value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Codec {
    pub pt: u8,
    pub name: String,
    pub clock: u32,
}

impl Codec {
    /// The `a=rtpmap` value for this codec (`PCMU/8000`).
    pub fn rtpmap(&self) -> String {
        format!("{}/{}", self.name, self.clock)
    }
}

/// The parsed audio media of an SDP body: payload types in preference order plus their rtpmap.
pub struct AudioMedia {
    /// Audio payload types in the offer's preference order (from the `m=audio` line).
    pub pts: Vec<u8>,
    /// pt → (uppercased NAME, clock), from `a=rtpmap` plus static defaults.
    map: HashMap<u8, (String, u32)>,
}

impl AudioMedia {
    /// Parse the audio codecs from an SDP body. Tolerant of missing rtpmap lines (falls back to
    /// the static payload-type table) and of CRLF/LF endings.
    pub fn parse(sdp: &str) -> AudioMedia {
        let mut pts = Vec::new();
        let mut map = HashMap::new();
        for line in sdp.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("m=audio") {
                // `m=audio <port> RTP/AVP <pt> <pt> ...`
                let mut it = rest.split_whitespace();
                let _port = it.next();
                let _proto = it.next();
                for tok in it {
                    if let Ok(pt) = tok.parse::<u8>() {
                        pts.push(pt);
                    }
                }
            } else if let Some(rest) = line.strip_prefix("a=rtpmap:") {
                // `<pt> <name>/<clock>[/<channels>]`
                if let Some((pt_s, val)) = rest.split_once(char::is_whitespace) {
                    if let Ok(pt) = pt_s.trim().parse::<u8>() {
                        let mut parts = val.trim().split('/');
                        let name = parts.next().unwrap_or("").trim().to_ascii_uppercase();
                        let clock = parts.next().and_then(|c| c.trim().parse().ok()).unwrap_or(8000);
                        if !name.is_empty() {
                            map.insert(pt, (name, clock));
                        }
                    }
                }
            }
        }
        AudioMedia { pts, map }
    }

    fn name_clock(&self, pt: u8) -> Option<(String, u32)> {
        self.map
            .get(&pt)
            .cloned()
            .or_else(|| static_codec(pt).map(|(n, c)| (n.to_string(), c)))
    }

    /// The negotiated `telephone-event` payload type (for RFC 4733 DTMF), if the offer has one.
    pub fn telephone_event_pt(&self) -> Option<u8> {
        self.pts.iter().copied().find(|&pt| {
            self.name_clock(pt).map(|(n, _)| n == "TELEPHONE-EVENT").unwrap_or(false)
        })
    }

    /// The caller's preferred **audio** codec (the first non-telephone-event PT), for transparent
    /// pass-through relay of any codec.
    pub fn preferred_audio(&self) -> Option<Codec> {
        self.pts.iter().copied().find_map(|pt| {
            let (name, clock) = self.name_clock(pt)?;
            (name != "TELEPHONE-EVENT").then_some(Codec { pt, name, clock })
        })
    }

    /// The first offered **G.711** codec (PCMU/PCMA) — the endpoint codec CommOS can synthesise
    /// and decode. Preference follows the offer's order; `None` if neither is offered.
    pub fn select_g711(&self) -> Option<Codec> {
        self.pts.iter().copied().find_map(|pt| {
            let (name, clock) = self.name_clock(pt)?;
            matches!(name.as_str(), "PCMU" | "PCMA").then_some(Codec { pt, name, clock })
        })
    }

    /// Render the offer's audio codecs (audio PTs + their rtpmap lines) into SDP `m=`/`a=rtpmap`
    /// fragments to *re-offer* the same list to the far end of a bridge — so caller and callee
    /// converge on a shared codec CommOS can relay untouched. Returns `(pt_list, rtpmap_lines)`.
    pub fn reoffer_lines(&self) -> (String, String) {
        let pt_list = self.pts.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(" ");
        let mut rtpmaps = String::new();
        for &pt in &self.pts {
            if let Some((name, clock)) = self.name_clock(pt) {
                rtpmaps.push_str(&format!("a=rtpmap:{pt} {name}/{clock}\r\n"));
            }
        }
        (pt_list, rtpmaps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFER: &str = "v=0\r\no=- 0 0 IN IP4 1.2.3.4\r\ns=-\r\nc=IN IP4 1.2.3.4\r\nt=0 0\r\n\
        m=audio 5004 RTP/AVP 9 8 0 101\r\n\
        a=rtpmap:9 G722/8000\r\na=rtpmap:8 PCMA/8000\r\na=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:101 telephone-event/8000\r\na=fmtp:101 0-16\r\na=sendrecv\r\n";

    #[test]
    fn parses_pts_in_preference_order() {
        let m = AudioMedia::parse(OFFER);
        assert_eq!(m.pts, vec![9, 8, 0, 101]);
    }

    #[test]
    fn negotiates_telephone_event_pt() {
        assert_eq!(AudioMedia::parse(OFFER).telephone_event_pt(), Some(101));
        // A non-standard dynamic PT is honoured.
        let m = AudioMedia::parse("m=audio 5 RTP/AVP 0 96\r\na=rtpmap:96 telephone-event/8000\r\n");
        assert_eq!(m.telephone_event_pt(), Some(96));
        // No telephone-event offered.
        assert_eq!(AudioMedia::parse("m=audio 5 RTP/AVP 0\r\n").telephone_event_pt(), None);
    }

    #[test]
    fn preferred_audio_is_the_callers_top_choice() {
        // G722 is first in the m= line, so it wins for pass-through.
        assert_eq!(AudioMedia::parse(OFFER).preferred_audio().unwrap().name, "G722");
    }

    #[test]
    fn select_g711_prefers_offer_order_among_g711_variants() {
        // PCMA (8) precedes PCMU (0) in the m= line → PCMA is the endpoint codec.
        let c = AudioMedia::parse(OFFER).select_g711().unwrap();
        assert_eq!(c.name, "PCMA");
        assert_eq!(c.pt, 8);
        // An offer with only PCMU → PCMU.
        let m = AudioMedia::parse("m=audio 5 RTP/AVP 0 101\r\n");
        assert_eq!(m.select_g711().unwrap().name, "PCMU");
        // A G.711-less offer (e.g. G722 only) → None (endpoint paths can't serve it).
        let m = AudioMedia::parse("m=audio 5 RTP/AVP 9\r\na=rtpmap:9 G722/8000\r\n");
        assert!(m.select_g711().is_none());
    }

    #[test]
    fn static_pts_resolve_without_rtpmap() {
        // No rtpmap lines at all — static PTs 0/8 still resolve.
        let m = AudioMedia::parse("m=audio 5 RTP/AVP 8 0\r\n");
        assert_eq!(m.select_g711().unwrap().name, "PCMA");
        assert_eq!(m.preferred_audio().unwrap().pt, 8);
    }

    #[test]
    fn reoffer_preserves_the_callers_codec_list() {
        let (pts, rtpmaps) = AudioMedia::parse(OFFER).reoffer_lines();
        assert_eq!(pts, "9 8 0 101");
        assert!(rtpmaps.contains("a=rtpmap:9 G722/8000\r\n"));
        assert!(rtpmaps.contains("a=rtpmap:101 TELEPHONE-EVENT/8000\r\n"));
    }
}
