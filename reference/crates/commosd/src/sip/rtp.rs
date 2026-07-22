//! Minimal RTP media path (Volume 7).
//!
//! A real media plane relays RTP between call legs, transcodes, and mixes conferences off
//! the hot path (CMOS-03-ARCH-021). This first cut proves the RTP path end-to-end with an
//! **echo test**: a UDP socket that reflects received packets back to their sender, so a
//! caller placing an inbound call hears themselves. Each Call gets its own socket + task;
//! aborting the task on BYE tears the media down.

use std::borrow::Cow;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::srtp::SrtpSession;

/// One side of a [`Bridge`]. Leg A faces the caller (the original INVITE sender), leg B the
/// callee.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leg {
    A,
    /// The relay handles holding either leg symmetrically, but only caller-side hold
    /// (`Held(Leg::A)`) is SIP-wired today — a *callee*-initiated hold re-INVITE arrives on the
    /// leg-B dialog, which the in-dialog handler does not yet route. Wiring that is a documented
    /// follow-up; the media path is already ready for it.
    #[allow(dead_code)]
    B,
}

/// Whether the bridge is relaying normally or one leg's phone has put the call on hold. While
/// held, the relay stops forwarding between the legs and streams music-on-hold to the leg that
/// is *not* holding, so the held party hears music instead of silence (the TODO.md gap).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoldState {
    /// Normal two-way relay.
    Active,
    /// The named leg's phone initiated hold → play MoH to the *other* leg, stop relaying.
    Held(Leg),
}

/// A shared capture buffer for call recording. The RTP task appends the **payload** of each
/// received packet (RTP header stripped) so the buffer is the raw audio bytes as-negotiated —
/// no transcoding (Volume 7: store audio as-is; the consumer decodes). For G.711 (PCMU/PCMA)
/// that is an 8 kHz mono stream a browser can decode client-side.
pub type Capture = Arc<Mutex<Vec<u8>>>;

/// Fixed RTP header length (no CSRC/extension — the common case for G.711 desk phones).
const RTP_HEADER_LEN: usize = 12;
/// Cap a single recording so a long/abandoned call can't exhaust memory (~35 min of G.711).
const MAX_CAPTURE_BYTES: usize = 16 * 1024 * 1024;

/// Append one RTP packet's payload to the capture buffer, bounded by [`MAX_CAPTURE_BYTES`].
fn capture_payload(cap: &Capture, packet: &[u8]) {
    if packet.len() <= RTP_HEADER_LEN {
        return;
    }
    let payload = &packet[RTP_HEADER_LEN..];
    let mut buf = cap.lock().expect("capture mutex not poisoned");
    if buf.len() + payload.len() <= MAX_CAPTURE_BYTES {
        buf.extend_from_slice(payload);
    }
}

/// A two-leg RTP relay bridging two call legs (CMOS-03-ARCH-021).
///
/// Leg A faces the caller, leg B faces the callee. Each leg discovers its remote peer's
/// address from the FIRST datagram it receives (symmetric RTP / "latching", RFC 7362) so we
/// never need the phones' actual RTP source ports up front — whatever address a packet
/// arrives from becomes that leg's peer. Thereafter every datagram received on A is
/// forwarded to B's learned peer and vice-versa.
///
/// The caller-facing port is advertised in the SDP answer to the caller (the callee-facing port
/// was advertised from [`PendingBridge`] before the relay started).
/// Dropping/[`abort`](Self::abort)ing tears the relay task down.
pub struct Bridge {
    /// UDP port facing the caller — advertise this in the SDP answer sent to the caller.
    pub leg_a_port: u16,
    task: JoinHandle<()>,
    /// Hold control: a re-INVITE handler flips this to start/stop music-on-hold.
    hold: watch::Sender<HoldState>,
}

impl Bridge {
    /// Tear the relay down (aborts the background task; sockets are then dropped).
    pub fn abort(self) {
        self.task.abort();
    }

    /// Signal the relay to enter/leave hold. A no-op once the relay task has ended (the caller
    /// need not care whether the bridge is still live).
    pub fn set_hold(&self, state: HoldState) {
        let _ = self.hold.send(state);
    }
}

/// Two ephemeral UDP sockets bound for a bridge, before the relay starts. The ports are known (so
/// they can be advertised in the leg-B offer and leg-A answer), but the SRTP keys — the callee's
/// leg-B key in particular — aren't settled until the callee answers, so binding and relaying are
/// split: bind now, [`start`](PendingBridge::start) the relay once both legs' keys are known.
pub struct PendingBridge {
    leg_a: UdpSocket,
    leg_b: UdpSocket,
    /// UDP port facing the caller — advertise this in the SDP answer sent to the caller.
    pub leg_a_port: u16,
    /// UDP port facing the callee — advertise this in the SDP offer sent to the callee.
    pub leg_b_port: u16,
}

/// Bind the two ephemeral UDP sockets for a bridge (OS-assigned ports on all interfaces; SDP
/// advertises the media IP), returning them before the relay is wired up.
pub async fn bind_bridge_sockets() -> std::io::Result<PendingBridge> {
    let leg_a = UdpSocket::bind("0.0.0.0:0").await?;
    let leg_b = UdpSocket::bind("0.0.0.0:0").await?;
    let leg_a_port = leg_a.local_addr()?.port();
    let leg_b_port = leg_b.local_addr()?.port();
    Ok(PendingBridge { leg_a, leg_b, leg_a_port, leg_b_port })
}

impl PendingBridge {
    /// Start relaying RTP between the two legs once each latches onto its peer.
    ///
    /// SRTP is terminated **independently per leg** (a B2BUA decrypts the sending leg and
    /// re-encrypts for the receiving leg — media is only ever plaintext inside CommOS): `srtp_a`
    /// keys the caller leg, `srtp_b` the callee leg, and either may be `None` for a plaintext leg,
    /// so an encrypted caller can bridge to a plaintext callee and vice-versa. Each direction
    /// decrypts with the source leg's `inbound` context and re-encrypts with the destination leg's
    /// `outbound` context; a packet that fails authentication is dropped. Recording captures the
    /// plaintext.
    pub fn start(
        self,
        capture: Option<Capture>,
        srtp_a: Option<SrtpSession>,
        srtp_b: Option<SrtpSession>,
        moh: Option<MohLoop>,
    ) -> Bridge {
        let PendingBridge { leg_a, leg_b, leg_a_port, leg_b_port: _ } = self;
        let (hold_tx, mut hold_rx) = watch::channel(HoldState::Active);
        let task = tokio::spawn(async move {
            // Each leg's peer address, learned from the first datagram it receives (latching).
            let mut peer_a: Option<SocketAddr> = None;
            let mut peer_b: Option<SocketAddr> = None;
            let mut buf_a = [0u8; 2048];
            let mut buf_b = [0u8; 2048];
            let mut srtp_a = srtp_a;
            let mut srtp_b = srtp_b;
            let mut hold = HoldState::Active;

            // Music-on-hold state: the loop pre-chunked into 20 ms frames + an RTP cursor. When a
            // leg is held, the 20 ms ticker streams these frames to the *other* leg.
            let moh_frames: Vec<Vec<u8>> = moh
                .as_ref()
                .map(|m| m.audio.chunks(FRAME_BYTES).map(|c| c.to_vec()).collect())
                .unwrap_or_default();
            let moh_pt = moh.as_ref().map(|m| m.payload_type).unwrap_or(0);
            let mut moh_idx = 0usize;
            let mut moh_seq: u16 = 0;
            let mut moh_ts: u32 = 0;
            let mut ticker = tokio::time::interval(Duration::from_millis(20));

            loop {
                tokio::select! {
                    // A hold/resume signal from the re-INVITE handler.
                    changed = hold_rx.changed() => {
                        if changed.is_ok() {
                            hold = *hold_rx.borrow();
                        }
                    }
                    // Packet from the caller side → decrypt (leg A), re-encrypt (leg B), forward.
                    r = leg_a.recv_from(&mut buf_a) => match r {
                        Ok((n, from)) => {
                            if peer_a != Some(from) {
                                tracing::debug!(leg = "A", %from, "RTP bridge latched caller peer");
                                peer_a = Some(from);
                            }
                            // Decrypt the caller leg (or pass through when it is plaintext).
                            let plain = match srtp_a.as_mut() {
                                Some(s) => match s.inbound.unprotect(&buf_a[..n]) {
                                    Some(p) => Cow::Owned(p),
                                    None => continue, // drop a forged/corrupt packet
                                },
                                None => Cow::Borrowed(&buf_a[..n]),
                            };
                            // Record the caller's plaintext leg (mono G.711 as-is; full
                            // dual-channel/mixed recording is future media-plane work).
                            if let Some(cap) = &capture {
                                capture_payload(cap, &plain);
                            }
                            // While held, do not relay between the legs (the held party hears MoH).
                            if matches!(hold, HoldState::Active) {
                                if let Some(dst) = peer_b {
                                    forward(&leg_b, dst, &plain, srtp_b.as_mut()).await;
                                }
                            }
                        }
                        Err(_) => break,
                    },
                    // Packet from the callee side → decrypt (leg B), re-encrypt (leg A), forward.
                    r = leg_b.recv_from(&mut buf_b) => match r {
                        Ok((n, from)) => {
                            if peer_b != Some(from) {
                                tracing::debug!(leg = "B", %from, "RTP bridge latched callee peer");
                                peer_b = Some(from);
                            }
                            let plain = match srtp_b.as_mut() {
                                Some(s) => match s.inbound.unprotect(&buf_b[..n]) {
                                    Some(p) => Cow::Owned(p),
                                    None => continue,
                                },
                                None => Cow::Borrowed(&buf_b[..n]),
                            };
                            if matches!(hold, HoldState::Active) {
                                if let Some(dst) = peer_a {
                                    forward(&leg_a, dst, &plain, srtp_a.as_mut()).await;
                                }
                            }
                        }
                        Err(_) => break,
                    },
                    // 20 ms tick: while a leg is held, stream one MoH frame to the *other* leg. The
                    // arm is disabled entirely when there is no MoH loop, so a call that never holds
                    // pays nothing.
                    _ = ticker.tick(), if !moh_frames.is_empty() => {
                        let target = match hold {
                            HoldState::Held(Leg::A) => peer_b.map(|dst| (&leg_b, dst, srtp_b.as_mut())),
                            HoldState::Held(Leg::B) => peer_a.map(|dst| (&leg_a, dst, srtp_a.as_mut())),
                            HoldState::Active => None,
                        };
                        if let Some((sock, dst, srtp)) = target {
                            let payload = &moh_frames[moh_idx % moh_frames.len()];
                            let pkt = super::ivr::rtp_frame(moh_pt, moh_seq, moh_ts, payload, moh_seq == 0);
                            forward(sock, dst, &pkt, srtp).await;
                            moh_seq = moh_seq.wrapping_add(1);
                            moh_ts = moh_ts.wrapping_add(FRAME_BYTES as u32);
                            moh_idx += 1;
                        }
                    }
                }
            }
        });
        Bridge { leg_a_port, task, hold: hold_tx }
    }
}

/// A music-on-hold loop for a [`Bridge`]: G.711 audio already transcoded to the bridge's
/// negotiated codec, plus the RTP payload type to packetise it with.
pub struct MohLoop {
    pub audio: Vec<u8>,
    pub payload_type: u8,
}

/// 20 ms of 8 kHz G.711 = 160 bytes — the frame size used for MoH packetisation.
const FRAME_BYTES: usize = 160;

/// Send `plain` (a plaintext RTP packet) to `dst` on `sock`, encrypting it with the destination
/// leg's `outbound` SRTP context first when that leg is secure.
async fn forward(sock: &UdpSocket, dst: SocketAddr, plain: &[u8], srtp: Option<&mut SrtpSession>) {
    match srtp {
        Some(s) => {
            if let Some(enc) = s.outbound.protect(plain) {
                let _ = sock.send_to(&enc, dst).await;
            }
        }
        None => {
            let _ = sock.send_to(plain, dst).await;
        }
    }
}

/// Bind an ephemeral UDP socket for a Call's RTP and echo datagrams back to their sender.
/// Returns the bound port (to advertise in SDP) and the task handle (abort on hangup).
///
/// When `srtp` is `Some`, CommOS is the SRTP endpoint: each inbound packet is authenticated and
/// decrypted with the caller's key before it is captured/echoed, and each outbound packet is
/// re-encrypted with CommOS's key. Capture therefore always stores plaintext G.711 (as before);
/// only the wire is protected. A packet that fails authentication is dropped.
pub async fn bind_echo(
    capture: Option<Capture>,
    srtp: Option<super::srtp::SrtpSession>,
) -> std::io::Result<(u16, JoinHandle<()>)> {
    // Bind all interfaces on an OS-assigned port; SDP advertises the configured media IP.
    let sock = UdpSocket::bind("0.0.0.0:0").await?;
    let port = sock.local_addr()?.port();
    let handle = tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        let mut srtp = srtp;
        // Reflect each RTP packet straight back to the caller (echo) until the socket errors,
        // capturing the caller's payload when recording is on.
        while let Ok((n, peer)) = sock.recv_from(&mut buf).await {
            match &mut srtp {
                Some(s) => {
                    // Drop packets that don't authenticate (forged/corrupt).
                    let Some(plain) = s.inbound.unprotect(&buf[..n]) else { continue };
                    if let Some(cap) = &capture {
                        capture_payload(cap, &plain);
                    }
                    if let Some(enc) = s.outbound.protect(&plain) {
                        let _ = sock.send_to(&enc, peer).await;
                    }
                }
                None => {
                    if let Some(cap) = &capture {
                        capture_payload(cap, &buf[..n]);
                    }
                    let _ = sock.send_to(&buf[..n], peer).await;
                }
            }
        }
    });
    Ok((port, handle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    /// Drive two local UDP "phones" through a [`bind_bridge`] and assert the relay carries a
    /// packet A→B and B→A once each leg has latched onto its peer.
    #[tokio::test]
    async fn bridge_relays_both_directions() {
        let pending = bind_bridge_sockets().await.expect("bind bridge sockets");
        let leg_a: SocketAddr = format!("127.0.0.1:{}", pending.leg_a_port).parse().unwrap();
        let leg_b: SocketAddr = format!("127.0.0.1:{}", pending.leg_b_port).parse().unwrap();
        let bridge = pending.start(None, None, None, None);

        // Two "phones", each bound to its own local port.
        let phone_a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let phone_b = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        // Prime each leg so the bridge latches onto both peers' addresses.
        phone_a.send_to(b"latch-a", leg_a).await.unwrap();
        phone_b.send_to(b"latch-b", leg_b).await.unwrap();

        // Give the bridge a moment to receive the priming datagrams and latch.
        let mut scratch = [0u8; 2048];
        // Drain any relayed priming packets (order-dependent; ignore what lands here).
        let _ = timeout(Duration::from_millis(50), phone_a.recv_from(&mut scratch)).await;
        let _ = timeout(Duration::from_millis(50), phone_b.recv_from(&mut scratch)).await;

        // A → B: a packet phone-A sends to leg A must arrive at phone-B via leg B.
        phone_a.send_to(b"hello-b", leg_a).await.unwrap();
        let mut buf = [0u8; 2048];
        let (n, _) = timeout(Duration::from_secs(1), phone_b.recv_from(&mut buf))
            .await
            .expect("A→B relay timed out")
            .expect("recv A→B");
        assert_eq!(&buf[..n], b"hello-b");

        // B → A: a packet phone-B sends to leg B must arrive at phone-A via leg A.
        phone_b.send_to(b"hello-a", leg_b).await.unwrap();
        let (n, _) = timeout(Duration::from_secs(1), phone_a.recv_from(&mut buf))
            .await
            .expect("B→A relay timed out")
            .expect("recv B→A");
        assert_eq!(&buf[..n], b"hello-a");

        bridge.abort();
    }

    /// A B2BUA terminates SRTP per leg: a packet phone-A encrypts with its key must arrive at
    /// phone-B decryptable with a *different* key — the bridge decrypts leg A and re-encrypts for
    /// leg B, so the two legs never share key material and the media is only plaintext in between.
    #[tokio::test]
    async fn bridge_reencrypts_srtp_across_legs() {
        use super::super::srtp::{random_key_salt, split_key_salt, SrtpContext, SrtpSession};

        // Key A: phone A ↔ the caller leg. Key B: the callee leg ↔ phone B. Distinct keys.
        let ka = random_key_salt();
        let kb = random_key_salt();
        let ctx = |ks: &[u8; 30]| {
            let (k, s) = split_key_salt(ks);
            SrtpContext::new(&k, &s)
        };
        // Leg A decrypts what phone A sends (key A); leg B encrypts toward phone B (key B). The
        // unused directions get throwaway keys.
        let srtp_a = SrtpSession { inbound: ctx(&ka), outbound: ctx(&random_key_salt()) };
        let srtp_b = SrtpSession { inbound: ctx(&random_key_salt()), outbound: ctx(&kb) };

        let pending = bind_bridge_sockets().await.expect("bind sockets");
        let leg_a: SocketAddr = format!("127.0.0.1:{}", pending.leg_a_port).parse().unwrap();
        let leg_b: SocketAddr = format!("127.0.0.1:{}", pending.leg_b_port).parse().unwrap();
        let bridge = pending.start(None, Some(srtp_a), Some(srtp_b), None);

        let phone_a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let phone_b = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        // Latch leg B's peer (the datagram fails SRTP auth and is dropped, but latching happens
        // first), so the bridge knows where to forward re-encrypted audio.
        phone_b.send_to(b"latch", leg_b).await.unwrap();
        let mut scratch = [0u8; 2048];
        let _ = timeout(Duration::from_millis(50), phone_a.recv_from(&mut scratch)).await;

        // phone A encrypts an RTP packet with key A and sends it to leg A.
        let mut a_out = ctx(&ka);
        let mut rtp = vec![0x80, 0x00, 0x00, 0x2a]; // V=2, PT=0, seq=42
        rtp.extend_from_slice(&0u32.to_be_bytes()); // timestamp
        rtp.extend_from_slice(&0x11u32.to_be_bytes()); // SSRC
        rtp.extend_from_slice(b"secret-audio");
        let sent = a_out.protect(&rtp).expect("protect");
        phone_a.send_to(&sent, leg_a).await.unwrap();

        // phone B receives an SRTP packet it can decrypt only with key B → the original plaintext.
        let mut buf = [0u8; 2048];
        let (n, _) = timeout(Duration::from_secs(1), phone_b.recv_from(&mut buf))
            .await
            .expect("A→B SRTP relay timed out")
            .expect("recv A→B");
        // The re-encrypted wire bytes are not phone A's ciphertext (different key) …
        assert_ne!(&buf[..n], &sent[..], "bridge must re-encrypt, not forward leg-A ciphertext");
        // … and decrypt with key B back to exactly what phone A sent.
        let mut b_in = ctx(&kb);
        let recovered = b_in.unprotect(&buf[..n]).expect("phone B decrypts with key B");
        assert_eq!(recovered, rtp);

        bridge.abort();
    }

    /// While a leg is held the other leg hears music-on-hold (not silence), and relaying is
    /// suspended; on resume the two-way relay comes back.
    #[tokio::test]
    async fn hold_streams_moh_to_other_leg_then_resume_relays() {
        let pending = bind_bridge_sockets().await.expect("bind sockets");
        let leg_a: SocketAddr = format!("127.0.0.1:{}", pending.leg_a_port).parse().unwrap();
        let leg_b: SocketAddr = format!("127.0.0.1:{}", pending.leg_b_port).parse().unwrap();
        // A one-frame MoH loop with a recognisable payload (0xAB × 160), PCMU payload type.
        let moh = MohLoop { audio: vec![0xAB; FRAME_BYTES], payload_type: 0 };
        let bridge = pending.start(None, None, None, Some(moh));

        let phone_a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let phone_b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // Latch both legs.
        phone_a.send_to(b"latch-a", leg_a).await.unwrap();
        phone_b.send_to(b"latch-b", leg_b).await.unwrap();
        let mut scratch = [0u8; 2048];
        let _ = timeout(Duration::from_millis(50), phone_a.recv_from(&mut scratch)).await;
        let _ = timeout(Duration::from_millis(50), phone_b.recv_from(&mut scratch)).await;

        // Caller (leg A) holds → phone B (the other leg) receives MoH RTP frames.
        bridge.set_hold(HoldState::Held(Leg::A));
        let mut buf = [0u8; 2048];
        let (n, _) = timeout(Duration::from_secs(1), phone_b.recv_from(&mut buf))
            .await
            .expect("MoH should reach the held leg")
            .expect("recv MoH");
        assert!(n > RTP_HEADER_LEN, "MoH packet carries an RTP header + payload");
        assert!(
            buf[RTP_HEADER_LEN..n].iter().all(|&b| b == 0xAB),
            "the held leg hears the music-on-hold payload"
        );

        // Resume → relay works again. Drain any buffered MoH, then relay a packet A→B.
        bridge.set_hold(HoldState::Active);
        let drain_until = tokio::time::Instant::now() + Duration::from_millis(80);
        while tokio::time::Instant::now() < drain_until {
            let _ = timeout(Duration::from_millis(20), phone_b.recv_from(&mut scratch)).await;
        }
        phone_a.send_to(b"relayed-after-resume", leg_a).await.unwrap();
        let relayed = timeout(Duration::from_secs(1), async {
            loop {
                let (n, _) = phone_b.recv_from(&mut buf).await.unwrap();
                if &buf[..n] == b"relayed-after-resume" {
                    break;
                }
            }
        })
        .await;
        assert!(relayed.is_ok(), "two-way relay resumes once hold ends");

        bridge.abort();
    }

    #[test]
    fn capture_strips_rtp_header_and_caps() {
        let cap: Capture = Arc::new(Mutex::new(Vec::new()));
        // 12-byte header + 3-byte payload → only the payload is captured.
        let mut pkt = vec![0u8; RTP_HEADER_LEN];
        pkt.extend_from_slice(b"\x01\x02\x03");
        capture_payload(&cap, &pkt);
        assert_eq!(cap.lock().unwrap().as_slice(), b"\x01\x02\x03");
        // A runt packet (header only, no payload) adds nothing.
        capture_payload(&cap, &[0u8; RTP_HEADER_LEN]);
        assert_eq!(cap.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn echo_captures_caller_payload() {
        let cap: Capture = Arc::new(Mutex::new(Vec::new()));
        let (port, task) = bind_echo(Some(cap.clone()), None).await.unwrap();
        let phone = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let echo: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        // Send an RTP-shaped packet (12B header + payload); expect it echoed + captured.
        let mut pkt = vec![0u8; RTP_HEADER_LEN];
        pkt.extend_from_slice(b"AUDIO");
        phone.send_to(&pkt, echo).await.unwrap();
        let mut buf = [0u8; 2048];
        let (n, _) = timeout(Duration::from_secs(1), phone.recv_from(&mut buf))
            .await
            .expect("echo timed out")
            .expect("recv echo");
        assert_eq!(&buf[..n], &pkt[..]);
        // The captured buffer holds only the payload, header stripped.
        assert_eq!(cap.lock().unwrap().as_slice(), b"AUDIO");
        task.abort();
    }
}
