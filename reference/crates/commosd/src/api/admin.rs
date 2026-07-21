//! Admin authentication — a login that gates the sensitive setup/config operations
//! (onboarding apply, config-as-code import) while keeping zero-config local dev working.
//!
//! ## Two modes, one extractor
//!
//! The whole design turns on whether an **admin password is configured**:
//!
//! - **Configured (production):** privileged routes require an opaque admin session token,
//!   presented as `Authorization: Bearer admin:<token>`. Tokens are minted by
//!   [`AdminAuth::login`] after a constant-time password check and expire after 12h. They
//!   are ephemeral, in-memory only (mirroring [`crate::control::registrations`]) — a hub
//!   restart logs everyone out, which is the safe default for a reference implementation.
//! - **Not configured (dev mode):** there is no admin password, so the extractor falls
//!   back to [`verify_bearer`] — *any* valid tenant/dev bearer is treated as admin. This
//!   keeps the existing test suite and the local dashboard working with zero setup, which
//!   is the project's zero-config promise.
//!
//! Because admin sessions are not (yet) tenant-aware, a request authorised via an admin
//! session is attributed to a fixed **dev tenant**, [`ADMIN_DEV_TENANT`]. In dev mode the
//! resolved tenant is the real one carried by the fallback bearer. See the security notes
//! at the bottom for what this defers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::async_trait;
use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::Json;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use commos_core::common::Uuid;

use super::auth::{verify_bearer, HasAuthConfig};
use super::problem::Problem;

/// Fixed tenant attributed to requests authorised by an admin **session** (configured
/// mode). Admin sessions are not per-tenant in this MVP, so privileged operations run as
/// this canonical dev tenant. A stable, canonical lowercase UUIDv7.
pub const ADMIN_DEV_TENANT: &str = "01920000-0000-7000-8000-000000000001";

/// How long a freshly-minted admin session token stays valid (seconds). 12 hours.
const SESSION_TTL_SECS: i64 = 12 * 60 * 60;

/// Ephemeral admin authentication state, held on `AppState` by the hub.
///
/// Cheap to clone (`Arc` handle over the session map). `password: None` means **dev mode** —
/// no admin password has been configured, so [`login`](Self::login) refuses to mint tokens
/// and the extractor falls back to tenant-bearer auth.
#[derive(Clone)]
pub struct AdminAuth {
    /// The configured admin password, or `None` in dev mode. Stored in the clear for the
    /// MVP; hashing at rest is a documented follow-up (see module security notes).
    password: Option<String>,
    /// Live admin sessions: opaque token → expiry (unix seconds). In-memory only.
    sessions: Arc<Mutex<HashMap<String, i64>>>,
}

impl AdminAuth {
    /// Construct admin auth. `password: None` ⇒ dev mode (no admin password configured).
    pub fn new(password: Option<String>) -> Self {
        AdminAuth {
            password,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Whether admin auth is in dev mode (no password configured).
    pub fn is_dev_mode(&self) -> bool {
        self.password.is_none()
    }

    /// Attempt a login. On a constant-time match against the configured password, mint a
    /// fresh opaque token, store it with a 12h expiry, and return it. Returns `None` if no
    /// password is configured (dev mode) or the password does not match.
    pub fn login(&self, password: &str) -> Option<String> {
        let configured = self.password.as_deref()?;
        if !constant_time_eq(password, configured) {
            return None;
        }
        // Opaque 256-bit token from two UUIDv7s (getrandom-backed via the uuid crate);
        // dashes stripped so it is a single hex-ish opaque string with no structure to
        // parse or guess. ~148 bits of randomness, which is ample for a session token.
        let token = format!("{}{}", Uuid::now_v7(), Uuid::now_v7()).replace('-', "");
        let expires = now_unix() + SESSION_TTL_SECS;
        let mut sessions = self.sessions.lock().expect("admin session mutex not poisoned");
        sessions.insert(token.clone(), expires);
        Some(token)
    }

    /// Invalidate a session token (idempotent — a no-op if unknown).
    pub fn logout(&self, token: &str) {
        let mut sessions = self.sessions.lock().expect("admin session mutex not poisoned");
        sessions.remove(token);
    }

    /// Whether `token` names a live (present and unexpired) session. Prunes expired
    /// sessions lazily on the way through.
    pub fn is_valid(&self, token: &str) -> bool {
        let now = now_unix();
        let mut sessions = self.sessions.lock().expect("admin session mutex not poisoned");
        // Lazy prune: drop everything already expired so the map does not grow unbounded.
        sessions.retain(|_, &mut exp| exp > now);
        sessions.get(token).is_some_and(|&exp| exp > now)
    }
}

/// Implemented by `AppState` so the extractor can reach [`AdminAuth`] without this module
/// depending on the concrete state type. The hub implements this in `state.rs`, exactly as
/// it does for [`HasAuthConfig`].
pub trait HasAdminAuth {
    fn admin_auth(&self) -> &AdminAuth;
}

/// An authorised admin request.
///
/// Obtained via the [`FromRequestParts`] impl below. Carries the resolved [`tenant_id`] to
/// scope any privileged work, a [`dev_mode`] flag (true when authorised via the tenant-bearer
/// fallback rather than an admin session), and the session `token` (present only in
/// configured mode) so [`logout`](AdminAuth::logout) can invalidate it.
///
/// [`tenant_id`]: AdminContext::tenant_id
/// [`dev_mode`]: AdminContext::dev_mode
#[derive(Clone, Debug)]
pub struct AdminContext {
    /// Tenant this admin request acts as. In configured mode this is [`ADMIN_DEV_TENANT`];
    /// in dev mode it is the real tenant carried by the fallback bearer.
    pub tenant_id: Uuid,
    /// True when authorised via the tenant-bearer fallback (no admin password configured).
    pub dev_mode: bool,
    /// The admin session token, in configured mode only. `None` in dev mode.
    pub token: Option<String>,
}

#[async_trait]
impl<S> FromRequestParts<S> for AdminContext
where
    S: Send + Sync + HasAdminAuth + HasAuthConfig,
{
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let trusted = super::peer::is_trusted_peer(parts);
        let header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| Problem::unauthorized("missing Authorization header"))?;

        let admin = state.admin_auth();

        if admin.is_dev_mode() {
            // Dev mode: a valid tenant/dev bearer is treated as admin (zero setup) — but ONLY
            // from a trusted (loopback/LAN) peer. A public peer can never gain admin without a
            // configured admin password, so an internet-exposed daemon is not silently open.
            if !trusted {
                return Err(Problem::unauthorized(
                    "admin access from a public network requires a configured admin session; \
                     set admin_password and POST /admin/login",
                ));
            }
            // `verify_bearer` is called with `trusted = true` here since we already gated on it.
            let ctx = verify_bearer(header, state.auth_config(), true)?;
            return Ok(AdminContext {
                tenant_id: ctx.tenant_id,
                dev_mode: true,
                token: None,
            });
        }

        // Configured mode: require `Bearer admin:<token>` naming a live session.
        let token = header
            .strip_prefix("Bearer ")
            .map(str::trim)
            .and_then(|t| t.strip_prefix("admin:"))
            .ok_or_else(|| Problem::unauthorized("expected an admin bearer token"))?;

        if !admin.is_valid(token) {
            return Err(Problem::unauthorized("invalid or expired admin session"));
        }

        let tenant_id = Uuid::parse(ADMIN_DEV_TENANT).expect("ADMIN_DEV_TENANT is a valid UUIDv7");
        Ok(AdminContext {
            tenant_id,
            dev_mode: false,
            token: Some(token.to_string()),
        })
    }
}

// --- Wire types -------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub token: String,
    /// Seconds until the token expires.
    pub expires_in: i64,
}

#[derive(Debug, Serialize)]
pub struct WhoamiResponse {
    /// Always `true` — reaching this handler means the caller is an authorised admin.
    pub admin: bool,
    /// True when admin auth is in dev mode (no admin password configured).
    pub dev_mode: bool,
}

#[derive(Debug, Serialize)]
pub struct LogoutResponse {
    pub logged_out: bool,
}

// --- Handlers ---------------------------------------------------------------------------

/// `POST /admin/login` — exchange the admin password for a session token.
///
/// Unauthenticated (this is how you *become* authenticated). In dev mode there is no admin
/// password, so this returns `400` telling the caller admin auth is disabled and dev tokens
/// already act as admin. A wrong password is `401`.
pub async fn login<S>(
    State(state): State<S>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, Problem>
where
    S: HasAdminAuth + Clone + Send + Sync + 'static,
{
    let admin = state.admin_auth();
    if admin.is_dev_mode() {
        return Err(Problem::bad_request(
            "admin auth is disabled (no admin password configured); \
             use a tenant/dev bearer token, which acts as admin in dev mode",
        ));
    }
    match admin.login(&req.password) {
        Some(token) => Ok(Json(LoginResponse {
            token,
            expires_in: SESSION_TTL_SECS,
        })),
        None => Err(Problem::unauthorized("invalid admin password")),
    }
}

/// `POST /admin/logout` — invalidate the current admin session (idempotent).
///
/// In dev mode there is no session token, so this is a no-op reporting `logged_out: false`.
pub async fn logout<S>(
    State(state): State<S>,
    ctx: AdminContext,
) -> Json<LogoutResponse>
where
    S: HasAdminAuth + Clone + Send + Sync + 'static,
{
    match ctx.token {
        Some(token) => {
            state.admin_auth().logout(&token);
            Json(LogoutResponse { logged_out: true })
        }
        None => Json(LogoutResponse { logged_out: false }),
    }
}

/// `GET /admin/whoami` — a cheap authorised-admin check for the UI.
///
/// Returns `200 { admin: true, dev_mode }` when the request is an authorised admin, and the
/// usual `401 Problem` (from the [`AdminContext`] extractor) otherwise.
pub async fn whoami(ctx: AdminContext) -> Json<WhoamiResponse> {
    Json(WhoamiResponse {
        admin: true,
        dev_mode: ctx.dev_mode,
    })
}

// --- Internals --------------------------------------------------------------------------

/// Current unix time in seconds.
fn now_unix() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// Constant-time string equality.
///
/// Both inputs are hashed with SHA-256 (fixed 32-byte digests) and the digests compared with
/// no early exit. Hashing first means neither the comparison time nor the loop bound leaks
/// the configured password's length — length would otherwise be a side channel on a raw
/// byte-by-byte compare.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let ha = Sha256::digest(a.as_bytes());
    let hb = Sha256::digest(b.as_bytes());
    let mut diff = 0u8;
    for i in 0..ha.len() {
        diff |= ha[i] ^ hb[i];
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWORD: &str = "correct horse battery staple";

    #[test]
    fn dev_mode_login_returns_none() {
        let admin = AdminAuth::new(None);
        assert!(admin.is_dev_mode());
        // No password configured → login never mints a token, even for an empty string.
        assert!(admin.login("").is_none());
        assert!(admin.login("anything").is_none());
    }

    #[test]
    fn login_with_right_password_mints_valid_token() {
        let admin = AdminAuth::new(Some(PASSWORD.to_string()));
        assert!(!admin.is_dev_mode());
        let token = admin.login(PASSWORD).expect("correct password logs in");
        assert!(!token.is_empty());
        assert!(!token.contains('-'), "token is opaque (no UUID structure)");
        assert!(admin.is_valid(&token));
    }

    #[test]
    fn login_with_wrong_password_returns_none() {
        let admin = AdminAuth::new(Some(PASSWORD.to_string()));
        assert!(admin.login("wrong").is_none());
        assert!(admin.login("").is_none());
        // A near-miss (prefix) is still rejected.
        assert!(admin.login("correct horse battery stapl").is_none());
    }

    #[test]
    fn each_login_mints_a_distinct_token() {
        let admin = AdminAuth::new(Some(PASSWORD.to_string()));
        let a = admin.login(PASSWORD).unwrap();
        let b = admin.login(PASSWORD).unwrap();
        assert_ne!(a, b, "tokens are unique per login");
        assert!(admin.is_valid(&a));
        assert!(admin.is_valid(&b));
    }

    #[test]
    fn unknown_token_is_invalid() {
        let admin = AdminAuth::new(Some(PASSWORD.to_string()));
        assert!(!admin.is_valid("not-a-real-token"));
    }

    #[test]
    fn logout_invalidates_token() {
        let admin = AdminAuth::new(Some(PASSWORD.to_string()));
        let token = admin.login(PASSWORD).unwrap();
        assert!(admin.is_valid(&token));
        admin.logout(&token);
        assert!(!admin.is_valid(&token), "token is dead after logout");
        // Idempotent: logging out an unknown/dead token is a no-op.
        admin.logout(&token);
        admin.logout("never-existed");
    }

    #[test]
    fn expired_token_is_invalid_and_pruned() {
        let admin = AdminAuth::new(Some(PASSWORD.to_string()));
        // Inject a token that expired an hour ago, bypassing the 12h TTL of `login`.
        let stale = "stale-token".to_string();
        {
            let mut sessions = admin.sessions.lock().unwrap();
            sessions.insert(stale.clone(), now_unix() - 3600);
        }
        assert!(!admin.is_valid(&stale), "expired token is rejected");
        // Lazy prune should have removed it from the map.
        assert!(
            !admin.sessions.lock().unwrap().contains_key(&stale),
            "expired token is pruned"
        );
    }

    #[test]
    fn future_expiry_token_is_valid() {
        let admin = AdminAuth::new(Some(PASSWORD.to_string()));
        let token = "future".to_string();
        {
            let mut sessions = admin.sessions.lock().unwrap();
            sessions.insert(token.clone(), now_unix() + 60);
        }
        assert!(admin.is_valid(&token));
    }

    #[test]
    fn constant_time_eq_matches_semantics() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn admin_dev_tenant_is_a_valid_uuidv7() {
        assert!(Uuid::parse(ADMIN_DEV_TENANT).is_ok());
    }
}
