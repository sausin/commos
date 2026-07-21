//! G.711 μ-law helpers for the IVR/voicemail media runtime (Volume 7).
//!
//! Prompt playback streams 8 kHz mono μ-law (`audio/basic`) — the one codec this reference
//! negotiates (PCMU) — so the media path stays pure-Rust with no codec libraries. This module
//! provides the μ-law *encoder* (to synthesise prompt/beep tones when no recorded prompt Object
//! is available) and small tone/silence generators. Decoding is the consumer's job (the phone),
//! exactly as with recordings.
//!
//! μ-law samples are 8 kHz, so one byte == 125 µs; a 20 ms RTP frame is 160 bytes
//! ([`crate::sip::rtp`] packetises the byte stream produced here).

use std::f64::consts::PI;

/// Sample rate of the negotiated codec (PCMU/8000).
pub const SAMPLE_RATE: usize = 8000;
/// μ-law byte value for digital silence (the encoding of a zero sample).
pub const ULAW_SILENCE: u8 = 0xFF;

/// Encode one 16-bit linear PCM sample to a G.711 μ-law byte (ITU-T G.711).
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

/// Synthesise a `freq_hz` sine tone of `ms` milliseconds as μ-law, at `amplitude` (0..=32767).
/// Used for a generated prompt/beep when an IVR has no recorded prompt Object, and for the
/// voicemail "leave a message" beep.
pub fn tone(freq_hz: f64, ms: usize, amplitude: i16) -> Vec<u8> {
    let n = SAMPLE_RATE * ms / 1000;
    (0..n)
        .map(|i| {
            let t = i as f64 / SAMPLE_RATE as f64;
            let s = (amplitude as f64 * (2.0 * PI * freq_hz * t).sin()) as i16;
            linear_to_ulaw(s)
        })
        .collect()
}

/// The classic dual-tone "beep" used to cue the caller to begin (a 425 Hz tone, `ms` long).
pub fn beep(ms: usize) -> Vec<u8> {
    tone(425.0, ms, 8000)
}

/// Synthesise a **dual-tone** of `ms` milliseconds as μ-law: the sum of two sines at `f1`/`f2`,
/// each at `amplitude` (kept at ≤ ~12000 so the summed peak stays well within range).
pub fn dual_tone(f1: f64, f2: f64, ms: usize, amplitude: i16) -> Vec<u8> {
    let n = SAMPLE_RATE * ms / 1000;
    (0..n)
        .map(|i| {
            let t = i as f64 / SAMPLE_RATE as f64;
            let s = amplitude as f64 * ((2.0 * PI * f1 * t).sin() + (2.0 * PI * f2 * t).sin());
            linear_to_ulaw(s as i16)
        })
        .collect()
}

/// One period of standard **ring-back** as μ-law: a 440 + 480 Hz dual tone with the North
/// American cadence (2 s on, 4 s off). Loop it while a callee's phone is ringing so the caller
/// hears audible ring-back instead of silence during transfer setup.
pub fn ringback() -> Vec<u8> {
    let mut buf = dual_tone(440.0, 480.0, 2000, 11000);
    buf.resize(buf.len() + SAMPLE_RATE * 4000 / 1000, ULAW_SILENCE);
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
        assert_eq!(tone(440.0, 100, 8000).len(), 800);
        assert!(tone(440.0, 100, 8000).iter().any(|&b| b != SILENCE));
    }

    #[test]
    fn beep_is_audible_and_frame_aligned() {
        let b = beep(200);
        assert_eq!(b.len(), 1600); // 200 ms
        assert!(b.iter().any(|&x| x != SILENCE));
    }

    #[test]
    fn ringback_has_a_tone_then_silence_cadence() {
        let rb = ringback();
        // 2 s tone + 4 s silence == 6 s == 48000 bytes.
        assert_eq!(rb.len(), 48000);
        assert_eq!(ULAW_SILENCE, SILENCE);
        // The first 2 s carries the dual tone; the tail 4 s is silence.
        assert!(rb[..16000].iter().any(|&b| b != SILENCE), "leading segment should be audible");
        assert!(rb[16000..].iter().all(|&b| b == SILENCE), "trailing segment should be silent");
    }
}
