//! SRTP media encryption (Volume 7; RFC 3711) — pure-Rust, no libsrtp / OpenSSL.
//!
//! CommOS encrypts the RTP media path so call audio can't be snooped off the wire, which is
//! worth doing even on an internal LAN. The crypto suite is the SRTP default and the one every
//! desk phone implements: **`AES_CM_128_HMAC_SHA1_80`** — AES-128 in counter mode for
//! confidentiality, HMAC-SHA1 truncated to 80 bits for message authentication. Keys are carried
//! in the SDP via SDES ([`crate::sip::sdes`], RFC 4568); this module is the wire crypto.
//!
//! Everything here is RustCrypto ([`aes`]/[`ctr`]/[`hmac`]/[`sha1`]) so encrypting the media
//! plane costs the arm64 cross-build nothing. The maths is validated against the RFC 3711
//! Appendix B.3 key-derivation test vectors (see the tests).
//!
//! **Scope.** One crypto suite (the ubiquitous 80-bit-tag AES-CM), SRTP for RTP (SRTCP is not
//! encrypted — CommOS sends no compound RTCP on these paths), and key-derivation-rate 0 (one
//! session key per master key, as SDES always uses). The 48-bit packet index is tracked per
//! stream with the RFC 3711 Appendix roll-over-counter estimation.

use aes::cipher::{KeyIvInit, StreamCipher};
use hmac::{Mac, SimpleHmac};

/// AES-CM in counter mode with a 128-bit big-endian counter block (RFC 3711 §4.1.1).
type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;
/// HMAC-SHA1 (RFC 3711 §4.2). `SimpleHmac` avoids the `Mac`-key-length precomputation of `Hmac`
/// and matches the plain HMAC construction phones use.
type HmacSha1 = SimpleHmac<sha1::Sha1>;

/// SDES master key length for `AES_CM_128_HMAC_SHA1_80` (128-bit AES key).
pub const MASTER_KEY_LEN: usize = 16;
/// SDES master salt length (112-bit salt).
pub const MASTER_SALT_LEN: usize = 14;
/// The `inline:` key material for this suite is the master key followed by the master salt.
pub const KEY_SALT_LEN: usize = MASTER_KEY_LEN + MASTER_SALT_LEN; // 30
/// Authentication tag length carried on the wire (HMAC-SHA1 truncated to 80 bits).
pub const AUTH_TAG_LEN: usize = 10;
/// Derived session authentication key length (HMAC-SHA1 key, 160 bits).
const AUTH_KEY_LEN: usize = 20;

/// KDF labels (RFC 3711 §4.3.1): the session encryption key, authentication key, and salt.
const LABEL_ENC: u8 = 0x00;
const LABEL_AUTH: u8 = 0x01;
const LABEL_SALT: u8 = 0x02;

/// The AES-CM key-derivation PRF (RFC 3711 §4.3.3): fill `out` with the keystream
/// `AES-CM(master_key, iv)`. `out` is consumed from a zeroed buffer so `apply_keystream` yields
/// the raw keystream.
fn prf_fill(master_key: &[u8; MASTER_KEY_LEN], iv: [u8; 16], out: &mut [u8]) {
    out.iter_mut().for_each(|b| *b = 0);
    let mut cipher = Aes128Ctr::new(master_key.into(), &iv.into());
    cipher.apply_keystream(out);
}

/// Build the KDF input block for `label` (RFC 3711 §4.3.1) with key-derivation-rate 0 and index 0:
/// `x = master_salt` with the label XORed into the octet 7 bytes from the left, then padded with
/// two zero octets to a 128-bit counter block.
fn kdf_iv(master_salt: &[u8; MASTER_SALT_LEN], label: u8) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[..MASTER_SALT_LEN].copy_from_slice(master_salt);
    iv[7] ^= label; // right-aligned key_id (label || 0..0) XORed into the salt
    iv
}

/// One SRTP stream's cryptographic context: the derived session keys plus the roll-over counter
/// state used to reconstruct the 48-bit packet index from the 16-bit RTP sequence number.
///
/// A context is directional — one for packets CommOS **sends** on a leg (keyed with the key
/// CommOS advertised in its `a=crypto`) and one for packets it **receives** (keyed with the far
/// end's advertised key). [`protect`](Self::protect) and [`unprotect`](Self::unprotect) each
/// advance this stream's index state.
pub struct SrtpContext {
    session_key: [u8; MASTER_KEY_LEN],
    session_salt: [u8; MASTER_SALT_LEN],
    auth_key: [u8; AUTH_KEY_LEN],
    /// Roll-over counter: the high 32 bits of the 48-bit packet index.
    roc: u32,
    /// Highest sequence number seen so far (for the RFC 3711 Appendix index estimation).
    s_l: u16,
    /// Whether any packet has been processed yet (seeds `s_l` on the first one).
    started: bool,
    /// Anti-replay state (RFC 3711 §3.3.2), receiver side. `replay_top` is the highest 48-bit
    /// index accepted; `replay_mask` bit *i* marks index `replay_top - i` as already seen, over a
    /// 64-packet window. A replayed or too-old index is dropped.
    replay_top: u64,
    replay_mask: u64,
    replay_started: bool,
}

impl SrtpContext {
    /// Derive the session keys for a stream from its SDES master key + salt (RFC 3711 §4.3).
    pub fn new(master_key: &[u8; MASTER_KEY_LEN], master_salt: &[u8; MASTER_SALT_LEN]) -> Self {
        let mut session_key = [0u8; MASTER_KEY_LEN];
        prf_fill(master_key, kdf_iv(master_salt, LABEL_ENC), &mut session_key);
        let mut session_salt = [0u8; MASTER_SALT_LEN];
        prf_fill(master_key, kdf_iv(master_salt, LABEL_SALT), &mut session_salt);
        let mut auth_key = [0u8; AUTH_KEY_LEN];
        prf_fill(master_key, kdf_iv(master_salt, LABEL_AUTH), &mut auth_key);
        SrtpContext {
            session_key,
            session_salt,
            auth_key,
            roc: 0,
            s_l: 0,
            started: false,
            replay_top: 0,
            replay_mask: 0,
            replay_started: false,
        }
    }

    /// Build the AES-CM keystream IV for one packet (RFC 3711 §4.1.1):
    /// `iv = (salt << 16) XOR (ssrc << 64) XOR (index << 16)`, as a 128-bit counter block.
    fn keystream_iv(&self, ssrc: [u8; 4], index: u64) -> [u8; 16] {
        let mut iv = [0u8; 16];
        iv[..MASTER_SALT_LEN].copy_from_slice(&self.session_salt);
        for i in 0..4 {
            iv[4 + i] ^= ssrc[i];
        }
        // The 48-bit index (ROC || SEQ) lands in octets 8..14.
        let idx = index.to_be_bytes(); // 8 bytes; the low 6 are the 48-bit index
        for i in 0..6 {
            iv[8 + i] ^= idx[2 + i];
        }
        iv
    }

    /// The authentication tag over `portion` (header || encrypted payload) with the roll-over
    /// counter appended, truncated to [`AUTH_TAG_LEN`] (RFC 3711 §4.2).
    fn auth_tag(&self, portion: &[u8], roc: u32) -> [u8; AUTH_TAG_LEN] {
        let mut mac = <HmacSha1 as Mac>::new_from_slice(&self.auth_key).expect("hmac key");
        mac.update(portion);
        mac.update(&roc.to_be_bytes());
        let full = mac.finalize().into_bytes();
        let mut tag = [0u8; AUTH_TAG_LEN];
        tag.copy_from_slice(&full[..AUTH_TAG_LEN]);
        tag
    }

    /// The 48-bit packet index for a received/sent `seq`, and the resulting roll-over counter,
    /// using the RFC 3711 Appendix estimation. Does not mutate state (the caller commits it only
    /// after authentication succeeds).
    fn estimate_index(&self, seq: u16) -> (u64, u32) {
        if !self.started {
            return (seq as u64, self.roc);
        }
        let roc = self.roc;
        let v = if self.s_l < 32768 {
            if seq.wrapping_sub(self.s_l) > 32768 {
                roc.wrapping_sub(1)
            } else {
                roc
            }
        } else if self.s_l - 32768 > seq {
            roc.wrapping_add(1)
        } else {
            roc
        };
        (((v as u64) << 16) | seq as u64, v)
    }

    /// Commit the index/roll-over state after a packet with `seq` estimated at roll-over `v` is
    /// accepted, advancing `s_l`/`roc` when the sequence moves forward.
    fn commit(&mut self, seq: u16, v: u32) {
        if !self.started {
            self.started = true;
            self.s_l = seq;
            self.roc = v;
            return;
        }
        if v > self.roc || (v == self.roc && seq > self.s_l) {
            self.s_l = seq;
            self.roc = v;
        }
    }

    /// Anti-replay window (RFC 3711 §3.3.2). Given the authenticated packet `index`, return
    /// `true` and record it if it is fresh (never seen and within the 64-packet window); return
    /// `false` if it is a replay or falls below the window. Must be called only *after* the auth
    /// tag verifies, so a forged packet cannot poison the window.
    const REPLAY_WINDOW: u64 = 64;
    fn replay_admit(&mut self, index: u64) -> bool {
        if !self.replay_started {
            self.replay_started = true;
            self.replay_top = index;
            self.replay_mask = 1; // bit 0 = the top index itself
            return true;
        }
        if index > self.replay_top {
            let delta = index - self.replay_top;
            if delta >= Self::REPLAY_WINDOW {
                self.replay_mask = 1;
            } else {
                self.replay_mask = (self.replay_mask << delta) | 1;
            }
            self.replay_top = index;
            true
        } else {
            let delta = self.replay_top - index;
            if delta >= Self::REPLAY_WINDOW {
                return false; // too old to tell — drop
            }
            let bit = 1u64 << delta;
            if self.replay_mask & bit != 0 {
                false // already seen — replay
            } else {
                self.replay_mask |= bit;
                true
            }
        }
    }

    /// Encrypt-and-authenticate one plaintext RTP packet into an SRTP packet
    /// (`header || AES-CM(payload) || tag`). Returns `None` if `rtp` is not a well-formed RTP
    /// packet. Advances this (sending) stream's index state.
    pub fn protect(&mut self, rtp: &[u8]) -> Option<Vec<u8>> {
        let header_len = rtp_header_len(rtp)?;
        let ssrc: [u8; 4] = rtp[8..12].try_into().ok()?;
        let seq = u16::from_be_bytes([rtp[2], rtp[3]]);
        let (index, v) = self.estimate_index(seq);

        let mut out = rtp.to_vec();
        // Encrypt the payload in place with the per-packet keystream.
        let iv = self.keystream_iv(ssrc, index);
        let mut cipher = Aes128Ctr::new((&self.session_key).into(), &iv.into());
        cipher.apply_keystream(&mut out[header_len..]);
        // Authenticate header || ciphertext (with ROC), append the 80-bit tag.
        let tag = self.auth_tag(&out, (index >> 16) as u32);
        out.extend_from_slice(&tag);

        self.commit(seq, v);
        Some(out)
    }

    /// Verify-and-decrypt one SRTP packet back to plaintext RTP. Returns `None` if the packet is
    /// malformed or the authentication tag does not verify (a forged/corrupt packet is dropped).
    /// Advances this (receiving) stream's index state only on success.
    pub fn unprotect(&mut self, srtp: &[u8]) -> Option<Vec<u8>> {
        if srtp.len() < AUTH_TAG_LEN {
            return None;
        }
        let body_len = srtp.len() - AUTH_TAG_LEN;
        let body = &srtp[..body_len];
        let tag = &srtp[body_len..];
        let header_len = rtp_header_len(body)?;
        let ssrc: [u8; 4] = body[8..12].try_into().ok()?;
        let seq = u16::from_be_bytes([body[2], body[3]]);
        let (index, v) = self.estimate_index(seq);

        // Constant-time tag comparison before touching the ciphertext.
        let expected = self.auth_tag(body, (index >> 16) as u32);
        if !constant_time_eq(&expected, tag) {
            return None;
        }

        // Anti-replay (RFC 3711 §3.3.2): drop a replayed or too-old (but validly-tagged) packet so
        // a captured frame cannot be re-injected. Checked only after the tag verifies.
        if !self.replay_admit(index) {
            return None;
        }

        let mut out = body.to_vec();
        let iv = self.keystream_iv(ssrc, index);
        let mut cipher = Aes128Ctr::new((&self.session_key).into(), &iv.into());
        cipher.apply_keystream(&mut out[header_len..]);

        self.commit(seq, v);
        Some(out)
    }
}

/// A pair of SRTP contexts for one call leg: the far end's key (to decrypt what it sends us) and
/// our key (to encrypt what we send it). SDES gives each side its own key.
pub struct SrtpSession {
    /// Decrypts packets arriving from the peer (peer's advertised key).
    pub inbound: SrtpContext,
    /// Encrypts packets CommOS sends to the peer (CommOS's advertised key).
    pub outbound: SrtpContext,
}

/// Generate a fresh random master key + salt for an outbound `a=crypto` offer/answer, sourced
/// from the OS CSPRNG (RFC 3711 keys must be unpredictable).
pub fn random_key_salt() -> [u8; KEY_SALT_LEN] {
    let mut ks = [0u8; KEY_SALT_LEN];
    getrandom::getrandom(&mut ks).expect("OS CSPRNG available for SRTP key generation");
    ks
}

/// Split a 30-byte SDES `inline:` key block into its master key (16) and master salt (14).
pub fn split_key_salt(ks: &[u8; KEY_SALT_LEN]) -> ([u8; MASTER_KEY_LEN], [u8; MASTER_SALT_LEN]) {
    let mut key = [0u8; MASTER_KEY_LEN];
    let mut salt = [0u8; MASTER_SALT_LEN];
    key.copy_from_slice(&ks[..MASTER_KEY_LEN]);
    salt.copy_from_slice(&ks[MASTER_KEY_LEN..]);
    (key, salt)
}

/// RTP header length (12 + CSRC list + optional extension), or `None` if `pkt` is too short to be
/// a valid RTP packet.
fn rtp_header_len(pkt: &[u8]) -> Option<usize> {
    if pkt.len() < 12 {
        return None;
    }
    let cc = (pkt[0] & 0x0f) as usize;
    let has_ext = (pkt[0] & 0x10) != 0;
    let mut len = 12 + 4 * cc;
    if has_ext {
        if pkt.len() < len + 4 {
            return None;
        }
        let ext_words = u16::from_be_bytes([pkt[len + 2], pkt[len + 3]]) as usize;
        len += 4 + 4 * ext_words;
    }
    (pkt.len() >= len).then_some(len)
}

/// Constant-time byte-slice equality, so tag verification does not leak timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// AES-128 single-block encryption (used only to validate the engine against FIPS-197 in tests;
/// the media path uses the counter-mode [`Aes128Ctr`] above).
#[cfg(test)]
fn aes128_encrypt_block(key: &[u8; 16], block: &mut [u8; 16]) {
    use aes::cipher::{BlockEncrypt, KeyInit};
    let cipher = aes::Aes128::new(key.into());
    let mut b = aes::cipher::generic_array::GenericArray::clone_from_slice(block);
    cipher.encrypt_block(&mut b);
    block.copy_from_slice(&b);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn aes128_matches_fips197() {
        // FIPS-197 Appendix B known-answer test for the raw AES-128 block cipher.
        let key: [u8; 16] = hex("000102030405060708090a0b0c0d0e0f").try_into().unwrap();
        let mut block: [u8; 16] = hex("00112233445566778899aabbccddeeff").try_into().unwrap();
        aes128_encrypt_block(&key, &mut block);
        assert_eq!(block.to_vec(), hex("69c4e0d86a7b0430d8cdb78070b4c55a"));
    }

    #[test]
    fn kdf_matches_rfc3711_b3() {
        // RFC 3711 Appendix B.3 key-derivation test vectors.
        let master_key: [u8; 16] = hex("E1F97A0D3E018BE0D64FA32C06DE4139").try_into().unwrap();
        let master_salt: [u8; 14] = hex("0EC675AD498AFEEBB6960B3AABE6").try_into().unwrap();
        let ctx = SrtpContext::new(&master_key, &master_salt);
        assert_eq!(ctx.session_key.to_vec(), hex("C61E7A93744F39EE10734AFE3FF7A087"), "cipher key");
        assert_eq!(ctx.session_salt.to_vec(), hex("30CBBC08863D8C85D49DB34A9AE1"), "cipher salt");
        assert_eq!(
            ctx.auth_key.to_vec(),
            hex("CEBE321F6FF7716B6FD4AB49AF256A156D38BAA4"),
            "auth key"
        );
    }

    /// A minimal RTP packet: V=2, no CSRC/ext, given payload type, seq, ts, ssrc, and payload.
    fn rtp(seq: u16, ssrc: u32, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0x80, 0x00];
        p.extend_from_slice(&seq.to_be_bytes());
        p.extend_from_slice(&0u32.to_be_bytes()); // timestamp
        p.extend_from_slice(&ssrc.to_be_bytes());
        p.extend_from_slice(payload);
        p
    }

    #[test]
    fn protect_then_unprotect_round_trips() {
        let ks = random_key_salt();
        let (k, s) = split_key_salt(&ks);
        let mut send = SrtpContext::new(&k, &s);
        let mut recv = SrtpContext::new(&k, &s);
        let clear = rtp(1000, 0xdead_beef, b"the quick brown fox");
        let protected = send.protect(&clear).unwrap();
        // Ciphertext differs from plaintext and carries the 10-byte tag.
        assert_eq!(protected.len(), clear.len() + AUTH_TAG_LEN);
        assert_ne!(&protected[12..12 + 19], &clear[12..]);
        let recovered = recv.unprotect(&protected).unwrap();
        assert_eq!(recovered, clear);
    }

    #[test]
    fn replayed_packet_is_dropped() {
        let (k, s) = split_key_salt(&random_key_salt());
        let mut send = SrtpContext::new(&k, &s);
        let mut recv = SrtpContext::new(&k, &s);
        let p1 = send.protect(&rtp(100, 0xabcd, b"one")).unwrap();
        let p2 = send.protect(&rtp(101, 0xabcd, b"two")).unwrap();
        // First receipt of each succeeds.
        assert!(recv.unprotect(&p1).is_some());
        assert!(recv.unprotect(&p2).is_some());
        // A byte-for-byte replay of an already-accepted packet is dropped (validly tagged, but
        // its index was already seen).
        assert!(recv.unprotect(&p1).is_none(), "replay of p1 must be dropped");
        assert!(recv.unprotect(&p2).is_none(), "replay of p2 must be dropped");
        // A fresh, later packet is still accepted.
        let p3 = send.protect(&rtp(102, 0xabcd, b"three")).unwrap();
        assert!(recv.unprotect(&p3).is_some());
    }

    #[test]
    fn tampered_packet_is_rejected() {
        let (k, s) = split_key_salt(&random_key_salt());
        let mut send = SrtpContext::new(&k, &s);
        let mut recv = SrtpContext::new(&k, &s);
        let mut protected = send.protect(&rtp(7, 1, b"hello")).unwrap();
        // Flip a ciphertext bit → the auth tag must fail and the packet is dropped.
        protected[13] ^= 0x01;
        assert!(recv.unprotect(&protected).is_none());
    }

    #[test]
    fn wrong_key_fails_authentication() {
        let (k1, s1) = split_key_salt(&random_key_salt());
        let (k2, s2) = split_key_salt(&random_key_salt());
        let mut send = SrtpContext::new(&k1, &s1);
        let mut recv = SrtpContext::new(&k2, &s2);
        let protected = send.protect(&rtp(1, 1, b"secret")).unwrap();
        assert!(recv.unprotect(&protected).is_none());
    }

    #[test]
    fn sequence_rollover_advances_roc() {
        let (k, s) = split_key_salt(&random_key_salt());
        let mut send = SrtpContext::new(&k, &s);
        let mut recv = SrtpContext::new(&k, &s);
        // Two packets straddling the 16-bit sequence wrap must still decrypt (ROC increments).
        for seq in [65534u16, 65535, 0, 1] {
            let clear = rtp(seq, 0xabcd, b"payload");
            let protected = send.protect(&clear).unwrap();
            assert_eq!(recv.unprotect(&protected).unwrap(), clear);
        }
        assert_eq!(recv.roc, 1, "ROC should have rolled over once");
    }
}
