//! SDES key exchange for SRTP (Volume 7; RFC 4568) — the `a=crypto` SDP attribute.
//!
//! SDES carries the SRTP master key + salt inline in the SDP offer/answer. Each side advertises
//! its own key: the offerer's `a=crypto` keys the media it *sends*, the answerer's keys the media
//! it sends back. Because the key travels in the SDP, SDES is only as private as the signalling
//! channel — it belongs behind SIP-over-TLS (the signalling-encryption work that pairs with this).
//! On a trusted LAN it still defeats a passive RTP sniffer, which is the point.
//!
//! CommOS implements the single mandatory-to-implement suite, `AES_CM_128_HMAC_SHA1_80`
//! ([`crate::sip::srtp`]). The optional lifetime/MKI parameters are tolerated on input and omitted
//! on output (key-derivation-rate 0, no MKI — the SDES common case).

use base64::Engine;

use super::srtp::KEY_SALT_LEN;

/// The one SRTP crypto suite CommOS negotiates (RFC 4568 §6.2), and the profile token that marks
/// secure RTP in an `m=audio` line.
pub const CRYPTO_SUITE: &str = "AES_CM_128_HMAC_SHA1_80";

/// A parsed (or generated) `a=crypto` attribute: its SDP tag and the 30-byte SRTP key+salt.
pub struct CryptoAttr {
    /// The `a=crypto:<tag>` ordinal, echoed back so the peer can correlate the answer.
    pub tag: u32,
    /// Master key (16) followed by master salt (14) — the `inline:` material for this suite.
    pub key_salt: [u8; KEY_SALT_LEN],
}

impl CryptoAttr {
    /// The first usable `a=crypto` attribute in `sdp` for our suite, or `None` if the offer has no
    /// `AES_CM_128_HMAC_SHA1_80` crypto line with a well-formed 30-byte inline key.
    pub fn from_sdp(sdp: &str) -> Option<CryptoAttr> {
        sdp.lines().find_map(|line| Self::parse_line(line.trim()))
    }

    /// Parse one `a=crypto:<tag> AES_CM_128_HMAC_SHA1_80 inline:<b64>[|params] ...` line.
    fn parse_line(line: &str) -> Option<CryptoAttr> {
        let rest = line.strip_prefix("a=crypto:")?;
        let mut tokens = rest.split_whitespace();
        let tag: u32 = tokens.next()?.parse().ok()?;
        if tokens.next()? != CRYPTO_SUITE {
            return None; // A suite we don't implement — skip it, a later line may match.
        }
        // key-param: `inline:<base64 key||salt>[|lifetime][|MKI:len]`. Take the base64 up to the
        // first `|` (lifetime/MKI are optional and CommOS doesn't use them).
        let key_param = tokens.next()?.strip_prefix("inline:")?;
        let b64 = key_param.split('|').next()?;
        let raw = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
        let key_salt: [u8; KEY_SALT_LEN] = raw.try_into().ok()?;
        Some(CryptoAttr { tag, key_salt })
    }

    /// Render this attribute as an SDP `a=crypto` line (no trailing CRLF).
    pub fn to_line(&self) -> String {
        let b64 = base64::engine::general_purpose::STANDARD.encode(self.key_salt);
        format!("a=crypto:{} {CRYPTO_SUITE} inline:{b64}", self.tag)
    }
}

/// Whether an SDP `m=audio` line advertises the **secure** RTP profile (`RTP/SAVP` or the
/// feedback variant `RTP/SAVPF`) — the signal that the peer wants SRTP.
pub fn offers_savp(sdp: &str) -> bool {
    sdp.lines().any(|line| {
        let line = line.trim();
        line.strip_prefix("m=audio")
            .and_then(|rest| rest.split_whitespace().nth(1)) // the transport/proto token
            .is_some_and(|proto| proto == "RTP/SAVP" || proto == "RTP/SAVPF")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFER: &str = "v=0\r\no=- 0 0 IN IP4 1.2.3.4\r\ns=-\r\nc=IN IP4 1.2.3.4\r\nt=0 0\r\n\
        m=audio 5004 RTP/SAVP 0 101\r\na=rtpmap:0 PCMU/8000\r\n\
        a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:WVNfX2xpdmUta2V5LW1hdGVyaWFsLXp6enp6enp6|2^20|1:32\r\n\
        a=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n";

    #[test]
    fn detects_secure_profile() {
        assert!(offers_savp(OFFER));
        assert!(offers_savp("m=audio 5 RTP/SAVPF 0\r\n"));
        // Plain RTP/AVP is not secure.
        assert!(!offers_savp("m=audio 5 RTP/AVP 0\r\n"));
    }

    #[test]
    fn parses_crypto_line_with_optional_params() {
        let attr = CryptoAttr::from_sdp(OFFER).expect("crypto attr");
        assert_eq!(attr.tag, 1);
        // The inline material decodes to exactly a 30-byte key+salt.
        assert_eq!(attr.key_salt.len(), KEY_SALT_LEN);
    }

    #[test]
    fn round_trips_through_sdp_line() {
        let attr = CryptoAttr { tag: 7, key_salt: [0x5a; KEY_SALT_LEN] };
        let line = attr.to_line();
        assert!(line.starts_with("a=crypto:7 AES_CM_128_HMAC_SHA1_80 inline:"));
        let back = CryptoAttr::parse_line(&line).expect("reparse");
        assert_eq!(back.tag, 7);
        assert_eq!(back.key_salt, [0x5a; KEY_SALT_LEN]);
    }

    #[test]
    fn rejects_unknown_suite_and_bad_key_length() {
        // A suite we don't implement is skipped.
        let s = "a=crypto:1 AES_256_CM_HMAC_SHA1_80 inline:YWJj\r\n";
        assert!(CryptoAttr::from_sdp(s).is_none());
        // Right suite but the inline key is the wrong length.
        let s = "a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:YWJj\r\n";
        assert!(CryptoAttr::from_sdp(s).is_none());
    }

    #[test]
    fn picks_our_suite_among_several_offered() {
        // Offerers list multiple crypto lines by preference; we take the first we support.
        let s = "a=crypto:1 AES_256_CM_HMAC_SHA1_80 inline:YWJj\r\n\
                 a=crypto:2 AES_CM_128_HMAC_SHA1_80 inline:WVNfX2xpdmUta2V5LW1hdGVyaWFsLXp6enp6enp6\r\n";
        let attr = CryptoAttr::from_sdp(s).expect("second line matches");
        assert_eq!(attr.tag, 2);
    }
}
