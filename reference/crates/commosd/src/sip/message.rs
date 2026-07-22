//! A small, **pure** SIP codec — the subset of RFC 3261 needed to terminate a real
//! softphone's signalling (Volume 7). It parses requests and responses, exposes the
//! headers the ingress cares about, and builds well-formed responses.
//!
//! This module has no I/O and no async: it turns bytes into a [`SipMessage`] and a
//! [`SipMessage`] plus a status into a response string. That keeps it exhaustively
//! unit-testable (see the tests at the bottom) while [`super::server`] owns the socket.
//!
//! Deliberately a *subset*: enough to REGISTER, answer OPTIONS, and frame INVITE/BYE.
//! It tolerates the real-world sloppiness phones emit — `\n`-only line endings,
//! arbitrary header-name casing, the RFC 3261 §7.3.3 compact header forms, and folded
//! (continuation) header lines.

use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};

/// The default registration lifetime when a REGISTER carries no `Expires` header and no
/// `expires=` contact parameter (RFC 3261 §10.2 / §20.19 recommend one hour).
pub const DEFAULT_EXPIRES: u64 = 3600;

/// A parsed SIP message — either a request (method + request-URI) or a response
/// (status + reason). Headers are kept in wire order with their raw names so nothing is
/// lost; typed accessors ([`Self::call_id`], [`Self::contact`], …) layer case-insensitive,
/// compact-form-aware lookup on top.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SipMessage {
    start_line: StartLine,
    /// `(raw-name, unfolded-value)` pairs, in the order received.
    headers: Vec<(String, String)>,
    /// Message body (e.g. SDP). Empty for the signalling this ingress handles today.
    body: Vec<u8>,
}

/// Request start line vs. response status line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartLine {
    /// e.g. `REGISTER sip:example.com SIP/2.0`.
    Request { method: String, uri: String },
    /// e.g. `SIP/2.0 200 OK`.
    Response { status: u16, reason: String },
}

/// Why a datagram could not be parsed as SIP. The server logs these at debug and drops the
/// datagram — a malformed packet must never take down the loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SipParseError {
    /// Nothing but whitespace.
    Empty,
    /// The first line is neither a valid request line nor a `SIP/2.0` status line.
    MalformedStartLine(String),
    /// A response status code that is not three digits.
    BadStatusCode(String),
}

impl fmt::Display for SipParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SipParseError::Empty => write!(f, "empty SIP message"),
            SipParseError::MalformedStartLine(l) => write!(f, "malformed SIP start line: {l:?}"),
            SipParseError::BadStatusCode(s) => write!(f, "malformed SIP status code: {s:?}"),
        }
    }
}

impl std::error::Error for SipParseError {}

/// Parse a SIP request or response from a datagram.
///
/// Handles CRLF and bare-LF line endings, header-name case-insensitivity, the compact
/// header forms (`v`/`f`/`t`/`i`/`m`/`l`/…), and line folding (a header value continued on
/// a following line that begins with whitespace).
pub fn parse(bytes: &[u8]) -> Result<SipMessage, SipParseError> {
    let (head, body) = split_head_body(bytes);
    // The header section is always ASCII/UTF-8 text; lossy is safe and never panics.
    let head = String::from_utf8_lossy(head);

    // Split into physical lines, tolerating `\n`-only endings by trimming a trailing `\r`.
    let mut raw_lines = head.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));

    let start = loop {
        match raw_lines.next() {
            Some(l) if l.trim().is_empty() => continue, // skip leading blank lines
            Some(l) => break l,
            None => return Err(SipParseError::Empty),
        }
    };
    let start_line = parse_start_line(start)?;

    // Unfold headers: a line starting with SP/HTAB continues the previous header value.
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in raw_lines {
        if line.is_empty() {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(last) = headers.last_mut() {
                last.1.push(' ');
                last.1.push_str(line.trim());
                continue;
            }
            // Continuation with no preceding header — ignore rather than fail.
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
        // A header line with no colon is malformed; skip it (be liberal in what we accept).
    }

    Ok(SipMessage {
        start_line,
        headers,
        body: body.to_vec(),
    })
}

fn parse_start_line(line: &str) -> Result<StartLine, SipParseError> {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix("SIP/2.0") {
        // Status line: `SIP/2.0 <status> <reason>`.
        let rest = rest.trim_start();
        let (code, reason) = match rest.split_once(char::is_whitespace) {
            Some((c, r)) => (c, r.trim()),
            None => (rest, ""),
        };
        let status: u16 = code
            .parse()
            .map_err(|_| SipParseError::BadStatusCode(code.to_string()))?;
        if !(100..=699).contains(&status) {
            return Err(SipParseError::BadStatusCode(code.to_string()));
        }
        return Ok(StartLine::Response {
            status,
            reason: reason.to_string(),
        });
    }

    // Request line: `METHOD SP Request-URI SP SIP/2.0`.
    let mut parts = line.split_whitespace();
    let method = parts.next();
    let uri = parts.next();
    let version = parts.next();
    match (method, uri, version) {
        (Some(m), Some(u), Some(v)) if v.eq_ignore_ascii_case("SIP/2.0") => Ok(StartLine::Request {
            method: m.to_ascii_uppercase(),
            uri: u.to_string(),
        }),
        _ => Err(SipParseError::MalformedStartLine(line.to_string())),
    }
}

/// Find the header/body boundary (`\r\n\r\n`, or bare `\n\n`), returning `(head, body)`.
fn split_head_body(bytes: &[u8]) -> (&[u8], &[u8]) {
    if let Some(i) = find_subslice(bytes, b"\r\n\r\n") {
        return (&bytes[..i], &bytes[i + 4..]);
    }
    if let Some(i) = find_subslice(bytes, b"\n\n") {
        return (&bytes[..i], &bytes[i + 2..]);
    }
    (bytes, &[])
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

// The codec exposes a complete accessor surface (used by the unit tests and by the
// forthcoming INVITE→Call handling); not every accessor is called by the current
// REGISTER-focused server yet.
#[allow(dead_code)]
impl SipMessage {
    // --- start-line accessors --------------------------------------------------------

    /// The request method (uppercased), or `None` for a response.
    pub fn method(&self) -> Option<&str> {
        match &self.start_line {
            StartLine::Request { method, .. } => Some(method),
            StartLine::Response { .. } => None,
        }
    }

    /// The request-URI, or `None` for a response.
    pub fn request_uri(&self) -> Option<&str> {
        match &self.start_line {
            StartLine::Request { uri, .. } => Some(uri),
            StartLine::Response { .. } => None,
        }
    }

    /// The response status code, or `None` for a request.
    pub fn status(&self) -> Option<u16> {
        match &self.start_line {
            StartLine::Response { status, .. } => Some(*status),
            StartLine::Request { .. } => None,
        }
    }

    /// True if this is a request.
    pub fn is_request(&self) -> bool {
        matches!(self.start_line, StartLine::Request { .. })
    }

    /// The parsed start line.
    pub fn start_line(&self) -> &StartLine {
        &self.start_line
    }

    /// The message body (e.g. SDP), if any.
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    // --- raw header access -----------------------------------------------------------

    /// All `(raw-name, value)` header pairs, in wire order.
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// First value for a header, matched case-insensitively and across compact forms
    /// (`header("Via")` also matches a `v:` line).
    pub fn header(&self, name: &str) -> Option<&str> {
        let want = canonical_header(name);
        self.headers
            .iter()
            .find(|(n, _)| canonical_header(n) == want)
            .map(|(_, v)| v.as_str())
    }

    /// Every value for a (possibly repeated) header, in wire order — e.g. the Via stack.
    pub fn header_all(&self, name: &str) -> Vec<&str> {
        let want = canonical_header(name);
        self.headers
            .iter()
            .filter(|(n, _)| canonical_header(n) == want)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    // --- typed convenience accessors -------------------------------------------------

    pub fn call_id(&self) -> Option<&str> {
        self.header("Call-ID")
    }

    pub fn cseq(&self) -> Option<&str> {
        self.header("CSeq")
    }

    /// The numeric sequence and method from `CSeq: <num> <METHOD>`.
    pub fn cseq_parts(&self) -> Option<(u32, &str)> {
        let raw = self.cseq()?.trim();
        let (num, method) = raw.split_once(char::is_whitespace)?;
        Some((num.trim().parse().ok()?, method.trim()))
    }

    pub fn user_agent(&self) -> Option<&str> {
        self.header("User-Agent")
    }

    pub fn max_forwards(&self) -> Option<u32> {
        self.header("Max-Forwards")?.trim().parse().ok()
    }

    pub fn content_length(&self) -> Option<usize> {
        self.header("Content-Length")?.trim().parse().ok()
    }

    /// The `branch` parameter of the topmost Via (the transaction identifier).
    pub fn via_branch(&self) -> Option<String> {
        find_param(self.header("Via")?, "branch")
    }

    /// The `tag` parameter of the From header (the originating dialog half-id).
    // `from_` here names the SIP *From* header (peer of `to_tag`), not a type conversion.
    #[allow(clippy::wrong_self_convention)]
    pub fn from_tag(&self) -> Option<String> {
        find_param(self.header("From")?, "tag")
    }

    /// The `tag` parameter of the To header (the terminating dialog half-id).
    pub fn to_tag(&self) -> Option<String> {
        find_param(self.header("To")?, "tag")
    }

    /// Address-of-record from the To header (`"Bob" <sip:100@host>;tag=x` → `sip:100@host`).
    pub fn to_aor(&self) -> Option<String> {
        self.header("To").map(uri_aor)
    }

    /// Address-of-record from the From header.
    // `from_` here names the SIP *From* header (peer of `to_aor`), not a type conversion.
    #[allow(clippy::wrong_self_convention)]
    pub fn from_aor(&self) -> Option<String> {
        self.header("From").map(uri_aor)
    }

    /// The best AoR to register: the To URI (the address being registered) if present,
    /// otherwise the From URI. RFC 3261 §10.2 puts the AoR in To; phones set From = To.
    pub fn register_aor(&self) -> Option<String> {
        self.to_aor().or_else(|| self.from_aor())
    }

    /// The Contact URI (the reachable location), stripped of angle brackets and header
    /// parameters (`<sip:100@1.2.3.4:5060>;expires=300` → `sip:100@1.2.3.4:5060`). `None`
    /// when there is no Contact, or the wildcard `*` (a de-register-all).
    pub fn contact_uri(&self) -> Option<String> {
        let raw = self.header("Contact")?.trim();
        if raw == "*" {
            return None;
        }
        Some(addr_spec(raw).to_string())
    }

    /// Effective registration lifetime in seconds: the `Expires` header, else the Contact
    /// `expires=` parameter, else [`DEFAULT_EXPIRES`]. A `0` means de-register.
    pub fn expires(&self) -> u64 {
        if let Some(v) = self.header("Expires") {
            if let Ok(n) = v.trim().parse::<u64>() {
                return n;
            }
        }
        if let Some(c) = self.header("Contact") {
            if let Some(v) = find_param(c, "expires") {
                if let Ok(n) = v.parse::<u64>() {
                    return n;
                }
            }
        }
        DEFAULT_EXPIRES
    }
}

/// Build an RFC 3261 response for `request`: `status`/`reason` on the status line, the
/// mandatory Via stack, From, To, Call-ID and CSeq echoed verbatim, a generated To `tag`
/// added on a 2xx if the request had none, plus `Server: commosd` and `Content-Length: 0`.
pub fn response(request: &SipMessage, status: u16, reason: &str) -> String {
    build_response(request, status, reason, &[])
}

/// Like [`response`], but appends `extra` headers (e.g. `Contact` and `Expires` echoed on a
/// REGISTER 200) before the terminating blank line.
pub fn response_with(
    request: &SipMessage,
    status: u16,
    reason: &str,
    extra: &[(&str, String)],
) -> String {
    build_response(request, status, reason, extra)
}

/// Build a syntactically valid outbound SIP **request** (the UAC side — e.g. the outbound
/// INVITE a B2BUA sends to a registered callee).
///
/// Emits the request line (`METHOD request-uri SIP/2.0`) followed by the caller-supplied
/// `headers` verbatim, then fills in any of the mandatory framing headers the caller did NOT
/// provide so the result is always well-formed: a `Via` (with a generated `branch`), `From`
/// (with a generated `tag`), `To`, `Call-ID`, `CSeq`, `Max-Forwards: 70`, and a
/// `Content-Length` matching `body`. When `body` is `Some((content_type, text))` a
/// `Content-Type` header and the body are appended.
///
/// This is deliberately minimal but RFC 3261-shaped: it round-trips through [`parse`]. The
/// caller owns dialog correctness (matching `From`/`To` tags, `Call-ID`, `CSeq` numbering);
/// this function only guarantees a parseable message with the required headers present.
pub fn request(
    method: &str,
    request_uri: &str,
    headers: &[(&str, String)],
    body: Option<(&str, &str)>,
) -> String {
    let method = method.trim().to_ascii_uppercase();
    let mut out = String::with_capacity(256);
    out.push_str(&format!("{method} {request_uri} SIP/2.0\r\n"));

    // Track which mandatory headers the caller already supplied (case-insensitive).
    let has = |name: &str| {
        let want = canonical_header(name);
        headers.iter().any(|(n, _)| canonical_header(n) == want)
    };
    let has_via = has("Via");
    let has_from = has("From");
    let has_to = has("To");
    let has_call_id = has("Call-ID");
    let has_cseq = has("CSeq");
    let has_max_forwards = has("Max-Forwards");
    let has_content_type = has("Content-Type");
    let has_content_length = has("Content-Length");

    // Caller-supplied headers first, verbatim.
    for (name, value) in headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }

    if !has_via {
        // A magic-cookie branch (RFC 3261 §8.1.1.7) so the response routes back. CSPRNG-random
        // (not derived from public inputs) so an off-path attacker cannot predict the branch and
        // forge a matching response for a locally-originated request.
        out.push_str(&format!(
            "Via: SIP/2.0/UDP commos.invalid;branch=z9hG4bK{}\r\n",
            rand_tag()
        ));
    }
    if !has_from {
        out.push_str(&format!(
            "From: <sip:commos@commos.invalid>;tag={}\r\n",
            rand_tag()
        ));
    }
    if !has_to {
        out.push_str(&format!("To: <{request_uri}>\r\n"));
    }
    if !has_call_id {
        out.push_str(&format!("Call-ID: {}@commos.invalid\r\n", rand_tag()));
    }
    if !has_cseq {
        out.push_str(&format!("CSeq: 1 {method}\r\n"));
    }
    if !has_max_forwards {
        out.push_str("Max-Forwards: 70\r\n");
    }

    match body {
        Some((content_type, text)) => {
            if !has_content_type {
                out.push_str(&format!("Content-Type: {content_type}\r\n"));
            }
            if !has_content_length {
                out.push_str(&format!("Content-Length: {}\r\n", text.len()));
            }
            out.push_str("\r\n");
            out.push_str(text);
        }
        None => {
            if !has_content_length {
                out.push_str("Content-Length: 0\r\n");
            }
            out.push_str("\r\n");
        }
    }
    out
}

fn build_response(
    request: &SipMessage,
    status: u16,
    reason: &str,
    extra: &[(&str, String)],
) -> String {
    let mut out = String::with_capacity(256);
    out.push_str(&format!("SIP/2.0 {status} {reason}\r\n"));

    // Via: echo the entire stack in order so the response routes back down the path.
    for via in request.header_all("Via") {
        out.push_str(&format!("Via: {via}\r\n"));
    }

    if let Some(from) = request.header("From") {
        out.push_str(&format!("From: {from}\r\n"));
    }

    // To: echo it, and on a 2xx add a tag if the request had none (RFC 3261 §8.2.6.2).
    if let Some(to) = request.header("To") {
        if (200..300).contains(&status) && request.to_tag().is_none() {
            let seed = format!("{}{}", request.call_id().unwrap_or(""), to);
            out.push_str(&format!("To: {to};tag={}\r\n", dialog_tag(&seed)));
        } else {
            out.push_str(&format!("To: {to}\r\n"));
        }
    }

    if let Some(call_id) = request.call_id() {
        out.push_str(&format!("Call-ID: {call_id}\r\n"));
    }
    if let Some(cseq) = request.cseq() {
        out.push_str(&format!("CSeq: {cseq}\r\n"));
    }

    for (name, value) in extra {
        out.push_str(&format!("{name}: {value}\r\n"));
    }

    out.push_str("Server: commosd\r\n");
    out.push_str("Content-Length: 0\r\n");
    out.push_str("\r\n");
    out
}

// --- URI / parameter helpers ---------------------------------------------------------

/// Normalise a header name to a canonical lowercase long form, expanding the RFC 3261
/// §7.3.3 / §20 compact forms so `Via`, `via` and `v` all compare equal. Known names return
/// a `'static` literal (no allocation); anything else returns an owned lowercase copy.
fn canonical_header(name: &str) -> Cow<'static, str> {
    let lower = name.trim().to_ascii_lowercase();
    let literal: Option<&'static str> = match lower.as_str() {
        "via" | "v" => Some("via"),
        "from" | "f" => Some("from"),
        "to" | "t" => Some("to"),
        "call-id" | "i" => Some("call-id"),
        "contact" | "m" => Some("contact"),
        "content-length" | "l" => Some("content-length"),
        "content-type" | "c" => Some("content-type"),
        "content-encoding" | "e" => Some("content-encoding"),
        "subject" | "s" => Some("subject"),
        "supported" | "k" => Some("supported"),
        "cseq" => Some("cseq"),
        "expires" => Some("expires"),
        "user-agent" => Some("user-agent"),
        "max-forwards" => Some("max-forwards"),
        "allow" => Some("allow"),
        _ => None,
    };
    match literal {
        Some(s) => Cow::Borrowed(s),
        None => Cow::Owned(lower),
    }
}

/// Extract the addr-spec (bare URI) from a name-addr or addr-spec header value.
///
/// `"Bob" <sip:100@host>;tag=x` → `sip:100@host`; `sip:100@host;tag=x` → `sip:100@host`.
/// When angle brackets are present the enclosed URI (with its own params) is returned; when
/// absent, everything up to the first `;` (the header parameters) is the addr-spec.
fn addr_spec(value: &str) -> &str {
    let v = value.trim();
    if let Some(lt) = v.find('<') {
        let after = &v[lt + 1..];
        if let Some(gt) = after.find('>') {
            return after[..gt].trim();
        }
    }
    match v.find(';') {
        Some(i) => v[..i].trim(),
        None => v,
    }
}

/// The Address-of-Record: the addr-spec with any URI parameters/headers stripped.
/// `<sip:100@host:5060;transport=udp>` → `sip:100@host:5060`.
fn uri_aor(value: &str) -> String {
    let spec = addr_spec(value);
    let end = spec.find([';', '?']).unwrap_or(spec.len());
    spec[..end].trim().to_string()
}

/// The header-parameter portion of a name-addr/addr-spec value — everything after the
/// closing `>` (name-addr) or after the first `;` (bare addr-spec). Where `expires=` lives
/// on a Contact.
fn header_params(value: &str) -> &str {
    let v = value.trim();
    if let Some(gt) = v.find('>') {
        &v[gt + 1..]
    } else {
        match v.find(';') {
            Some(i) => &v[i..],
            None => "",
        }
    }
}

/// Find a `;name=value` parameter (case-insensitive name) in a header value, searching only
/// the parameter region so a `;tag=` inside an angle-bracketed URI is not confused with a
/// header parameter of the same name.
fn find_param(value: &str, key: &str) -> Option<String> {
    for part in header_params(value).split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.split_once('=') {
            Some((k, v)) if k.trim().eq_ignore_ascii_case(key) => {
                return Some(v.trim().trim_matches('"').to_string());
            }
            None if part.eq_ignore_ascii_case(key) => return Some(String::new()),
            _ => {}
        }
    }
    None
}

/// A stable, opaque dialog tag derived from a seed (Call-ID + To). Deterministic **on purpose**:
/// a 2xx response's To-tag must be identical across retransmissions of that response so the peer
/// keeps matching the same dialog. Because it labels *our* side of an already-established dialog
/// (not a value an attacker must fail to guess to inject a request), determinism here is safe —
/// unpredictability is required only for locally-originated request branches/tags, which use
/// [`rand_tag`].
fn dialog_tag(seed: &str) -> String {
    let mut h = DefaultHasher::new();
    seed.hash(&mut h);
    format!("cmos{:016x}", h.finish())
}

/// A CSPRNG-backed opaque token (128 bits, hex) for a locally-originated request's Via branch,
/// From tag, and Call-ID. Unpredictable so an off-path attacker cannot guess the transaction
/// identifiers and forge a matching response or in-dialog request over UDP.
fn rand_tag() -> String {
    let mut b = [0u8; 16];
    getrandom::getrandom(&mut b).expect("OS CSPRNG available for SIP tag generation");
    let mut s = String::with_capacity(4 + 32);
    s.push_str("cmos");
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    // A realistic REGISTER as a Linphone-class softphone emits it (CRLF-terminated).
    const REGISTER: &str = "REGISTER sip:example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.168.1.5:5060;branch=z9hG4bKnashds7;rport\r\n\
Max-Forwards: 70\r\n\
From: \"Alice\" <sip:100@example.com>;tag=a6c85cf\r\n\
To: <sip:100@example.com>\r\n\
Call-ID: 843817637684230@998sdasdh09\r\n\
CSeq: 1826 REGISTER\r\n\
Contact: <sip:100@192.168.1.5:5060>;expires=600\r\n\
Expires: 600\r\n\
User-Agent: Linphone/5.0\r\n\
Content-Length: 0\r\n\
\r\n";

    #[test]
    fn parses_register() {
        let msg = parse(REGISTER.as_bytes()).unwrap();
        assert_eq!(msg.method(), Some("REGISTER"));
        assert_eq!(msg.request_uri(), Some("sip:example.com"));
        assert_eq!(msg.call_id(), Some("843817637684230@998sdasdh09"));
        assert_eq!(msg.cseq(), Some("1826 REGISTER"));
        assert_eq!(msg.cseq_parts(), Some((1826, "REGISTER")));
        assert_eq!(msg.register_aor().as_deref(), Some("sip:100@example.com"));
        assert_eq!(
            msg.contact_uri().as_deref(),
            Some("sip:100@192.168.1.5:5060")
        );
        assert_eq!(msg.expires(), 600);
        assert_eq!(msg.user_agent(), Some("Linphone/5.0"));
        assert_eq!(msg.via_branch().as_deref(), Some("z9hG4bKnashds7"));
        assert_eq!(msg.from_tag().as_deref(), Some("a6c85cf"));
        assert_eq!(msg.to_tag(), None);
        assert_eq!(msg.max_forwards(), Some(70));
    }

    #[test]
    fn parses_invite_with_body() {
        let invite = "INVITE sip:200@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.168.1.5:5060;branch=z9hG4bK776asdhds\r\n\
From: <sip:100@example.com>;tag=1928301774\r\n\
To: <sip:200@example.com>\r\n\
Call-ID: a84b4c76e66710\r\n\
CSeq: 314159 INVITE\r\n\
Contact: <sip:100@192.168.1.5:5060>\r\n\
Content-Type: application/sdp\r\n\
Content-Length: 18\r\n\
\r\n\
v=0\r\no=- 0 0 IN IP4";
        let msg = parse(invite.as_bytes()).unwrap();
        assert_eq!(msg.method(), Some("INVITE"));
        assert_eq!(msg.request_uri(), Some("sip:200@example.com"));
        assert_eq!(msg.call_id(), Some("a84b4c76e66710"));
        assert_eq!(msg.cseq_parts(), Some((314159, "INVITE")));
        assert_eq!(msg.to_aor().as_deref(), Some("sip:200@example.com"));
        assert_eq!(msg.content_length(), Some(18));
        assert_eq!(msg.body(), b"v=0\r\no=- 0 0 IN IP4");
        // No Expires anywhere → the RFC default.
        assert_eq!(msg.expires(), DEFAULT_EXPIRES);
    }

    #[test]
    fn parses_options_with_compact_headers_and_lf_only() {
        // Compact header forms + bare `\n` line endings + mixed casing.
        let options = "OPTIONS sip:example.com SIP/2.0\n\
v: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKopt\n\
f: <sip:probe@example.com>;tag=99\n\
t: <sip:example.com>\n\
i: opt-call-id-1\n\
CSeq: 1 OPTIONS\n\
l: 0\n\
\n";
        let msg = parse(options.as_bytes()).unwrap();
        assert_eq!(msg.method(), Some("OPTIONS"));
        // Compact forms resolve through the typed accessors.
        assert_eq!(msg.call_id(), Some("opt-call-id-1"));
        assert_eq!(msg.from_tag().as_deref(), Some("99"));
        assert_eq!(msg.content_length(), Some(0));
        assert_eq!(msg.via_branch().as_deref(), Some("z9hG4bKopt"));
    }

    #[test]
    fn parses_response_status_line() {
        let ok = "SIP/2.0 200 OK\r\nCall-ID: x\r\nCSeq: 1 REGISTER\r\n\r\n";
        let msg = parse(ok.as_bytes()).unwrap();
        assert!(!msg.is_request());
        assert_eq!(msg.status(), Some(200));
        assert_eq!(msg.method(), None);
    }

    #[test]
    fn folded_header_is_unfolded() {
        // NB: written on one line — a `\`-continuation in a Rust literal would strip the
        // leading fold whitespace, which is exactly the byte that makes this a folded header.
        let folded = "REGISTER sip:example.com SIP/2.0\r\nContact: <sip:100@192.168.1.5:5060>\r\n ;expires=1200\r\nCall-ID: fold-1\r\nCSeq: 1 REGISTER\r\nTo: <sip:100@example.com>\r\n\r\n";
        let msg = parse(folded.as_bytes()).unwrap();
        // The continuation line is joined onto the Contact value → expires param is seen.
        assert_eq!(msg.expires(), 1200);
        assert_eq!(
            msg.contact_uri().as_deref(),
            Some("sip:100@192.168.1.5:5060")
        );
    }

    #[test]
    fn empty_datagram_errors() {
        assert_eq!(parse(b"   \r\n"), Err(SipParseError::Empty));
    }

    #[test]
    fn malformed_start_line_errors() {
        assert!(matches!(
            parse(b"this is not sip\r\n\r\n"),
            Err(SipParseError::MalformedStartLine(_))
        ));
    }

    #[test]
    fn builds_register_200_and_round_trips() {
        let req = parse(REGISTER.as_bytes()).unwrap();
        let resp = response_with(
            &req,
            200,
            "OK",
            &[
                ("Contact", "<sip:100@192.168.1.5:5060>;expires=600".to_string()),
                ("Expires", "600".to_string()),
            ],
        );

        // Status line and mandatory framing.
        assert!(resp.starts_with("SIP/2.0 200 OK\r\n"));
        assert!(resp.ends_with("\r\n\r\n"));
        assert!(resp.contains("Server: commosd\r\n"));
        assert!(resp.contains("Content-Length: 0\r\n"));
        assert!(resp.contains("Expires: 600\r\n"));

        // The response must be parseable and round-trip the dialog identifiers.
        let parsed = parse(resp.as_bytes()).unwrap();
        assert_eq!(parsed.status(), Some(200));
        assert_eq!(parsed.call_id(), req.call_id());
        assert_eq!(parsed.cseq(), req.cseq());
        // Via echoed verbatim so the response routes home.
        assert_eq!(parsed.header("Via"), req.header("Via"));
        // A To tag was synthesised (the request had none) on this 2xx.
        assert!(parsed.to_tag().is_some());
    }

    #[test]
    fn response_echoes_full_via_stack() {
        let req = "OPTIONS sip:example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP proxy.example.com;branch=z9hG4bK1\r\n\
Via: SIP/2.0/UDP 192.168.1.5:5060;branch=z9hG4bK2\r\n\
From: <sip:a@example.com>;tag=1\r\n\
To: <sip:example.com>\r\n\
Call-ID: multi-via\r\n\
CSeq: 1 OPTIONS\r\n\
\r\n";
        let msg = parse(req.as_bytes()).unwrap();
        let resp = response(&msg, 200, "OK");
        let via_lines: Vec<_> = resp.lines().filter(|l| l.starts_with("Via:")).collect();
        assert_eq!(via_lines.len(), 2, "both Via headers echoed in order");
        assert!(via_lines[0].contains("branch=z9hG4bK1"));
        assert!(via_lines[1].contains("branch=z9hG4bK2"));
    }

    #[test]
    fn request_builds_and_round_trips() {
        let sdp = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\n";
        let out = request(
            "invite", // lowercased on purpose — should be normalised to INVITE
            "sip:200@192.168.1.9:5060",
            &[
                ("From", "<sip:100@commos>;tag=abc".to_string()),
                ("To", "<sip:200@192.168.1.9:5060>".to_string()),
                ("Call-ID", "call-xyz".to_string()),
                ("CSeq", "1 INVITE".to_string()),
                ("Contact", "<sip:commos@10.0.0.1>".to_string()),
            ],
            Some(("application/sdp", sdp)),
        );

        // Request line is well-formed and the method is uppercased.
        assert!(out.starts_with("INVITE sip:200@192.168.1.9:5060 SIP/2.0\r\n"));

        let msg = parse(out.as_bytes()).unwrap();
        assert_eq!(msg.method(), Some("INVITE"));
        assert_eq!(msg.request_uri(), Some("sip:200@192.168.1.9:5060"));
        assert_eq!(msg.call_id(), Some("call-xyz"));
        assert_eq!(msg.cseq_parts(), Some((1, "INVITE")));
        assert_eq!(msg.from_tag().as_deref(), Some("abc"));
        assert_eq!(msg.to_aor().as_deref(), Some("sip:200@192.168.1.9:5060"));
        // Framing the caller omitted was filled in.
        assert!(msg.via_branch().is_some());
        assert_eq!(msg.max_forwards(), Some(70));
        assert_eq!(msg.content_length(), Some(sdp.len()));
        assert_eq!(msg.body(), sdp.as_bytes());
    }

    #[test]
    fn request_bodyless_fills_mandatory_headers() {
        // Nothing but method + URI supplied → every mandatory header is generated.
        let out = request("ACK", "sip:200@host", &[], None);
        let msg = parse(out.as_bytes()).unwrap();
        assert_eq!(msg.method(), Some("ACK"));
        assert!(msg.via_branch().is_some());
        assert!(msg.from_tag().is_some());
        assert!(msg.call_id().is_some());
        assert_eq!(msg.cseq_parts(), Some((1, "ACK")));
        assert_eq!(msg.max_forwards(), Some(70));
        assert_eq!(msg.content_length(), Some(0));
    }

    #[test]
    fn wildcard_contact_yields_no_uri() {
        let dereg = "REGISTER sip:example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.168.1.5:5060;branch=z9hG4bKd\r\n\
From: <sip:100@example.com>;tag=1\r\n\
To: <sip:100@example.com>\r\n\
Call-ID: dereg-1\r\n\
CSeq: 2 REGISTER\r\n\
Contact: *\r\n\
Expires: 0\r\n\
\r\n";
        let msg = parse(dereg.as_bytes()).unwrap();
        assert_eq!(msg.contact_uri(), None);
        assert_eq!(msg.expires(), 0);
    }
}
