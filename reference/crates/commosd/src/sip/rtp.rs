//! Minimal RTP media path (Volume 7).
//!
//! A real media plane relays RTP between call legs, transcodes, and mixes conferences off
//! the hot path (CMOS-03-ARCH-021). This first cut proves the RTP path end-to-end with an
//! **echo test**: a UDP socket that reflects received packets back to their sender, so a
//! caller placing an inbound call hears themselves. Each Call gets its own socket + task;
//! aborting the task on BYE tears the media down.

use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

/// Bind an ephemeral UDP socket for a Call's RTP and echo datagrams back to their sender.
/// Returns the bound port (to advertise in SDP) and the task handle (abort on hangup).
pub async fn bind_echo() -> std::io::Result<(u16, JoinHandle<()>)> {
    // Bind all interfaces on an OS-assigned port; SDP advertises the configured media IP.
    let sock = UdpSocket::bind("0.0.0.0:0").await?;
    let port = sock.local_addr()?.port();
    let handle = tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        loop {
            match sock.recv_from(&mut buf).await {
                Ok((n, peer)) => {
                    // Reflect the RTP packet straight back to the caller (echo).
                    let _ = sock.send_to(&buf[..n], peer).await;
                }
                Err(_) => break,
            }
        }
    });
    Ok((port, handle))
}
