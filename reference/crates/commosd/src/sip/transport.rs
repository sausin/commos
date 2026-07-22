//! SIP transport abstraction (Volume 7) — decouples the request handlers from the wire.
//!
//! SIP runs over both a **datagram** transport (UDP, one message per datagram, reply to the
//! source address) and **stream** transports (TCP, and TLS = TCP + rustls — RFC 3261 §7.5),
//! where messages have no datagram boundaries and a reply must go back on the *same* connection.
//! [`Responder`] hides that difference so [`super::server`]'s handlers are transport-agnostic:
//! they take a `&Responder` and call [`Responder::send`], never a bare socket + address.
//!
//! [`StreamFramer`] is the other half of a stream transport: it re-derives message boundaries
//! from a byte stream using the `Content-Length` header, so the handlers keep receiving whole
//! parsed messages exactly as they do from a UDP datagram.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use super::message;

/// Where a SIP reply goes — the transport-agnostic reply handle threaded through the handlers.
///
/// Cheap to clone (an `Arc`/`Sender`), so a fresh `Responder` is built per inbound message and
/// carried into the handler that answers it.
#[derive(Clone)]
pub enum Responder {
    /// Connectionless: reply is `send_to(bytes, dst)` on the shared ingress UDP socket.
    Udp { socket: Arc<UdpSocket>, dst: SocketAddr },
    /// Stream (TCP/TLS): reply is written back on *this* connection. The `mpsc` feeds a
    /// per-connection writer task, keeping the handle `Clone`/`Send` like the UDP arm and
    /// serialising concurrent writes (100-Trying, a bridge answer, an MWI NOTIFY) onto the one
    /// connection through a single writer. Constructed only by a stream transport ([`super::tls`]).
    #[cfg_attr(not(feature = "tls"), allow(dead_code))]
    Stream { tx: mpsc::Sender<Vec<u8>>, peer: SocketAddr },
}

impl Responder {
    /// Send one SIP message back to the peer over whichever transport this responder wraps.
    pub async fn send(&self, bytes: &[u8]) -> std::io::Result<()> {
        match self {
            Responder::Udp { socket, dst } => {
                socket.send_to(bytes, *dst).await?;
                Ok(())
            }
            Responder::Stream { tx, .. } => tx.send(bytes.to_vec()).await.map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::BrokenPipe, "SIP stream connection closed")
            }),
        }
    }

    /// The peer's address — the UDP source or the stream connection's remote — for logging and
    /// for synthesising a `Contact` when a REGISTER omits one.
    pub fn peer(&self) -> SocketAddr {
        match self {
            Responder::Udp { dst, .. } => *dst,
            Responder::Stream { peer, .. } => *peer,
        }
    }

    /// Whether this transport is confidential. The stream transport is SIP-over-TLS (SIPS); plain
    /// UDP is not. Used to gate SDES SRTP keying, which sends the media key inside the SDP and is
    /// therefore only meaningful when the signalling channel itself is encrypted.
    pub fn is_secure(&self) -> bool {
        matches!(self, Responder::Stream { .. })
    }
}

/// Largest single SIP message accepted on a stream transport, so a peer can't grow the reassembly
/// buffer without bound by dribbling a huge (or bogus) `Content-Length`.
#[cfg_attr(not(feature = "tls"), allow(dead_code))]
const MAX_STREAM_MESSAGE: usize = 64 * 1024;

/// Incremental `Content-Length` framer for stream transports (SIP over TCP/TLS, RFC 3261 §7.5).
///
/// Bytes are [`push`](Self::push)ed as they arrive; [`next_message`](Self::next_message) yields
/// each complete SIP message (head + body) once enough bytes are present, leaving any trailing
/// partial message buffered for the next read.
///
/// Exercised by a stream transport ([`super::tls`]); the tests below cover it regardless of build.
#[cfg_attr(not(feature = "tls"), allow(dead_code))]
#[derive(Default)]
pub struct StreamFramer {
    buf: Vec<u8>,
}

/// Outcome of pulling from the framer.
#[cfg_attr(not(feature = "tls"), allow(dead_code))]
pub enum Frame {
    /// A complete SIP message (its raw bytes, ready for [`message::parse`]).
    Message(Vec<u8>),
    /// No complete message yet — wait for more bytes.
    Incomplete,
    /// The stream is unframeable (a message exceeds [`MAX_STREAM_MESSAGE`]); the caller should
    /// close the connection.
    Overflow,
}

#[cfg_attr(not(feature = "tls"), allow(dead_code))]
impl StreamFramer {
    /// A new, empty framer.
    pub fn new() -> Self {
        StreamFramer::default()
    }

    /// Append freshly-read bytes to the reassembly buffer.
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Pull the next complete message from the buffer, if one is fully present.
    pub fn next_message(&mut self) -> Frame {
        // Skip any leading CRLF keep-alive pings a phone may send between messages (RFC 5626).
        while self.buf.first() == Some(&b'\r') || self.buf.first() == Some(&b'\n') {
            self.buf.remove(0);
        }
        let Some(sep) = find_subsequence(&self.buf, b"\r\n\r\n") else {
            // No complete header block yet; bound the pre-body buffer too.
            return if self.buf.len() > MAX_STREAM_MESSAGE { Frame::Overflow } else { Frame::Incomplete };
        };
        let head_end = sep + 4;
        // Parse just the head (with an empty body) to read the declared Content-Length.
        let content_length = match message::parse(&self.buf[..head_end]) {
            Ok(m) => m.content_length().unwrap_or(0),
            // A malformed head that nonetheless terminated: treat as a zero-length body so the
            // handler's own parse can reject it, rather than stalling the connection.
            Err(_) => 0,
        };
        // Guard the addition itself: a header-supplied Content-Length near `usize::MAX` would
        // otherwise wrap and slip a small `total` past the size check below.
        let total = match head_end.checked_add(content_length) {
            Some(t) if t <= MAX_STREAM_MESSAGE => t,
            _ => return Frame::Overflow,
        };
        if self.buf.len() < total {
            return Frame::Incomplete;
        }
        Frame::Message(self.buf.drain(..total).collect())
    }
}

/// The index of the first occurrence of `needle` in `haystack`, or `None`.
#[cfg_attr(not(feature = "tls"), allow(dead_code))]
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(body: &str) -> String {
        format!(
            "OPTIONS sip:commos SIP/2.0\r\nVia: SIP/2.0/TCP x;branch=z\r\nCSeq: 1 OPTIONS\r\n\
             Content-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
    }

    #[test]
    fn frames_a_single_message() {
        let mut f = StreamFramer::new();
        f.push(msg("").as_bytes());
        match f.next_message() {
            Frame::Message(m) => assert!(m.ends_with(b"\r\n\r\n")),
            _ => panic!("expected a complete message"),
        }
        assert!(matches!(f.next_message(), Frame::Incomplete));
    }

    #[test]
    fn reassembles_a_message_split_across_reads() {
        let whole = msg("hello");
        let (a, b) = whole.split_at(whole.len() - 3);
        let mut f = StreamFramer::new();
        f.push(a.as_bytes());
        assert!(matches!(f.next_message(), Frame::Incomplete), "body not fully arrived");
        f.push(b.as_bytes());
        match f.next_message() {
            Frame::Message(m) => assert!(m.ends_with(b"hello")),
            _ => panic!("expected the reassembled message"),
        }
    }

    #[test]
    fn splits_two_pipelined_messages() {
        let mut f = StreamFramer::new();
        f.push(format!("{}{}", msg("aa"), msg("bb")).as_bytes());
        assert!(matches!(f.next_message(), Frame::Message(m) if m.ends_with(b"aa")));
        assert!(matches!(f.next_message(), Frame::Message(m) if m.ends_with(b"bb")));
        assert!(matches!(f.next_message(), Frame::Incomplete));
    }

    #[test]
    fn skips_leading_keepalive_crlf() {
        let mut f = StreamFramer::new();
        f.push(b"\r\n\r\n"); // a CRLF keep-alive ping
        f.push(msg("").as_bytes());
        assert!(matches!(f.next_message(), Frame::Message(_)));
    }

    #[test]
    fn overflow_on_absurd_content_length() {
        let mut f = StreamFramer::new();
        f.push(b"OPTIONS sip:x SIP/2.0\r\nContent-Length: 999999999\r\n\r\n");
        assert!(matches!(f.next_message(), Frame::Overflow));
    }
}
