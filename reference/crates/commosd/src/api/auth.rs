//! Authentication → tenant context (Volume 9; CMOS-04-API bearer auth).
//!
//! The frozen API secures every `/v1` route with `bearerAuth` (JWT). This slice performs
//! real **HS256 JWT** verification: the signature is checked with a configured shared
//! secret (constant-time), `exp`/`nbf` are enforced, and the tenant is taken from a
//! `tenant_id`/`tid` claim (a UUIDv7). For local development and the existing test suite a
//! **dev token** of the form `tenant:<uuidv7>` is still accepted *when dev mode is enabled*,
//! so tenant isolation (CMOS-03-ARCH-050) stays real and testable with zero setup.
//!
//! The extractor is the single choke point: `verify_bearer` is a pure function (fully
//! unit-tested below) and the `FromRequestParts` impl merely sources the [`AuthConfig`]
//! from `AppState` and delegates to it.

use axum::async_trait;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use base64::Engine;
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

use commos_core::common::Uuid;

use super::problem::Problem;

type HmacSha256 = Hmac<Sha256>;

/// The authenticated caller's tenant. Every data access is scoped by this
/// (defence in depth, CMOS-03-ARCH-050).
#[derive(Clone, Copy, Debug)]
pub struct TenantContext {
    pub tenant_id: Uuid,
}

/// Verifier configuration — the shared secret for HS256 JWTs and whether the legacy
/// `tenant:<uuid>` development token is still honoured.
///
/// Held on `AppState` by the hub and handed to [`verify_bearer`]. Cheap to clone.
#[derive(Clone, Debug)]
pub struct AuthConfig {
    /// HS256 signing secret (raw bytes). `None` disables JWT verification — any JWT-shaped
    /// token is then rejected, leaving only the dev token (if enabled).
    pub jwt_secret: Option<Vec<u8>>,
    /// When `true`, accept the `tenant:<uuidv7>` development bearer. Default `true` so
    /// existing tests and local dashboards keep working with no configuration.
    pub dev_tokens: bool,
}

impl Default for AuthConfig {
    fn default() -> Self {
        // Zero-config default: no JWT secret, dev tokens on → unchanged behaviour.
        AuthConfig {
            jwt_secret: None,
            dev_tokens: true,
        }
    }
}

/// Implemented by the application state (`AppState`) so the extractor can reach the
/// [`AuthConfig`] without this module depending on the concrete state type. The hub
/// implements this in `state.rs` (`fn auth_config(&self) -> &AuthConfig { &self.auth }`).
pub trait HasAuthConfig {
    fn auth_config(&self) -> &AuthConfig;
}

/// Verify an `Authorization` header value and resolve the caller's tenant.
///
/// `auth_header` is the raw header value (e.g. `"Bearer eyJ...".`). This is the pure,
/// side-effect-free core of authentication — see the unit tests below.
pub fn verify_bearer(auth_header: &str, cfg: &AuthConfig) -> Result<TenantContext, Problem> {
    let token = auth_header
        .strip_prefix("Bearer ")
        .ok_or_else(|| Problem::unauthorized("expected a Bearer token"))?
        .trim();

    if token.is_empty() {
        return Err(Problem::unauthorized("empty bearer token"));
    }

    // A compact JWS/JWT is exactly three dot-separated segments: header.payload.signature.
    let segments: Vec<&str> = token.split('.').collect();
    if segments.len() == 3 {
        return verify_jwt(&segments, cfg);
    }

    // Not a JWT — fall back to the development token shape `tenant:<uuidv7>`.
    if !cfg.dev_tokens {
        return Err(Problem::unauthorized("unrecognised token"));
    }
    let raw = token
        .strip_prefix("tenant:")
        .ok_or_else(|| Problem::unauthorized("unrecognised token"))?;
    let tenant_id = Uuid::parse(raw)
        .map_err(|_| Problem::unauthorized("token tenant is not a valid UUIDv7"))?;
    Ok(TenantContext { tenant_id })
}

/// Verify an HS256 JWT given its three base64url segments.
fn verify_jwt(segments: &[&str], cfg: &AuthConfig) -> Result<TenantContext, Problem> {
    let secret = cfg
        .jwt_secret
        .as_deref()
        .ok_or_else(|| Problem::unauthorized("JWT authentication is not configured"))?;

    let header_b64 = segments[0];
    let payload_b64 = segments[1];
    let signature_b64 = segments[2];

    // Header must declare HS256 (this reference supports only the HMAC family here).
    let header = decode_json(header_b64).map_err(|_| Problem::unauthorized("malformed JWT header"))?;
    match header.get("alg").and_then(Value::as_str) {
        Some("HS256") => {}
        _ => return Err(Problem::unauthorized("unsupported JWT alg (expected HS256)")),
    }

    // Verify the signature over `header.payload` in constant time.
    let signing_input = format!("{header_b64}.{payload_b64}");
    let signature = b64url_decode(signature_b64)
        .map_err(|_| Problem::unauthorized("malformed JWT signature"))?;
    let mut mac = HmacSha256::new_from_slice(secret)
        .map_err(|_| Problem::unauthorized("invalid signing secret"))?;
    mac.update(signing_input.as_bytes());
    // `verify_slice` is a constant-time comparison of the computed vs. presented tag.
    mac.verify_slice(&signature)
        .map_err(|_| Problem::unauthorized("invalid JWT signature"))?;

    // Signature is good — now validate the claims.
    let claims =
        decode_json(payload_b64).map_err(|_| Problem::unauthorized("malformed JWT claims"))?;

    let now = time::OffsetDateTime::now_utc().unix_timestamp();

    // `exp` is required and must be in the future.
    let exp = claims
        .get("exp")
        .and_then(Value::as_i64)
        .ok_or_else(|| Problem::unauthorized("JWT missing exp claim"))?;
    if now >= exp {
        return Err(Problem::unauthorized("JWT has expired"));
    }

    // `nbf` is optional; if present the token is not yet valid until then.
    if let Some(nbf) = claims.get("nbf").and_then(Value::as_i64) {
        if now < nbf {
            return Err(Problem::unauthorized("JWT is not yet valid (nbf)"));
        }
    }

    // Tenant comes from `tenant_id` or `tid`, as a canonical UUIDv7.
    let tenant_raw = claims
        .get("tenant_id")
        .or_else(|| claims.get("tid"))
        .and_then(Value::as_str)
        .ok_or_else(|| Problem::unauthorized("JWT missing tenant claim"))?;
    let tenant_id = Uuid::parse(tenant_raw)
        .map_err(|_| Problem::unauthorized("JWT tenant claim is not a valid UUIDv7"))?;

    Ok(TenantContext { tenant_id })
}

/// Base64url-decode (no padding, per RFC 7515) a JWT segment.
fn b64url_decode(segment: &str) -> Result<Vec<u8>, base64::DecodeError> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(segment)
}

/// Base64url-decode a segment and parse it as a JSON object.
fn decode_json(segment: &str) -> Result<Value, ()> {
    let bytes = b64url_decode(segment).map_err(|_| ())?;
    serde_json::from_slice(&bytes).map_err(|_| ())
}

#[async_trait]
impl<S> FromRequestParts<S> for TenantContext
where
    S: Send + Sync + HasAuthConfig,
{
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| Problem::unauthorized("missing Authorization header"))?;

        verify_bearer(header, state.auth_config())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A stable, canonical lowercase UUIDv7 for tests.
    const TENANT: &str = "01890a5d-ac96-774b-bcce-b302099a8057";
    const SECRET: &[u8] = b"super-secret-signing-key";

    fn b64url(bytes: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    /// Mint an HS256 JWT with the given claims JSON string, signed with `secret`.
    fn mint(claims: &str, secret: &[u8]) -> String {
        let header = b64url(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = b64url(claims.as_bytes());
        let signing_input = format!("{header}.{payload}");
        let mut mac = HmacSha256::new_from_slice(secret).unwrap();
        mac.update(signing_input.as_bytes());
        let sig = b64url(&mac.finalize().into_bytes());
        format!("{signing_input}.{sig}")
    }

    fn jwt_cfg() -> AuthConfig {
        AuthConfig {
            jwt_secret: Some(SECRET.to_vec()),
            dev_tokens: false,
        }
    }

    fn future() -> i64 {
        time::OffsetDateTime::now_utc().unix_timestamp() + 3600
    }
    fn past() -> i64 {
        time::OffsetDateTime::now_utc().unix_timestamp() - 3600
    }

    #[test]
    fn valid_hs256_token_passes() {
        let token = mint(
            &format!(r#"{{"tenant_id":"{TENANT}","exp":{}}}"#, future()),
            SECRET,
        );
        let ctx = verify_bearer(&format!("Bearer {token}"), &jwt_cfg()).unwrap();
        assert_eq!(ctx.tenant_id.to_string(), TENANT);
    }

    #[test]
    fn tid_claim_alias_is_accepted() {
        let token = mint(&format!(r#"{{"tid":"{TENANT}","exp":{}}}"#, future()), SECRET);
        let ctx = verify_bearer(&format!("Bearer {token}"), &jwt_cfg()).unwrap();
        assert_eq!(ctx.tenant_id.to_string(), TENANT);
    }

    #[test]
    fn tampered_signature_fails() {
        let token = mint(
            &format!(r#"{{"tenant_id":"{TENANT}","exp":{}}}"#, future()),
            SECRET,
        );
        // Flip the last character of the signature.
        let mut bytes: Vec<char> = token.chars().collect();
        let last = bytes.len() - 1;
        bytes[last] = if bytes[last] == 'A' { 'B' } else { 'A' };
        let tampered: String = bytes.into_iter().collect();
        assert!(verify_bearer(&format!("Bearer {tampered}"), &jwt_cfg()).is_err());
    }

    #[test]
    fn wrong_secret_fails() {
        let token = mint(
            &format!(r#"{{"tenant_id":"{TENANT}","exp":{}}}"#, future()),
            b"a-different-secret",
        );
        assert!(verify_bearer(&format!("Bearer {token}"), &jwt_cfg()).is_err());
    }

    #[test]
    fn expired_token_fails() {
        let token = mint(
            &format!(r#"{{"tenant_id":"{TENANT}","exp":{}}}"#, past()),
            SECRET,
        );
        let err = verify_bearer(&format!("Bearer {token}"), &jwt_cfg()).unwrap_err();
        assert_eq!(err.status, 401);
    }

    #[test]
    fn not_yet_valid_nbf_fails() {
        let token = mint(
            &format!(
                r#"{{"tenant_id":"{TENANT}","exp":{},"nbf":{}}}"#,
                future(),
                future()
            ),
            SECRET,
        );
        assert!(verify_bearer(&format!("Bearer {token}"), &jwt_cfg()).is_err());
    }

    #[test]
    fn missing_exp_fails() {
        let token = mint(&format!(r#"{{"tenant_id":"{TENANT}"}}"#), SECRET);
        assert!(verify_bearer(&format!("Bearer {token}"), &jwt_cfg()).is_err());
    }

    #[test]
    fn missing_tenant_claim_fails() {
        let token = mint(&format!(r#"{{"exp":{}}}"#, future()), SECRET);
        assert!(verify_bearer(&format!("Bearer {token}"), &jwt_cfg()).is_err());
    }

    #[test]
    fn invalid_tenant_claim_fails() {
        let token = mint(
            &format!(r#"{{"tenant_id":"not-a-uuid","exp":{}}}"#, future()),
            SECRET,
        );
        assert!(verify_bearer(&format!("Bearer {token}"), &jwt_cfg()).is_err());
    }

    #[test]
    fn jwt_rejected_when_no_secret_configured() {
        let token = mint(
            &format!(r#"{{"tenant_id":"{TENANT}","exp":{}}}"#, future()),
            SECRET,
        );
        let cfg = AuthConfig {
            jwt_secret: None,
            dev_tokens: true,
        };
        assert!(verify_bearer(&format!("Bearer {token}"), &cfg).is_err());
    }

    #[test]
    fn dev_token_passes_when_dev_enabled() {
        let cfg = AuthConfig {
            jwt_secret: None,
            dev_tokens: true,
        };
        let ctx = verify_bearer(&format!("Bearer tenant:{TENANT}"), &cfg).unwrap();
        assert_eq!(ctx.tenant_id.to_string(), TENANT);
    }

    #[test]
    fn dev_token_rejected_when_dev_disabled() {
        let cfg = AuthConfig {
            jwt_secret: Some(SECRET.to_vec()),
            dev_tokens: false,
        };
        assert!(verify_bearer(&format!("Bearer tenant:{TENANT}"), &cfg).is_err());
    }

    #[test]
    fn malformed_header_fails() {
        let cfg = jwt_cfg();
        // No "Bearer " prefix.
        assert!(verify_bearer("Basic abc123", &cfg).is_err());
        // Prefix but empty token.
        assert!(verify_bearer("Bearer ", &cfg).is_err());
        // JWT-shaped but garbage segments.
        assert!(verify_bearer("Bearer a.b.c", &cfg).is_err());
    }

    #[test]
    fn default_config_is_dev_friendly() {
        let cfg = AuthConfig::default();
        assert!(cfg.dev_tokens);
        assert!(cfg.jwt_secret.is_none());
        // A dev token still works out of the box.
        assert!(verify_bearer(&format!("Bearer tenant:{TENANT}"), &cfg).is_ok());
    }
}
