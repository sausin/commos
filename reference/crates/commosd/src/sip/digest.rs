//! **Pure** SIP Digest access authentication — the RFC 3261 §22 profile of the HTTP
//! Digest scheme (RFC 2617). This is what lets the ingress challenge a REGISTER/INVITE with
//! a `401 Unauthorized` + `WWW-Authenticate`, then verify the phone's `Authorization` reply.
//!
//! Like [`super::message`], this module has no I/O and no async: it is pure functions over
//! `&str`, so the whole thing is exhaustively unit-testable (see the tests at the bottom).
//! The one dependency is the `md-5` crate for the MD5 primitive the scheme mandates.
//!
//! The correctness anchor is the canonical RFC 2617 §3.5 worked example (`Mufasa` /
//! `Circle Of Life`), asserted verbatim in the tests.

use md5::{Digest, Md5};

/// A server-issued Digest challenge — the `realm` the credentials scope to and the opaque,
/// single-use `nonce`. Rendered into the `WWW-Authenticate` header value of a `401`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Challenge {
    pub realm: String,
    pub nonce: String,
}

impl Challenge {
    /// Construct a challenge from a `realm` and a freshly-minted `nonce`.
    pub fn new(realm: impl Into<String>, nonce: impl Into<String>) -> Self {
        Challenge {
            realm: realm.into(),
            nonce: nonce.into(),
        }
    }

    /// The `WWW-Authenticate` header *value* for a `401`, advertising MD5 + `qop="auth"`:
    /// `Digest realm="<realm>", nonce="<nonce>", algorithm=MD5, qop="auth"`.
    pub fn header_value(&self) -> String {
        format!(
            "Digest realm=\"{}\", nonce=\"{}\", algorithm=MD5, qop=\"auth\"",
            self.realm, self.nonce
        )
    }
}

/// A parsed client `Authorization` (or `Proxy-Authorization`) Digest response. The five
/// fields the scheme always requires are owned `String`s; the qop-auth extras
/// (`qop`/`nc`/`cnonce`) and `algorithm` are optional.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    pub realm: String,
    pub nonce: String,
    pub uri: String,
    pub response: String,
    pub qop: Option<String>,
    pub nc: Option<String>,
    pub cnonce: Option<String>,
    pub algorithm: Option<String>,
}

impl Credentials {
    /// Parse an `Authorization` header value of the form
    /// `Digest username="100", realm="commos", nonce="abc", uri="sip:commos.local",
    /// response="6629...", qop=auth, nc=00000001, cnonce="0a4f113b", algorithm=MD5`.
    ///
    /// Liberal in what it accepts (cf. [`super::message::parse`]): an optional leading
    /// `Digest ` scheme (any casing), comma-separated `key=value` pairs, values that are
    /// bare or double-quoted (surrounding quotes stripped), arbitrary surrounding
    /// whitespace, and unknown keys (ignored). Returns `None` if any of the required
    /// `username`/`realm`/`nonce`/`uri`/`response` fields is absent.
    pub fn parse(header_value: &str) -> Option<Credentials> {
        // Strip an optional leading `Digest` scheme token, case-insensitively.
        let body = {
            let trimmed = header_value.trim();
            match trimmed.get(..6) {
                Some(prefix) if prefix.eq_ignore_ascii_case("Digest") => trimmed[6..].trim_start(),
                _ => trimmed,
            }
        };

        let mut username = None;
        let mut realm = None;
        let mut nonce = None;
        let mut uri = None;
        let mut response = None;
        let mut qop = None;
        let mut nc = None;
        let mut cnonce = None;
        let mut algorithm = None;

        for pair in split_digest_params(body) {
            let (key, value) = match pair.split_once('=') {
                Some((k, v)) => (k.trim(), unquote(v.trim())),
                None => continue,
            };
            match key.to_ascii_lowercase().as_str() {
                "username" => username = Some(value),
                "realm" => realm = Some(value),
                "nonce" => nonce = Some(value),
                "uri" => uri = Some(value),
                "response" => response = Some(value),
                "qop" => qop = Some(value),
                "nc" => nc = Some(value),
                "cnonce" => cnonce = Some(value),
                "algorithm" => algorithm = Some(value),
                _ => {} // unknown key — ignore (be liberal in what we accept).
            }
        }

        Some(Credentials {
            username: username?,
            realm: realm?,
            nonce: nonce?,
            uri: uri?,
            response: response?,
            qop,
            nc,
            cnonce,
            algorithm,
        })
    }
}

/// Split a Digest parameter list on commas, but never inside a double-quoted value (a quoted
/// value may legitimately contain a comma, e.g. `realm="a, b"`).
fn split_digest_params(input: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut in_quotes = false;
    let mut start = 0;
    for (i, ch) in input.char_indices() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                parts.push(input[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(input[start..].trim());
    parts.into_iter().filter(|p| !p.is_empty()).collect()
}

/// Strip a single pair of surrounding double quotes from a value, if present.
fn unquote(value: &str) -> String {
    let v = value.trim();
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        v[1..v.len() - 1].to_string()
    } else {
        v.to_string()
    }
}

/// Lowercase hex MD5 of `input` — the primitive every Digest hash (`HA1`, `HA2`, the final
/// response) is built from.
fn md5_hex(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(32);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Compute the Digest `response` value (RFC 2617 §3.2.2.1) as lowercase hex.
///
/// `HA1 = MD5(username:realm:password)`, `HA2 = MD5(method:uri)`. With `qop == Some("auth")`
/// *and* both `nc` and `cnonce` present, the response is
/// `MD5(HA1:nonce:nc:cnonce:auth:HA2)`; otherwise (no qop, or a malformed qop=auth missing
/// its `nc`/`cnonce`) it falls back to the legacy `MD5(HA1:nonce:HA2)` so this never panics.
// The digest response is a function of exactly these RFC 2617 inputs; grouping them into a
// struct would only rename the same fields.
#[allow(clippy::too_many_arguments)]
pub fn compute_response(
    username: &str,
    realm: &str,
    password: &str,
    method: &str,
    uri: &str,
    nonce: &str,
    qop: Option<&str>,
    nc: Option<&str>,
    cnonce: Option<&str>,
) -> String {
    let ha1 = md5_hex(&format!("{username}:{realm}:{password}"));
    let ha2 = md5_hex(&format!("{method}:{uri}"));

    match (qop, nc, cnonce) {
        (Some("auth"), Some(nc), Some(cnonce)) => {
            md5_hex(&format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}"))
        }
        // No qop, or a qop=auth that is missing its nc/cnonce: fall back to the legacy form
        // rather than panic.
        _ => md5_hex(&format!("{ha1}:{nonce}:{ha2}")),
    }
}

/// Verify a client's [`Credentials`] against the shared `password` and the request `method`.
///
/// Recomputes the expected response from the credentials' own advertised fields
/// (`realm`/`nonce`/`uri`/`qop`/`nc`/`cnonce`) plus the supplied secret, and compares it to
/// `creds.response` case-insensitively (hex may be sent upper- or lower-case). Returns `true`
/// iff they match.
pub fn verify(creds: &Credentials, method: &str, password: &str) -> bool {
    let expected = compute_response(
        &creds.username,
        &creds.realm,
        password,
        method,
        &creds.uri,
        &creds.nonce,
        creds.qop.as_deref(),
        creds.nc.as_deref(),
        creds.cnonce.as_deref(),
    );
    expected.eq_ignore_ascii_case(&creds.response)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical RFC 2617 §3.5 worked example — the correctness anchor for the whole
    /// module. If this vector matches a real interop stack, the math is right.
    #[test]
    fn rfc2617_worked_example() {
        let response = compute_response(
            "Mufasa",
            "testrealm@host.com",
            "Circle Of Life",
            "GET",
            "/dir/index.html",
            "dcd98b7102dd2f0e8b11d0f600bfb0c093",
            Some("auth"),
            Some("00000001"),
            Some("0a4f113b"),
        );
        assert_eq!(response, "6629fae49393a05397450978507c4ef1");
    }

    /// No-qop path: compute a response, wrap it in Credentials, and confirm `verify` accepts
    /// the right password and rejects a wrong one.
    #[test]
    fn no_qop_verify_roundtrip() {
        let username = "100";
        let realm = "commos";
        let password = "secret";
        let method = "REGISTER";
        let uri = "sip:commos.local";
        let nonce = "abc123";

        let response = compute_response(
            username, realm, password, method, uri, nonce, None, None, None,
        );

        // The legacy form is exactly MD5(HA1:nonce:HA2) — assert internal consistency.
        let ha1 = md5_hex(&format!("{username}:{realm}:{password}"));
        let ha2 = md5_hex(&format!("{method}:{uri}"));
        assert_eq!(response, md5_hex(&format!("{ha1}:{nonce}:{ha2}")));

        let creds = Credentials {
            username: username.to_string(),
            realm: realm.to_string(),
            nonce: nonce.to_string(),
            uri: uri.to_string(),
            response: response.clone(),
            qop: None,
            nc: None,
            cnonce: None,
            algorithm: None,
        };
        assert!(verify(&creds, method, password));
        assert!(!verify(&creds, method, "wrong-password"));
    }

    /// A qop=auth credential (the Mufasa vector) round-trips through `verify`, and a wrong
    /// password fails.
    #[test]
    fn qop_auth_verify_roundtrip() {
        let creds = Credentials {
            username: "Mufasa".to_string(),
            realm: "testrealm@host.com".to_string(),
            nonce: "dcd98b7102dd2f0e8b11d0f600bfb0c093".to_string(),
            uri: "/dir/index.html".to_string(),
            response: "6629fae49393a05397450978507c4ef1".to_string(),
            qop: Some("auth".to_string()),
            nc: Some("00000001".to_string()),
            cnonce: Some("0a4f113b".to_string()),
            algorithm: Some("MD5".to_string()),
        };
        assert!(verify(&creds, "GET", "Circle Of Life"));
        assert!(!verify(&creds, "GET", "Hakuna Matata"));
        // Case-insensitive response comparison: uppercase hex still verifies.
        let mut upper = creds.clone();
        upper.response = upper.response.to_ascii_uppercase();
        assert!(verify(&upper, "GET", "Circle Of Life"));
    }

    #[test]
    fn parse_full_authorization() {
        let header = "Digest username=\"100\", realm=\"commos\", nonce=\"abc\", \
uri=\"sip:commos.local\", response=\"deadbeef\", qop=auth, nc=00000001, cnonce=\"xyz\", \
algorithm=MD5";
        let creds = Credentials::parse(header).expect("should parse");
        assert_eq!(creds.username, "100");
        assert_eq!(creds.realm, "commos");
        assert_eq!(creds.nonce, "abc");
        assert_eq!(creds.uri, "sip:commos.local");
        assert_eq!(creds.response, "deadbeef");
        assert_eq!(creds.qop.as_deref(), Some("auth"));
        assert_eq!(creds.nc.as_deref(), Some("00000001"));
        assert_eq!(creds.cnonce.as_deref(), Some("xyz"));
        assert_eq!(creds.algorithm.as_deref(), Some("MD5"));
    }

    #[test]
    fn parse_missing_required_field_is_none() {
        // No `response` → not a usable credential.
        let header = "Digest username=\"100\", realm=\"commos\", nonce=\"abc\", \
uri=\"sip:commos.local\"";
        assert_eq!(Credentials::parse(header), None);
    }

    #[test]
    fn parse_tolerates_quoted_and_unquoted_and_no_scheme() {
        // Mixed quoting, no leading `Digest` scheme, extra whitespace, unknown key.
        let header = "  username=alice ,realm=\"commos\",  nonce = n1 ,uri=sip:x , \
response=\"abcd\" , opaque=\"ignored\" , cnonce=cc  ";
        let creds = Credentials::parse(header).expect("should parse");
        assert_eq!(creds.username, "alice");
        assert_eq!(creds.realm, "commos");
        assert_eq!(creds.nonce, "n1");
        assert_eq!(creds.uri, "sip:x");
        assert_eq!(creds.response, "abcd");
        assert_eq!(creds.cnonce.as_deref(), Some("cc"));
        assert_eq!(creds.qop, None);
        assert_eq!(creds.algorithm, None);
    }

    #[test]
    fn challenge_header_value_is_exact() {
        let ch = Challenge::new("commos", "abc123");
        assert_eq!(
            ch.header_value(),
            "Digest realm=\"commos\", nonce=\"abc123\", algorithm=MD5, qop=\"auth\""
        );
    }
}
