//! Minimal RTP media path (Volume 7).
//!
//! A real media plane relays RTP between call legs, transcodes, and mixes conferences off
//! the hot path (CMOS-03-ARCH-021). This first cut proves the RTP path end-to-end with an
//! **echo test**: a UDP socket that reflects received packets back to their sender, so a
//! caller placing an inbound call hears themselves. Each Call gets its own socket + task;
//! aborting the task on BYE tears the media down.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

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
/// The ports are advertised in each leg's SDP (`leg_a_port` to the caller, `leg_b_port` to
/// the callee). Dropping/[`abort`](Self::abort)ing tears the relay task down.
pub struct Bridge {
    /// UDP port facing the caller — advertise this in the SDP answer sent to the caller.
    pub leg_a_port: u16,
    /// UDP port facing the callee — advertise this in the SDP offer sent to the callee.
    pub leg_b_port: u16,
    task: JoinHandle<()>,
}

impl Bridge {
    /// Tear the relay down (aborts the background task; sockets are then dropped).
    pub fn abort(self) {
        self.task.abort();
    }
}

/// Bind two ephemeral UDP sockets and relay RTP between them once each leg has latched onto
/// its peer. Returns the two bound ports (to advertise in SDP) inside a [`Bridge`].
pub async fn bind_bridge(capture: Option<Capture>) -> std::io::Result<Bridge> {
    // Bind both legs on OS-assigned ports on all interfaces; SDP advertises the media IP.
    let leg_a = UdpSocket::bind("0.0.0.0:0").await?;
    let leg_b = UdpSocket::bind("0.0.0.0:0").await?;
    let leg_a_port = leg_a.local_addr()?.port();
    let leg_b_port = leg_b.local_addr()?.port();

    let task = tokio::spawn(async move {
        // Each leg's peer address, learned from the first datagram it receives (latching).
        let mut peer_a: Option<SocketAddr> = None;
        let mut peer_b: Option<SocketAddr> = None;
        let mut buf_a = [0u8; 2048];
        let mut buf_b = [0u8; 2048];
        loop {
            tokio::select! {
                // Packet from the caller side → forward to the callee's learned peer.
                r = leg_a.recv_from(&mut buf_a) => match r {
                    Ok((n, from)) => {
                        if peer_a != Some(from) {
                            tracing::debug!(leg = "A", %from, "RTP bridge latched caller peer");
                            peer_a = Some(from);
                        }
                        // Record the caller's leg (mono G.711 as-is; full dual-channel/mixed
                        // recording is future media-plane work).
                        if let Some(cap) = &capture {
                            capture_payload(cap, &buf_a[..n]);
                        }
                        if let Some(dst) = peer_b {
                            let _ = leg_b.send_to(&buf_a[..n], dst).await;
                        }
                    }
                    Err(_) => break,
                },
                // Packet from the callee side → forward to the caller's learned peer.
                r = leg_b.recv_from(&mut buf_b) => match r {
                    Ok((n, from)) => {
                        if peer_b != Some(from) {
                            tracing::debug!(leg = "B", %from, "RTP bridge latched callee peer");
                            peer_b = Some(from);
                        }
                        if let Some(dst) = peer_a {
                            let _ = leg_a.send_to(&buf_b[..n], dst).await;
                        }
                    }
                    Err(_) => break,
                },
            }
        }
    });

    Ok(Bridge {
        leg_a_port,
        leg_b_port,
        task,
    })
}

/// Bind an ephemeral UDP socket for a Call's RTP and echo datagrams back to their sender.
/// Returns the bound port (to advertise in SDP) and the task handle (abort on hangup).
pub async fn bind_echo(capture: Option<Capture>) -> std::io::Result<(u16, JoinHandle<()>)> {
    // Bind all interfaces on an OS-assigned port; SDP advertises the configured media IP.
    let sock = UdpSocket::bind("0.0.0.0:0").await?;
    let port = sock.local_addr()?.port();
    let handle = tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        // Reflect each RTP packet straight back to the caller (echo) until the socket errors,
        // capturing the caller's payload when recording is on.
        while let Ok((n, peer)) = sock.recv_from(&mut buf).await {
            if let Some(cap) = &capture {
                capture_payload(cap, &buf[..n]);
            }
            let _ = sock.send_to(&buf[..n], peer).await;
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
        let bridge = bind_bridge(None).await.expect("bind bridge");
        let leg_a: SocketAddr = format!("127.0.0.1:{}", bridge.leg_a_port).parse().unwrap();
        let leg_b: SocketAddr = format!("127.0.0.1:{}", bridge.leg_b_port).parse().unwrap();

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
        let (port, task) = bind_echo(Some(cap.clone())).await.unwrap();
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
