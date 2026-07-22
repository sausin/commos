//! G.711 helpers for the IVR/voicemail media runtime (Volume 7).
//!
//! Prompt playback streams 8 kHz mono G.711 — the codec family CommOS synthesises for the paths
//! where it is itself an endpoint (echo/IVR/voicemail) — so the media path stays pure-Rust with
//! no codec libraries. Both **μ-law** (PCMU) and **A-law** (PCMA) are supported so those paths
//! honour codec negotiation ([`crate::sip::codec`]); tones are generated in whichever the caller
//! negotiated. Decoding is the consumer's job (the phone), exactly as with recordings.
//!
//! G.711 samples are 8 kHz, so one byte == 125 µs; a 20 ms RTP frame is 160 bytes
//! ([`crate::sip::rtp`] packetises the byte stream produced here).

use std::f64::consts::PI;

/// Sample rate of the negotiated codec (G.711/8000).
pub const SAMPLE_RATE: usize = 8000;

/// The G.711 variant negotiated for an endpoint path — the one CommOS encodes tones in and the
/// RTP payload type it uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum G711 {
    /// PCMU — μ-law, RTP payload type 0.
    Ulaw,
    /// PCMA — A-law, RTP payload type 8.
    Alaw,
}

impl G711 {
    /// Map an SDP codec name (`PCMU`/`PCMA`) to the variant; `None` for a non-G.711 name.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "PCMU" => Some(G711::Ulaw),
            "PCMA" => Some(G711::Alaw),
            _ => None,
        }
    }
    /// The static RTP payload type (PCMU = 0, PCMA = 8).
    pub fn payload_type(self) -> u8 {
        match self {
            G711::Ulaw => 0,
            G711::Alaw => 8,
        }
    }
    /// The SDP rtpmap encoding name (`PCMU`/`PCMA`).
    pub fn sdp_name(self) -> &'static str {
        match self {
            G711::Ulaw => "PCMU",
            G711::Alaw => "PCMA",
        }
    }
    /// Encode one linear PCM sample in this variant.
    pub fn encode(self, sample: i16) -> u8 {
        match self {
            G711::Ulaw => linear_to_ulaw(sample),
            G711::Alaw => linear_to_alaw(sample),
        }
    }
    /// The byte value for digital silence (the encoding of a zero sample).
    pub fn silence(self) -> u8 {
        match self {
            G711::Ulaw => 0xFF,
            G711::Alaw => 0xD5,
        }
    }
}

/// Encode one 16-bit linear PCM sample to a G.711 **μ-law** byte (ITU-T G.711).
pub fn linear_to_ulaw(sample: i16) -> u8 {
    const BIAS: i32 = 0x84; // 132
    const CLIP: i32 = 32635;
    let sign = if sample < 0 { 0x80 } else { 0 };
    let mut mag = (sample as i32).abs();
    if mag > CLIP {
        mag = CLIP;
    }
    mag += BIAS;
    // Exponent = index of the highest set bit at/above bit 7 (the μ-law segment).
    let mut exponent = 7i32;
    let mut mask = 0x4000i32;
    while (mag & mask) == 0 && exponent > 0 {
        exponent -= 1;
        mask >>= 1;
    }
    let mantissa = (mag >> (exponent + 3)) & 0x0f;
    // μ-law is stored one's-complemented.
    !(sign | (exponent << 4) | mantissa) as u8
}

/// Encode one 16-bit linear PCM sample to a G.711 **A-law** byte (ITU-T G.711). A-law flips the
/// even bits with `0x55` on output.
pub fn linear_to_alaw(sample: i16) -> u8 {
    const CLIP: i32 = 32635;
    let sign = if sample >= 0 { 0x80 } else { 0x00 };
    let mut mag = (sample as i32).abs();
    if mag > CLIP {
        mag = CLIP;
    }
    let compressed = if mag < 256 {
        (mag >> 4) as u8
    } else {
        // Segment = position of the highest set bit (bits 8..=11).
        let mut exponent = 7i32;
        let mut mask = 0x4000i32;
        while (mag & mask) == 0 && exponent > 1 {
            exponent -= 1;
            mask >>= 1;
        }
        let mantissa = (mag >> (exponent + 3)) & 0x0f;
        ((exponent << 4) | mantissa) as u8
    };
    (sign as u8 | compressed) ^ 0x55
}

/// Decode one G.711 **μ-law** byte back to a 16-bit linear PCM sample (ITU-T G.711). The inverse
/// of [`linear_to_ulaw`]; used to transcode μ-law prompt files (FreePBX `.ulaw`) to A-law when a
/// call negotiated PCMA.
pub fn ulaw_to_linear(u: u8) -> i16 {
    const BIAS: i32 = 0x84;
    let u = !u; // μ-law is stored one's-complemented
    let sign = (u & 0x80) != 0;
    let exponent = ((u >> 4) & 0x07) as i32;
    let mantissa = (u & 0x0f) as i32;
    let magnitude = (((mantissa << 3) + BIAS) << exponent) - BIAS;
    if sign { -(magnitude as i16) } else { magnitude as i16 }
}

/// Re-encode a buffer of G.711 **μ-law** bytes (e.g. a FreePBX `.ulaw` prompt, or a stored
/// voicemail — both μ-law) into `codec`. For a μ-law target this is a cheap copy; for A-law it
/// decodes each sample and re-encodes, so prompts and stored messages play correctly regardless
/// of the codec the live call negotiated.
pub fn transcode_ulaw(ulaw: &[u8], codec: G711) -> Vec<u8> {
    match codec {
        G711::Ulaw => ulaw.to_vec(),
        G711::Alaw => ulaw.iter().map(|&b| linear_to_alaw(ulaw_to_linear(b))).collect(),
    }
}

/// Synthesise a `freq_hz` sine tone of `ms` milliseconds in `codec`, at `amplitude` (0..=32767).
/// Used for a generated prompt/beep when an IVR has no recorded prompt Object, and for the
/// voicemail "leave a message" beep.
pub fn tone(freq_hz: f64, ms: usize, amplitude: i16, codec: G711) -> Vec<u8> {
    let n = SAMPLE_RATE * ms / 1000;
    (0..n)
        .map(|i| {
            let t = i as f64 / SAMPLE_RATE as f64;
            let s = (amplitude as f64 * (2.0 * PI * freq_hz * t).sin()) as i16;
            codec.encode(s)
        })
        .collect()
}

/// The classic "beep" cue (a 425 Hz tone, `ms` long) in `codec`.
pub fn beep(ms: usize, codec: G711) -> Vec<u8> {
    tone(425.0, ms, 8000, codec)
}

/// Synthesise a **dual-tone** of `ms` milliseconds in `codec`: the sum of two sines at `f1`/`f2`,
/// each at `amplitude` (kept at ≤ ~12000 so the summed peak stays well within range).
pub fn dual_tone(f1: f64, f2: f64, ms: usize, amplitude: i16, codec: G711) -> Vec<u8> {
    let n = SAMPLE_RATE * ms / 1000;
    (0..n)
        .map(|i| {
            let t = i as f64 / SAMPLE_RATE as f64;
            let s = amplitude as f64 * ((2.0 * PI * f1 * t).sin() + (2.0 * PI * f2 * t).sin());
            codec.encode(s as i16)
        })
        .collect()
}

/// One period of standard **ring-back** in `codec`: a 440 + 480 Hz dual tone with the North
/// American cadence (2 s on, 4 s off). Loop it while a callee's phone is ringing so the caller
/// hears audible ring-back instead of silence during transfer setup.
pub fn ringback(codec: G711) -> Vec<u8> {
    let mut buf = dual_tone(440.0, 480.0, 2000, 11000, codec);
    buf.resize(buf.len() + SAMPLE_RATE * 4000 / 1000, codec.silence());
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    /// μ-law byte value for digital silence (the encoding of a zero sample).
    const SILENCE: u8 = 0xFF;

    #[test]
    fn zero_sample_is_ulaw_silence() {
        // The μ-law encoding of a zero sample is 0xFF (digital silence).
        assert_eq!(linear_to_ulaw(0), SILENCE);
    }

    #[test]
    fn ulaw_decode_and_transcode() {
        // 0xFF is μ-law digital silence → a zero sample.
        assert_eq!(ulaw_to_linear(0xFF), 0);
        // Decode preserves sign polarity across the codeword's sign bit.
        assert!(ulaw_to_linear(0x00) < 0); // sign bit set after inversion → negative, large mag
        assert!(ulaw_to_linear(0x80) > 0); // cleared → positive, large mag
        // Decode→encode is idempotent on the positive half (no ±0 aliasing there).
        for b in 0x80u8..=0xFF {
            assert_eq!(linear_to_ulaw(ulaw_to_linear(b)), b, "ulaw byte {b:#04x} did not round-trip");
        }
        // transcode to μ-law is a copy; to A-law it changes bytes but preserves length.
        let ulaw = vec![0x00u8, 0x7f, 0xff, 0x80];
        assert_eq!(transcode_ulaw(&ulaw, G711::Ulaw), ulaw);
        assert_eq!(transcode_ulaw(&ulaw, G711::Alaw).len(), ulaw.len());
        // Silence (0xFF μ-law == zero sample) maps to A-law silence (0xD5).
        assert_eq!(transcode_ulaw(&[0xFF], G711::Alaw), vec![0xD5]);
    }

    #[test]
    fn g711_variants_map_names_pts_and_silence() {
        assert_eq!(G711::from_name("PCMU"), Some(G711::Ulaw));
        assert_eq!(G711::from_name("PCMA"), Some(G711::Alaw));
        assert_eq!(G711::from_name("G722"), None);
        assert_eq!(G711::Ulaw.payload_type(), 0);
        assert_eq!(G711::Alaw.payload_type(), 8);
        // The silence byte is the encoding of a zero sample for each variant.
        assert_eq!(G711::Ulaw.silence(), linear_to_ulaw(0));
        assert_eq!(G711::Alaw.silence(), linear_to_alaw(0));
        assert_eq!(G711::Alaw.silence(), 0xD5);
    }

    #[test]
    fn alaw_sign_bit_reflects_polarity() {
        // A-law encodes sign in bit 7 (after the 0x55 toggle, +x and -x differ in that bit).
        let pos = linear_to_alaw(4000);
        let neg = linear_to_alaw(-4000);
        assert_ne!(pos & 0x80, neg & 0x80);
        let _ = linear_to_alaw(i16::MAX);
        let _ = linear_to_alaw(i16::MIN);
    }

    #[test]
    fn sign_bit_reflects_polarity() {
        // Positive samples clear the sign bit (bit 7 of the *inverted* byte is 0);
        // negative samples set it. Compare a symmetric pair.
        let pos = linear_to_ulaw(4000);
        let neg = linear_to_ulaw(-4000);
        // The stored byte is inverted, so the sign bit differs between +x and -x.
        assert_ne!(pos & 0x80, neg & 0x80);
        // Magnitude bits (after un-inverting) match for ±x.
        assert_eq!((!pos) & 0x7f, (!neg) & 0x7f);
    }

    #[test]
    fn full_scale_clips_without_panicking() {
        // Extremes encode to valid bytes (no overflow in exponent/mantissa maths).
        let _ = linear_to_ulaw(i16::MAX);
        let _ = linear_to_ulaw(i16::MIN);
    }

    #[test]
    fn tone_has_expected_length_and_is_audible() {
        // 100 ms at 8 kHz == 800 bytes; a tone is not digital silence.
        assert_eq!(tone(440.0, 100, 8000, G711::Ulaw).len(), 800);
        assert!(tone(440.0, 100, 8000, G711::Ulaw).iter().any(|&b| b != SILENCE));
        // A-law tone is likewise audible (not A-law silence).
        assert!(tone(440.0, 100, 8000, G711::Alaw).iter().any(|&b| b != G711::Alaw.silence()));
    }

    #[test]
    fn beep_is_audible_and_frame_aligned() {
        let b = beep(200, G711::Ulaw);
        assert_eq!(b.len(), 1600); // 200 ms
        assert!(b.iter().any(|&x| x != SILENCE));
    }

    #[test]
    fn ringback_has_a_tone_then_silence_cadence() {
        for codec in [G711::Ulaw, G711::Alaw] {
            let rb = ringback(codec);
            // 2 s tone + 4 s silence == 6 s == 48000 bytes.
            assert_eq!(rb.len(), 48000);
            let sil = codec.silence();
            // The first 2 s carries the dual tone; the tail 4 s is silence.
            assert!(rb[..16000].iter().any(|&b| b != sil), "leading segment should be audible");
            assert!(rb[16000..].iter().all(|&b| b == sil), "trailing segment should be silent");
        }
    }
}
