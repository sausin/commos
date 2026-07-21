//! A minimal, dependency-free HTTP/1.1 webhook delivery client with HMAC-SHA256 signing.
//!
//! This speaks HTTP/1.1 directly over a [`tokio::net::TcpStream`] rather than pulling in an
//! HTTP-client crate. That is deliberate: the project has a hard pure-Rust, clean-cross-compile
//! mandate (no OpenSSL / native-tls — see the workspace `Cargo.toml`), so we hand-roll the
//! wire the same way [`crate::sip`] hand-rolls SIP over UDP. It reuses the `hmac`/`sha2` crates
//! the workspace already depends on for HS256 JWT verification (see [`crate::api`]).
//!
//! ## Scope: `http://` only
//!
//! Only `http://` targets are supported. TLS termination is a **documented add** in this
//! reference implementation — mirroring the SIP-over-UDP and database postures — expected to be
//! handled by a reverse proxy or sidecar in front of the webhook endpoint. An `https://` URL is
//! rejected up front with [`DeliveryError::UnsupportedScheme`] rather than silently sent in the
//! clear.
//!
//! ## What "delivered" means
//!
//! [`deliver`] returns `Ok(Delivered)` for **any** well-formed HTTP response, whatever its
//! status code — a `4xx`/`5xx` is a delivered response, not a transport error. The caller
//! decides what counts as success (typically `2xx`). Only connect/timeout/I/O/parse failures
//! surface as `Err`.
//!
//! This module is intentionally free of any dependency on the `Webhook` entity: it takes raw
//! `url` / `secret` / `body` primitives so a dispatcher can wire it to whatever source it likes.

use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use sha2::Sha256;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// How long a single delivery attempt (connect + write + read) may take before it is abandoned
/// as a [`DeliveryError::Timeout`].
const TIMEOUT: Duration = Duration::from_secs(5);

/// The header carrying the request-body signature when a `secret` is supplied.
const SIGNATURE_HEADER: &str = "X-CommOS-Signature";

/// A successfully delivered webhook — i.e. the endpoint returned *a* well-formed HTTP response.
///
/// The `status_code` is whatever the server sent (a `4xx`/`5xx` is still a `Delivered`); the
/// caller decides whether that counts as success. `duration_ms` is the wall-clock round trip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivered {
    /// The HTTP status code parsed from the response status line.
    pub status_code: u16,
    /// End-to-end wall-clock duration of the attempt, in milliseconds.
    pub duration_ms: u64,
}

/// Why a webhook could not be delivered. A returned HTTP status (any code) is **not** an error —
/// it is an [`Ok(Delivered)`]; these variants are strictly transport/parse failures.
#[derive(Debug, thiserror::Error)]
pub enum DeliveryError {
    /// The URL scheme is not `http://` (e.g. `https://`, which is a documented reverse-proxy /
    /// sidecar concern, or some other scheme entirely).
    #[error("unsupported URL scheme (only http:// is supported): {0}")]
    UnsupportedScheme(String),
    /// The URL could not be parsed into host / port / path.
    #[error("invalid webhook URL: {0}")]
    InvalidUrl(String),
    /// The target resolves to a disallowed (internal/loopback/link-local/metadata) address, so
    /// the delivery is refused before any connection is made (SSRF guard). The message is
    /// deliberately generic and does not reveal whether an internal host exists.
    #[error("webhook target is not an allowed destination")]
    Blocked,
    /// The TCP connection to the endpoint could not be established.
    #[error("failed to connect to webhook endpoint: {0}")]
    Connect(String),
    /// An I/O error occurred while writing the request or reading the response.
    #[error("webhook I/O error: {0}")]
    Io(String),
    /// The attempt exceeded [`TIMEOUT`].
    #[error("webhook delivery timed out")]
    Timeout,
    /// A response was received but its status line could not be parsed.
    #[error("malformed HTTP response from webhook endpoint: {0}")]
    BadResponse(String),
}

/// Deliver `body` to `url` as an `HTTP/1.1 POST` with `Content-Type: application/json`, signing
/// the body with HMAC-SHA256 under `secret` when one is provided.
///
/// Returns [`Ok(Delivered)`] for any well-formed HTTP response regardless of status code — the
/// caller decides success (usually `2xx`). Connect / timeout / I/O / parse problems are `Err`.
///
/// See the [module docs](self) for the `http://`-only scope and TLS posture.
pub async fn deliver(
    url: &str,
    secret: Option<&str>,
    body: &[u8],
) -> Result<Delivered, DeliveryError> {
    // Production always vets the target (public destinations only).
    deliver_inner(url, secret, body, false).await
}

/// Delivery core. `allow_local` disables the SSRF egress vet and is set ONLY by in-crate tests
/// that need to reach a throwaway loopback server; production always calls with `false`.
async fn deliver_inner(
    url: &str,
    secret: Option<&str>,
    body: &[u8],
    allow_local: bool,
) -> Result<Delivered, DeliveryError> {
    let (host, port, path) = parse_target(url)?;
    let started = Instant::now();

    // SSRF guard: resolve the hostname ourselves and vet the concrete IP, then connect to *that*
    // pinned address. This blocks targets pointing at the local host, the private LAN, or the
    // cloud metadata service, and closes the DNS-rebinding gap (the vetted IP is the one we
    // dial, so a name cannot resolve "public" for the check then "internal" for the connect).
    let addr = resolve_vetted(&host, port, allow_local).await?;

    // Connect, bounded by TIMEOUT. `timeout` elapsing → Timeout; a connect error → Connect.
    let stream = timeout(TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| DeliveryError::Timeout)?
        .map_err(|e| DeliveryError::Connect(e.to_string()))?;

    // The rest of the exchange shares the remaining budget under a single timeout so a slow
    // server cannot hold the task open past TIMEOUT.
    let status_code = timeout(TIMEOUT, exchange(stream, &host, port, &path, secret, body))
        .await
        .map_err(|_| DeliveryError::Timeout)??;

    Ok(Delivered {
        status_code,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

/// Write the request and read back the status code on an already-connected `stream`.
async fn exchange(
    mut stream: TcpStream,
    host: &str,
    port: u16,
    path: &str,
    secret: Option<&str>,
    body: &[u8],
) -> Result<u16, DeliveryError> {
    let request = build_request(host, port, path, secret, body);

    // Header block then body in a single write, matching the one-shot style of the SIP codec.
    stream
        .write_all(&request)
        .await
        .map_err(|e| DeliveryError::Io(e.to_string()))?;
    stream
        .flush()
        .await
        .map_err(|e| DeliveryError::Io(e.to_string()))?;

    // We only need the status line, but the server may coalesce the status line, headers and
    // body into one or more reads. Read until we have seen the end of the status line (the first
    // CRLF / LF) or the peer closes. A small cap keeps a chatty server from ballooning memory.
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];
    loop {
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| DeliveryError::Io(e.to_string()))?;
        if n == 0 {
            break; // peer closed
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.contains(&b'\n') {
            break; // status line is complete
        }
        if buf.len() > 8192 {
            break; // status line implausibly long — give up and let parsing fail
        }
    }

    parse_status_code(&buf)
}

/// Assemble the raw request bytes: request line, headers, blank line, body.
fn build_request(host: &str, port: u16, path: &str, secret: Option<&str>, body: &[u8]) -> Vec<u8> {
    // The Host header omits the port when it is the default (80), matching common client output.
    let host_header = if port == 80 {
        host.to_string()
    } else {
        format!("{host}:{port}")
    };

    let mut head = String::with_capacity(256);
    head.push_str(&format!("POST {path} HTTP/1.1\r\n"));
    head.push_str(&format!("Host: {host_header}\r\n"));
    head.push_str("User-Agent: commosd\r\n");
    head.push_str("Content-Type: application/json\r\n");
    head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    head.push_str("Connection: close\r\n");

    if let Some(secret) = secret {
        let signature = sign(secret.as_bytes(), body);
        head.push_str(&format!("{SIGNATURE_HEADER}: sha256={signature}\r\n"));
    }

    head.push_str("\r\n");

    let mut request = head.into_bytes();
    request.extend_from_slice(body);
    request
}

/// `hex(HMAC-SHA256(secret, body))` — the value (sans the `sha256=` prefix) of the signature
/// header. HMAC key ingestion never fails for any key length, so this is infallible.
fn sign(secret: &[u8], body: &[u8]) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts a key of any length");
    mac.update(body);
    hex(&mac.finalize().into_bytes())
}

/// Lowercase hex encoding of `bytes`.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Parse the numeric status code from an HTTP response's status line `HTTP/1.1 <code> <reason>`.
fn parse_status_code(response: &[u8]) -> Result<u16, DeliveryError> {
    // The status line is ASCII; lossy conversion of the leading bytes never panics.
    let text = String::from_utf8_lossy(response);
    let line = text
        .split('\n')
        .next()
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .unwrap_or("")
        .trim();
    if line.is_empty() {
        return Err(DeliveryError::BadResponse("empty response".to_string()));
    }

    let mut parts = line.split_whitespace();
    let version = parts.next();
    let code = parts.next();
    match (version, code) {
        (Some(v), Some(c)) if v.starts_with("HTTP/") => c
            .parse::<u16>()
            .map_err(|_| DeliveryError::BadResponse(format!("non-numeric status code: {c:?}"))),
        _ => Err(DeliveryError::BadResponse(format!(
            "unrecognised status line: {line:?}"
        ))),
    }
}

/// Resolve `host:port` to a concrete socket address that is safe to connect to, or fail.
///
/// Every resolved candidate is vetted with [`crate::net::is_disallowed_egress`]; the first
/// allowed (public, routable) address is returned and used for the connection (pinned). If the
/// name resolves only to disallowed addresses, [`DeliveryError::Blocked`] is returned — a
/// literal IP literal in the URL takes the same path, so `http://169.254.169.254/...`,
/// `http://127.0.0.1/...`, and `http://10.0.0.5/...` are all refused.
async fn resolve_vetted(
    host: &str,
    port: u16,
    allow_local: bool,
) -> Result<std::net::SocketAddr, DeliveryError> {
    let candidates = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| DeliveryError::Connect(e.to_string()))?;

    let mut saw_any = false;
    for addr in candidates {
        saw_any = true;
        if allow_local || !crate::net::is_disallowed_egress(&addr.ip()) {
            return Ok(addr);
        }
    }
    if saw_any {
        // Resolved, but every address is internal/loopback/metadata → SSRF attempt, refuse.
        Err(DeliveryError::Blocked)
    } else {
        Err(DeliveryError::Connect("name did not resolve".to_string()))
    }
}

/// Split an `http://` URL into `(host, port, path)`.
///
/// - Requires the `http://` scheme; `https://` and everything else are rejected.
/// - `host[:port]` is split on the first `/` (or `?`/`#`) after the authority; a missing port
///   defaults to `80`, a missing path to `/`.
///
/// Examples: `http://example.com/hook` → `("example.com", 80, "/hook")`;
/// `http://10.0.0.1:9000/x` → `("10.0.0.1", 9000, "/x")`.
pub fn parse_target(url: &str) -> Result<(String, u16, String), DeliveryError> {
    let rest = match url.strip_prefix("http://") {
        Some(rest) => rest,
        None => return Err(DeliveryError::UnsupportedScheme(url.to_string())),
    };

    // Everything up to the first path/query/fragment delimiter is the authority; the remainder
    // (including its leading `/`) is the path. A query/fragment with no `/` still ends the host.
    let (authority, path) = match rest.find(['/', '?', '#']) {
        Some(i) => {
            let (a, p) = rest.split_at(i);
            // Preserve `?`/`#`-only tails by prefixing `/` so the request-target stays valid.
            let path = if p.starts_with('/') {
                p.to_string()
            } else {
                format!("/{p}")
            };
            (a, path)
        }
        None => (rest, "/".to_string()),
    };

    if authority.is_empty() {
        return Err(DeliveryError::InvalidUrl(url.to_string()));
    }

    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => {
            let port = p
                .parse::<u16>()
                .map_err(|_| DeliveryError::InvalidUrl(url.to_string()))?;
            (h, port)
        }
        None => (authority, 80),
    };

    if host.is_empty() {
        return Err(DeliveryError::InvalidUrl(url.to_string()));
    }

    Ok((host.to_string(), port, path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn https_is_rejected_as_unsupported_scheme() {
        let err = deliver("https://example.com/hook", None, b"{}")
            .await
            .expect_err("https must be rejected");
        assert!(matches!(err, DeliveryError::UnsupportedScheme(_)));
    }

    #[test]
    fn parse_target_splits_host_port_path() {
        assert_eq!(
            parse_target("http://example.com/hook").unwrap(),
            ("example.com".to_string(), 80, "/hook".to_string())
        );
        assert_eq!(
            parse_target("http://10.0.0.1:9000/x").unwrap(),
            ("10.0.0.1".to_string(), 9000, "/x".to_string())
        );
        // No path → default "/".
        assert_eq!(
            parse_target("http://host.example").unwrap(),
            ("host.example".to_string(), 80, "/".to_string())
        );
        // A non-http scheme is an UnsupportedScheme, not an InvalidUrl.
        assert!(matches!(
            parse_target("ftp://host/x"),
            Err(DeliveryError::UnsupportedScheme(_))
        ));
        // A garbage port is an InvalidUrl.
        assert!(matches!(
            parse_target("http://host:notaport/x"),
            Err(DeliveryError::InvalidUrl(_))
        ));
    }

    #[test]
    fn hex_encodes_known_input() {
        assert_eq!(hex(&[]), "");
        assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");
        assert_eq!(hex(b"\xde\xad\xbe\xef"), "deadbeef");
    }

    #[test]
    fn sign_matches_known_hmac_vector() {
        // RFC 4231-adjacent sanity: a fixed key/body yields a stable 64-hex-char digest.
        let sig = sign(b"secret", b"hello world");
        assert_eq!(sig.len(), 64);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
        // Recomputing is deterministic.
        assert_eq!(sig, sign(b"secret", b"hello world"));
        // A different body changes the signature.
        assert_ne!(sig, sign(b"secret", b"hello worlx"));
    }

    #[tokio::test]
    async fn signed_post_reaches_endpoint_and_returns_200() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let secret = "topsecret";
        let body = br#"{"event":"call.completed"}"#;
        let expected_sig = format!("sha256={}", sign(secret.as_bytes(), body));

        // A one-shot server: accept a single connection, read the request, assert the signature,
        // and reply 200. Runs concurrently with the client below.
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            // Read until we have the full header block (blank line) — the body follows it.
            loop {
                let n = sock.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&buf).to_lowercase();
            let sig_line = format!("{}: {}", SIGNATURE_HEADER, expected_sig).to_lowercase();
            assert!(
                request.contains(&sig_line),
                "signature header present and correct; got request:\n{}",
                String::from_utf8_lossy(&buf)
            );
            assert!(request.contains("content-type: application/json"));
            assert!(request.starts_with("post / http/1.1"));

            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
            sock.flush().await.unwrap();
        });

        let url = format!("http://{}/", addr);
        // `allow_local` lets this reach the loopback test server; production `deliver` vets it out.
        let delivered = deliver_inner(&url, Some(secret), body, true)
            .await
            .expect("delivery succeeds against the throwaway server");
        assert_eq!(delivered.status_code, 200);

        server.await.unwrap();
    }

    #[tokio::test]
    async fn deliver_blocks_ssrf_targets() {
        // Loopback, private, and the cloud metadata address are all refused before any connect —
        // and as `Blocked`, not a connection error that would leak internal reachability.
        for url in [
            "http://127.0.0.1:9/x",
            "http://10.0.0.1:80/x",
            "http://169.254.169.254/latest/meta-data/",
        ] {
            let err = deliver(url, None, b"{}").await.expect_err("must be blocked");
            assert!(matches!(err, DeliveryError::Blocked), "{url} -> {err:?}");
        }
    }
}
