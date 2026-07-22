//! A minimal, dependency-free SMTP submission client.
//!
//! Like [`crate::control::webhook_delivery`], this speaks the wire protocol directly over a
//! [`tokio::net::TcpStream`] rather than pulling in an SMTP crate, honouring the workspace's
//! pure-Rust / clean-cross-compile mandate (no OpenSSL / native-tls). It reuses `base64`
//! (already a dependency) for `AUTH LOGIN` and MIME attachment encoding.
//!
//! ## Scope: plaintext submission to a trusted relay
//!
//! Only **plaintext** SMTP (typically port 25 or the 587 submission port to a relay on the
//! trusted network) is implemented, mirroring the `http://`-only webhook client and the
//! local/trusted-PostgreSQL posture. STARTTLS / implicit TLS to an external provider is a
//! documented add behind the existing `tls` feature (the repo already vendors a `ring`-based
//! rustls for SIPS). `AUTH LOGIN` is offered when credentials are supplied so a relay that
//! requires authentication on the LAN still works.
//!
//! This module is intentionally free of any dependency on the voicemail entities: it takes a
//! plain [`Email`] so any caller can drive it.

use std::time::Duration;

use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// How long any single SMTP command/response round-trip may take.
const TIMEOUT: Duration = Duration::from_secs(10);

/// SMTP transport configuration (resolved from `smtp:` config; the password SecretRef is
/// resolved to a plain value before this is built).
#[derive(Clone, Debug)]
pub struct SmtpTransport {
    pub host: String,
    pub port: u16,
    /// Envelope + `From:` sender address.
    pub from: String,
    /// The HELO/EHLO domain we announce. Defaults to the `from` domain if empty.
    pub helo: String,
    /// Optional `AUTH LOGIN` credentials (username, password).
    pub auth: Option<(String, String)>,
}

/// A file attached to an [`Email`].
#[derive(Clone, Debug)]
pub struct Attachment {
    pub filename: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// One outbound message.
#[derive(Clone, Debug)]
pub struct Email {
    pub to: Vec<String>,
    pub subject: String,
    pub text_body: String,
    pub attachment: Option<Attachment>,
}

#[derive(Debug, thiserror::Error)]
pub enum SmtpError {
    #[error("no recipients")]
    NoRecipients,
    #[error("failed to connect to SMTP relay: {0}")]
    Connect(String),
    #[error("SMTP I/O error: {0}")]
    Io(String),
    #[error("SMTP relay timed out")]
    Timeout,
    #[error("SMTP relay rejected {command}: {code} {message}")]
    Rejected { command: String, code: u16, message: String },
    #[error("malformed SMTP reply: {0}")]
    BadReply(String),
}

/// Send `email` through the relay described by `transport`.
///
/// Every step checks the reply code and fails fast on an unexpected one, so a
/// mis-configured relay surfaces as a precise [`SmtpError::Rejected`] naming the command
/// rather than a silent drop.
pub async fn send(transport: &SmtpTransport, email: &Email) -> Result<(), SmtpError> {
    if email.to.is_empty() {
        return Err(SmtpError::NoRecipients);
    }
    let addr = (transport.host.as_str(), transport.port);
    let stream = timeout(TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| SmtpError::Timeout)?
        .map_err(|e| SmtpError::Connect(e.to_string()))?;
    timeout(TIMEOUT, converse(stream, transport, email))
        .await
        .map_err(|_| SmtpError::Timeout)?
}

/// Drive the full SMTP conversation on an already-connected stream.
async fn converse(
    mut stream: TcpStream,
    t: &SmtpTransport,
    email: &Email,
) -> Result<(), SmtpError> {
    let mut buf = Vec::with_capacity(512);

    // Greeting.
    expect(&mut stream, &mut buf, 220, "greeting").await?;

    // EHLO — announce ourselves. We do not parse the capability list (we only optionally
    // AUTH), just require a 250.
    let helo = if t.helo.trim().is_empty() {
        domain_of(&t.from)
    } else {
        t.helo.clone()
    };
    command(&mut stream, &mut buf, &format!("EHLO {helo}"), 250, "EHLO").await?;

    // Optional AUTH LOGIN (base64 username then password).
    if let Some((user, pass)) = &t.auth {
        command(&mut stream, &mut buf, "AUTH LOGIN", 334, "AUTH LOGIN").await?;
        command(&mut stream, &mut buf, &b64(user.as_bytes()), 334, "AUTH username").await?;
        command(&mut stream, &mut buf, &b64(pass.as_bytes()), 235, "AUTH password").await?;
    }

    command(&mut stream, &mut buf, &format!("MAIL FROM:<{}>", t.from), 250, "MAIL FROM").await?;
    for rcpt in &email.to {
        command(&mut stream, &mut buf, &format!("RCPT TO:<{rcpt}>", ), 250, "RCPT TO").await?;
    }
    command(&mut stream, &mut buf, "DATA", 354, "DATA").await?;

    // The message body. Dot-stuffing: any line starting with '.' is escaped to '..' so the
    // terminating "\r\n.\r\n" is unambiguous (RFC 5321 §4.5.2).
    let message = build_message(t, email);
    let stuffed = dot_stuff(&message);
    write_all(&mut stream, stuffed.as_bytes()).await?;
    write_all(&mut stream, b"\r\n.\r\n").await?;
    expect(&mut stream, &mut buf, 250, "message body").await?;

    // Best-effort QUIT; the message is already accepted, so ignore a QUIT hiccup.
    let _ = command(&mut stream, &mut buf, "QUIT", 221, "QUIT").await;
    Ok(())
}

/// Write a command line (`\r\n`-terminated) and require a specific reply code.
async fn command(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    line: &str,
    want: u16,
    what: &str,
) -> Result<(), SmtpError> {
    // Write the command line and its CRLF as one buffer so a peer reading a chunk at a time
    // never sees a command split across the terminator.
    let mut framed = String::with_capacity(line.len() + 2);
    framed.push_str(line);
    framed.push_str("\r\n");
    write_all(stream, framed.as_bytes()).await?;
    expect(stream, buf, want, what).await
}

/// Read one (possibly multi-line) SMTP reply and require its code equals `want`.
async fn expect(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    want: u16,
    what: &str,
) -> Result<(), SmtpError> {
    let (code, message) = read_reply(stream, buf).await?;
    if code == want {
        Ok(())
    } else {
        Err(SmtpError::Rejected { command: what.to_string(), code, message })
    }
}

/// Read a full SMTP reply. Multi-line replies repeat the code with a `-` after it on every
/// line but the last, which uses a space: `250-first\r\n250 last\r\n`.
async fn read_reply(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Result<(u16, String), SmtpError> {
    buf.clear();
    let mut chunk = [0u8; 512];
    loop {
        // A complete reply ends with a line "<code><SP>...". Check whether what we have so far
        // already contains such a terminating line.
        if let Some(done) = try_parse_reply(buf) {
            return done;
        }
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| SmtpError::Io(e.to_string()))?;
        if n == 0 {
            // Peer closed; try one last parse, else it's malformed.
            return try_parse_reply(buf)
                .unwrap_or_else(|| Err(SmtpError::BadReply("connection closed mid-reply".into())));
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 65536 {
            return Err(SmtpError::BadReply("reply too long".into()));
        }
    }
}

/// If `buf` holds a complete SMTP reply, return the parsed `(code, final-line-text)`;
/// otherwise `None` (need more bytes).
///
/// A reply is complete once its **final** CRLF-terminated line has a space as the 4th
/// character (`"250 done"`); continuation lines carry a `-` there (`"250-more"`). Splitting on
/// `\r\n` yields the still-unterminated tail as the last element, so the complete lines are
/// everything before it — we inspect the last complete line.
fn try_parse_reply(buf: &[u8]) -> Option<Result<(u16, String), SmtpError>> {
    let text = String::from_utf8_lossy(buf);
    let parts: Vec<&str> = text.split("\r\n").collect();
    if parts.len() < 2 {
        return None; // no CRLF yet → no complete line
    }
    // The last element is the (possibly empty) unterminated tail; the one before it is the
    // last fully-received line.
    let last = *parts.get(parts.len() - 2)?;
    if last.len() < 4 {
        return None;
    }
    if last.as_bytes()[3] != b' ' {
        return None; // still inside a multi-line reply; wait for the terminating line
    }
    let code = match last[..3].parse::<u16>() {
        Ok(c) => c,
        Err(_) => return Some(Err(SmtpError::BadReply(format!("non-numeric code: {last:?}")))),
    };
    let msg = last.get(4..).unwrap_or("").trim().to_string();
    Some(Ok((code, msg)))
}

async fn write_all(stream: &mut TcpStream, bytes: &[u8]) -> Result<(), SmtpError> {
    stream.write_all(bytes).await.map_err(|e| SmtpError::Io(e.to_string()))?;
    stream.flush().await.map_err(|e| SmtpError::Io(e.to_string()))
}

/// Assemble the RFC 5322 message: headers then either a plain text body or a
/// `multipart/mixed` body when there is an attachment.
fn build_message(t: &SmtpTransport, email: &Email) -> String {
    let mut m = String::with_capacity(512);
    m.push_str(&format!("From: {}\r\n", t.from));
    m.push_str(&format!("To: {}\r\n", email.to.join(", ")));
    m.push_str(&format!("Subject: {}\r\n", header_encode(&email.subject)));
    m.push_str("MIME-Version: 1.0\r\n");

    match &email.attachment {
        None => {
            m.push_str("Content-Type: text/plain; charset=utf-8\r\n\r\n");
            m.push_str(&email.text_body);
            m.push_str("\r\n");
        }
        Some(att) => {
            // A fixed boundary is fine: the body is generated, not user-influenced, and it does
            // not appear in the base64 attachment or the text body.
            let boundary = "commos_vm_boundary_9f3a";
            m.push_str(&format!("Content-Type: multipart/mixed; boundary=\"{boundary}\"\r\n\r\n"));
            // Text part.
            m.push_str(&format!("--{boundary}\r\n"));
            m.push_str("Content-Type: text/plain; charset=utf-8\r\n\r\n");
            m.push_str(&email.text_body);
            m.push_str("\r\n");
            // Attachment part (base64, 76-char lines per MIME).
            m.push_str(&format!("--{boundary}\r\n"));
            m.push_str(&format!("Content-Type: {}\r\n", att.content_type));
            m.push_str("Content-Transfer-Encoding: base64\r\n");
            m.push_str(&format!(
                "Content-Disposition: attachment; filename=\"{}\"\r\n\r\n",
                att.filename
            ));
            m.push_str(&b64_wrapped(&att.bytes));
            m.push_str("\r\n");
            m.push_str(&format!("--{boundary}--\r\n"));
        }
    }
    m
}

/// Escape any line beginning with '.' by doubling it, so the message body cannot prematurely
/// terminate the DATA phase (RFC 5321 transparency).
fn dot_stuff(message: &str) -> String {
    message
        .split("\r\n")
        .map(|l| if l.starts_with('.') { format!(".{l}") } else { l.to_string() })
        .collect::<Vec<_>>()
        .join("\r\n")
}

/// Base64 without line wrapping (for AUTH tokens).
fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Base64 wrapped at 76 columns with CRLF (for MIME bodies).
fn b64_wrapped(bytes: &[u8]) -> String {
    let raw = base64::engine::general_purpose::STANDARD.encode(bytes);
    let mut out = String::with_capacity(raw.len() + raw.len() / 76 * 2);
    for (i, c) in raw.chars().enumerate() {
        if i > 0 && i % 76 == 0 {
            out.push_str("\r\n");
        }
        out.push(c);
    }
    out
}

/// Encode a header value that may contain non-ASCII using MIME "encoded-word" (RFC 2047), so
/// a caller name or subject with accents survives. Pure-ASCII values pass through unchanged.
fn header_encode(s: &str) -> String {
    if s.is_ascii() {
        s.to_string()
    } else {
        format!("=?utf-8?B?{}?=", b64(s.as_bytes()))
    }
}

/// The domain part of an email address (after `@`), or the whole string if there is no `@`.
fn domain_of(addr: &str) -> String {
    addr.rsplit('@').next().unwrap_or(addr).to_string()
}

/// Wrap raw 8 kHz mono G.711 μ-law samples (CommOS's storage codec) into a minimal WAV
/// container (`WAVE`, format tag 7 = μ-law) so the attachment plays directly in a mail
/// client, instead of shipping headerless bytes no player recognises.
pub fn wav_ulaw(ulaw: &[u8]) -> Vec<u8> {
    const SAMPLE_RATE: u32 = 8000;
    const CHANNELS: u16 = 1;
    const BITS: u16 = 8;
    const FMT_MULAW: u16 = 7;
    let data_len = ulaw.len() as u32;
    let byte_rate = SAMPLE_RATE * CHANNELS as u32 * (BITS as u32 / 8);
    let block_align = CHANNELS * (BITS / 8);
    // RIFF size = 4 ("WAVE") + (8 + 18 fmt) + (8 + data). fmt chunk is 18 bytes for non-PCM
    // (includes the cbSize=0 field).
    let riff_size = 4 + (8 + 18) + (8 + data_len);

    let mut w = Vec::with_capacity(ulaw.len() + 58);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&riff_size.to_le_bytes());
    w.extend_from_slice(b"WAVE");
    // fmt chunk.
    w.extend_from_slice(b"fmt ");
    w.extend_from_slice(&18u32.to_le_bytes());
    w.extend_from_slice(&FMT_MULAW.to_le_bytes());
    w.extend_from_slice(&CHANNELS.to_le_bytes());
    w.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    w.extend_from_slice(&byte_rate.to_le_bytes());
    w.extend_from_slice(&block_align.to_le_bytes());
    w.extend_from_slice(&BITS.to_le_bytes());
    w.extend_from_slice(&0u16.to_le_bytes()); // cbSize
    // data chunk.
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    w.extend_from_slice(ulaw);
    w
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn wav_header_is_wellformed_mulaw() {
        let w = wav_ulaw(&[0x7f, 0x80, 0xff, 0x00]);
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(&w[8..12], b"WAVE");
        assert_eq!(&w[12..16], b"fmt ");
        // Format tag (offset 20) = 7 (μ-law), little-endian.
        assert_eq!(u16::from_le_bytes([w[20], w[21]]), 7);
        // Sample rate (offset 24) = 8000.
        assert_eq!(u32::from_le_bytes([w[24], w[25], w[26], w[27]]), 8000);
        // data chunk present and length matches.
        assert_eq!(&w[38..42], b"data");
        assert_eq!(u32::from_le_bytes([w[42], w[43], w[44], w[45]]), 4);
        assert_eq!(&w[46..], &[0x7f, 0x80, 0xff, 0x00]);
    }

    #[test]
    fn dot_stuffing_escapes_leading_dots() {
        assert_eq!(dot_stuff("normal\r\n.hidden\r\nok"), "normal\r\n..hidden\r\nok");
        assert_eq!(dot_stuff("..already\r\n"), "...already\r\n");
    }

    #[test]
    fn message_with_attachment_is_multipart() {
        let t = SmtpTransport {
            host: "h".into(), port: 25, from: "vm@commos.local".into(), helo: String::new(), auth: None,
        };
        let email = Email {
            to: vec!["alice@example.com".into()],
            subject: "New voicemail".into(),
            text_body: "You have a new message.".into(),
            attachment: Some(Attachment {
                filename: "voicemail.wav".into(),
                content_type: "audio/wav".into(),
                bytes: vec![1, 2, 3],
            }),
        };
        let m = build_message(&t, &email);
        assert!(m.contains("Content-Type: multipart/mixed; boundary="));
        assert!(m.contains("Content-Disposition: attachment; filename=\"voicemail.wav\""));
        assert!(m.contains("Content-Transfer-Encoding: base64"));
        assert!(m.contains("From: vm@commos.local"));
        assert!(m.contains("To: alice@example.com"));
    }

    #[test]
    fn header_encode_passes_ascii_and_wraps_utf8() {
        assert_eq!(header_encode("New voicemail"), "New voicemail");
        assert!(header_encode("Café").starts_with("=?utf-8?B?"));
    }

    #[test]
    fn reply_parser_handles_multiline() {
        // Multi-line 250 reply: only the final "250 " line terminates.
        let buf = b"250-mail.example.com at your service\r\n250-SIZE 35882577\r\n250 AUTH LOGIN PLAIN\r\n";
        let (code, msg) = try_parse_reply(buf).unwrap().unwrap();
        assert_eq!(code, 250);
        assert_eq!(msg, "AUTH LOGIN PLAIN");
    }

    #[test]
    fn reply_parser_waits_for_incomplete_line() {
        // A partial line without CRLF is not yet a complete reply.
        assert!(try_parse_reply(b"220 mail.exa").is_none());
        // With the CRLF it parses.
        let (code, _) = try_parse_reply(b"220 mail.example.com ESMTP\r\n").unwrap().unwrap();
        assert_eq!(code, 220);
    }

    async fn say(sock: &mut TcpStream, line: &str) {
        sock.write_all(line.as_bytes()).await.unwrap();
        sock.flush().await.unwrap();
    }

    /// A tiny in-process SMTP server that walks the happy path and captures the DATA blob. It
    /// accumulates bytes and processes complete `\r\n`-terminated command lines, so it is
    /// robust to TCP segmentation (a command split across reads is still handled once).
    async fn fake_smtp(
        listener: TcpListener,
        capture: std::sync::Arc<tokio::sync::Mutex<String>>,
    ) {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut chunk = [0u8; 1024];
        say(&mut sock, "220 fake ESMTP\r\n").await;
        let mut in_data = false;
        let mut data = String::new();
        let mut acc = String::new();
        loop {
            let n = sock.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            acc.push_str(&String::from_utf8_lossy(&chunk[..n]));
            if in_data {
                data.push_str(&acc);
                acc.clear();
                if data.contains("\r\n.\r\n") {
                    in_data = false;
                    *capture.lock().await = data.clone();
                    say(&mut sock, "250 OK queued\r\n").await;
                }
                continue;
            }
            // Process every complete command line currently buffered.
            while let Some(idx) = acc.find("\r\n") {
                let line: String = acc.drain(..idx + 2).collect();
                let upper = line.trim_end().to_uppercase();
                if upper.starts_with("EHLO") {
                    say(&mut sock, "250-fake\r\n250 AUTH LOGIN\r\n").await;
                } else if upper.starts_with("MAIL FROM") || upper.starts_with("RCPT TO") {
                    say(&mut sock, "250 OK\r\n").await;
                } else if upper == "DATA" {
                    say(&mut sock, "354 End data with <CR><LF>.<CR><LF>\r\n").await;
                    in_data = true;
                    // Any bytes after DATA belong to the message body.
                    data.push_str(&acc);
                    acc.clear();
                    break;
                } else if upper == "QUIT" {
                    say(&mut sock, "221 Bye\r\n").await;
                    return;
                } else {
                    say(&mut sock, "250 OK\r\n").await;
                }
            }
        }
    }

    #[tokio::test]
    async fn full_send_reaches_relay_and_delivers_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let capture = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let server = tokio::spawn(fake_smtp(listener, capture.clone()));

        let transport = SmtpTransport {
            host: addr.ip().to_string(),
            port: addr.port(),
            from: "voicemail@commos.local".into(),
            helo: "commos.local".into(),
            auth: None,
        };
        let email = Email {
            to: vec!["alice@example.com".into()],
            subject: "New voicemail from 5551234".into(),
            text_body: "A caller left you a 12s message.".into(),
            attachment: Some(Attachment {
                filename: "voicemail.wav".into(),
                content_type: "audio/wav".into(),
                bytes: wav_ulaw(&[0x7f; 160]),
            }),
        };
        send(&transport, &email).await.expect("send succeeds");
        server.await.unwrap();

        let body = capture.lock().await.clone();
        assert!(body.contains("Subject: New voicemail from 5551234"));
        assert!(body.contains("multipart/mixed"));
        assert!(body.contains("filename=\"voicemail.wav\""));
    }

    #[tokio::test]
    async fn rejected_command_surfaces_error() {
        // A server that greets then rejects MAIL FROM with 550.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut chunk = [0u8; 512];
            sock.write_all(b"220 fake\r\n").await.unwrap();
            loop {
                let n = sock.read(&mut chunk).await.unwrap();
                if n == 0 { break; }
                let up = String::from_utf8_lossy(&chunk[..n]).to_uppercase();
                if up.starts_with("EHLO") {
                    sock.write_all(b"250 fake\r\n").await.unwrap();
                } else if up.starts_with("MAIL FROM") {
                    sock.write_all(b"550 no relaying\r\n").await.unwrap();
                    break;
                } else {
                    sock.write_all(b"250 OK\r\n").await.unwrap();
                }
            }
        });
        let transport = SmtpTransport {
            host: addr.ip().to_string(), port: addr.port(),
            from: "vm@commos.local".into(), helo: "commos.local".into(), auth: None,
        };
        let email = Email { to: vec!["a@b.c".into()], subject: "x".into(), text_body: "y".into(), attachment: None };
        let err = send(&transport, &email).await.expect_err("must reject");
        assert!(matches!(err, SmtpError::Rejected { code: 550, .. }), "{err:?}");
        let _ = server.await;
    }

    #[tokio::test]
    async fn no_recipients_is_rejected_before_connecting() {
        let transport = SmtpTransport {
            host: "127.0.0.1".into(), port: 1, from: "vm@commos.local".into(), helo: String::new(), auth: None,
        };
        let email = Email { to: vec![], subject: "x".into(), text_body: "y".into(), attachment: None };
        assert!(matches!(send(&transport, &email).await, Err(SmtpError::NoRecipients)));
    }
}
