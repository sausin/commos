//! `/admin` — the simple admin console: a single-page app for the people who run the office
//! phones (people, groups, numbers, opening hours, history, system).
//!
//! The page is three static assets (`console/index.html`, `app.css`, `app.js`) compiled into the
//! binary and stitched into one document at first request — no build step, no CDN, no external
//! requests. It talks to the same `/v1` API as everything else; nothing here is a side door.
//!
//! ## Auth
//!
//! `GET /admin` sits behind [`super::console_guard`] like `/dashboard` (HTTP Basic with the admin
//! password; LAN-only in dev mode). Because that check has already proven the password, a
//! configured deployment mints an admin **session** for the page and embeds it in a `<meta>` tag,
//! so the browser never shows a second login. The response is `no-store`. If the page has no
//! embedded session (signed out, or a stale one) its own login form posts to `/admin/login`.

use std::sync::OnceLock;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use commos_core::common::Uuid;

use super::admin::AdminContext;
use super::problem::Problem;
use crate::state::AppState;

const INDEX_HTML: &str = include_str!("console/index.html");
const APP_CSS: &str = include_str!("console/app.css");
const APP_JS: &str = include_str!("console/app.js");

/// Placeholder in `index.html` replaced by the per-request `<meta>` tags.
const META_MARK: &str = "<!--COMMOS_META-->";

/// The page with CSS and JS inlined (built once).
fn page_template() -> &'static str {
    static PAGE: OnceLock<String> = OnceLock::new();
    PAGE.get_or_init(|| {
        INDEX_HTML
            .replace("/*COMMOS_CSS*/", APP_CSS)
            .replace("/*COMMOS_JS*/", APP_JS)
    })
}

/// Escape a value for use inside a double-quoted HTML attribute.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;")
}

/// `GET /admin` — serve the console.
pub async fn page(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let mut meta = format!(
        "<meta name=\"commos-version\" content=\"{}\">\n<meta name=\"commos-dev\" content=\"{}\">",
        esc(env!("COMMOS_VERSION")),
        if st.admin.is_dev_mode() { "1" } else { "0" },
    );
    // The Basic-auth guard has already verified this password; trade it for a session so the
    // page can call `/v1` without a second prompt.
    if let Some(pw) = super::basic_auth_password(&headers) {
        if let Some(token) = st.admin.login(&pw) {
            meta.push_str(&format!("\n<meta name=\"commos-session\" content=\"{}\">", esc(&token)));
        }
    }
    let html = page_template().replacen(META_MARK, &meta, 1);
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8")),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
            (header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")),
            (header::REFERRER_POLICY, HeaderValue::from_static("no-referrer")),
            (
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(
                    "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; \
                     img-src 'self' data:; media-src blob:; connect-src 'self'; \
                     form-action 'none'; base-uri 'none'; frame-ancestors 'none'",
                ),
            ),
        ],
        axum::response::Html(html),
    )
        .into_response()
}

/// `POST /v1/extensions/{id}/sip-password` — mint a fresh SIP password for the extension and
/// return it **once** (`{ "number", "password" }`). The old password stops working immediately;
/// the phone has to be updated (or re-provisioned) with the new one. Admin-only.
pub async fn reset_sip_password(
    State(st): State<AppState>,
    admin: AdminContext,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, Problem> {
    let id = Uuid::parse(&id).map_err(|_| Problem::bad_request("id is not a valid UUIDv7"))?;
    let ext = st
        .store
        .get_extension(admin.tenant_id, id)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?
        .ok_or_else(|| Problem::not_found("no such extension"))?;
    let password = st
        .provisioning
        .reset_credential(admin.tenant_id, &ext.number)
        .await
        .map_err(|e| Problem::internal(e.to_string()))?;
    Ok(Json(json!({ "number": ext.number, "password": password })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::sync::Arc;

    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Method, Request, StatusCode};
    use base64::Engine;
    use tower::ServiceExt;

    use crate::api::admin::AdminAuth;
    use crate::api::auth::AuthConfig;
    use crate::{bus, control, introspect, media, metrics, objectstore, relay, store};

    const PW: &str = "correct horse battery";
    const LAN: &str = "192.168.1.20:5555";
    const PUBLIC: &str = "8.8.8.8:5555";

    /// A fully wired in-memory `AppState` (same wiring as `main::run`, minus the sockets).
    fn state(admin: AdminAuth) -> AppState {
        let recent = introspect::RecentEvents::new();
        let bus = bus::EventBus::new(recent.clone());
        let store: Arc<dyn store::Store> = Arc::new(store::MemStore::new());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<media::MediaFact>();
        let signal = relay::RelaySignal::new();
        let routing = control::routing::Routing::new(
            store.clone(),
            Arc::new(media::LoopbackMedia::new(tx)),
            signal.clone(),
            control::policy::PolicyLimits::default(),
            "1",
        );
        let blob: Arc<dyn objectstore::ObjectStore> = Arc::new(objectstore::LocalObjectStore::new(
            std::env::temp_dir().join(format!("commos-console-test-{}", Uuid::now_v7())),
        ));
        let objects = control::objects::ObjectService::new(blob, store.clone(), signal.clone());
        AppState::new(
            store.clone(),
            routing,
            control::messaging::MessagingService::new(store.clone(), signal.clone()),
            control::realtime::RealtimeService::new(store.clone(), signal.clone()),
            control::queue::QueueService::new(store.clone(), signal.clone()),
            control::ringing::RingingService::new(store.clone(), signal.clone()),
            control::callflow::CallFlowService::new(store.clone(), signal.clone()),
            control::ivr::IvrService::new(store.clone(), signal.clone()),
            control::trunking::TrunkingService::new(store.clone(), signal.clone()),
            control::provisioning::Provisioning::new(store.clone(), signal.clone()),
            control::webhooks::WebhookService::new(store.clone(), signal.clone()),
            objects.clone(),
            control::recordings::RecordingService::new(objects.clone(), store.clone(), signal.clone()),
            control::voicemail::VoicemailService::new(objects.clone(), store.clone(), signal.clone()),
            metrics::Metrics::new(),
            control::agents::AgentRegistry::new(store.clone(), signal.clone()),
            control::registrations::RegistrationRegistry::new(),
            AuthConfig::default(),
            admin,
            "127.0.0.1".parse().unwrap(),
            5060,
            None,
            None,
            None,
            "/tmp/none".into(),
            "memory".into(),
            bus,
            recent,
        )
    }

    fn basic(pw: &str) -> String {
        format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("admin:{pw}")))
    }

    async fn send(
        app: &axum::Router,
        method: Method,
        uri: &str,
        peer: &str,
        auth: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, HeaderMap, String) {
        let mut b = Request::builder().method(method).uri(uri);
        if let Some(a) = auth {
            b = b.header(header::AUTHORIZATION, a);
        }
        let mut req = if let Some(j) = body {
            b.header(header::CONTENT_TYPE, "application/json").body(Body::from(j.to_string())).unwrap()
        } else {
            b.body(Body::empty()).unwrap()
        };
        req.extensions_mut().insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        let resp = app.clone().oneshot(req).await.unwrap();
        let (parts, body) = resp.into_parts();
        let bytes = axum::body::to_bytes(body, 4 * 1024 * 1024).await.unwrap();
        (parts.status, parts.headers, String::from_utf8_lossy(&bytes).into_owned())
    }

    fn session_from(html: &str) -> Option<String> {
        let key = "name=\"commos-session\" content=\"";
        let i = html.find(key)? + key.len();
        Some(html[i..html[i..].find('"')? + i].to_string())
    }

    #[test]
    fn template_inlines_assets_and_has_meta_slot() {
        let t = page_template();
        assert!(t.contains(META_MARK), "meta placeholder survives");
        assert!(!t.contains("/*COMMOS_CSS*/") && !t.contains("/*COMMOS_JS*/"));
        assert!(t.len() < 80 * 1024, "console stays small ({} bytes)", t.len());
        // Self-contained: no external origins to fetch from.
        assert!(!t.contains("src=\"http") && !t.contains("href=\"http"));
        assert!(!t.contains("cdn."), "no CDN assets");
    }

    #[test]
    fn esc_escapes_attribute_breakers() {
        assert_eq!(esc("a\"<b>&"), "a&quot;&lt;b&gt;&amp;");
    }

    #[tokio::test]
    async fn dev_mode_serves_page_to_lan_only() {
        let app = crate::api::router(state(AdminAuth::new(None)));
        let (st, h, body) = send(&app, Method::GET, "/admin", LAN, None, None).await;
        assert_eq!(st, StatusCode::OK);
        assert!(h[header::CONTENT_TYPE].to_str().unwrap().starts_with("text/html"));
        assert_eq!(h[header::CACHE_CONTROL], "no-store");
        assert!(h.contains_key(header::CONTENT_SECURITY_POLICY));
        assert!(body.contains("CommOS Admin"));
        assert!(body.contains("name=\"commos-dev\" content=\"1\""));
        assert!(session_from(&body).is_none(), "no session is minted without a password");
        // The trailing-slash form works too.
        assert_eq!(send(&app, Method::GET, "/admin/", LAN, None, None).await.0, StatusCode::OK);
        // The public internet is refused outright in dev mode.
        assert_eq!(send(&app, Method::GET, "/admin", PUBLIC, None, None).await.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn configured_mode_gates_page_and_embeds_a_working_session() {
        let app = crate::api::router(state(AdminAuth::new(Some(PW.into()))));
        assert_eq!(send(&app, Method::GET, "/admin", LAN, None, None).await.0, StatusCode::UNAUTHORIZED);
        let wrong = basic("nope");
        assert_eq!(send(&app, Method::GET, "/admin", LAN, Some(&wrong), None).await.0, StatusCode::UNAUTHORIZED);

        let ok = basic(PW);
        let (st, _, body) = send(&app, Method::GET, "/admin", PUBLIC, Some(&ok), None).await;
        assert_eq!(st, StatusCode::OK);
        assert!(body.contains("name=\"commos-dev\" content=\"0\""));
        let token = session_from(&body).expect("session embedded for an authenticated page load");

        // The embedded admin session is accepted on tenant-scoped reads (from any peer)...
        let bearer = format!("Bearer admin:{token}");
        let (st, _, body) = send(&app, Method::GET, "/v1/extensions", PUBLIC, Some(&bearer), None).await;
        assert_eq!(st, StatusCode::OK, "{body}");
        // ...and on admin-gated writes, attributed to the same tenant (the write is readable back).
        let (st, _, _) = send(&app, Method::POST, "/v1/users", PUBLIC, Some(&bearer),
            Some(serde_json::json!({"display_name": "Sam"}))).await;
        assert_eq!(st, StatusCode::CREATED);
        let (_, _, users) = send(&app, Method::GET, "/v1/users", PUBLIC, Some(&bearer), None).await;
        assert!(users.contains("Sam"), "{users}");

        // Garbage / absent admin tokens are rejected.
        let bad = "Bearer admin:deadbeef";
        assert_eq!(send(&app, Method::GET, "/v1/extensions", PUBLIC, Some(bad), None).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(send(&app, Method::GET, "/v1/extensions", PUBLIC, None, None).await.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn admin_session_is_invalid_in_dev_mode() {
        let app = crate::api::router(state(AdminAuth::new(None)));
        let (st, _, _) = send(&app, Method::GET, "/v1/extensions", LAN, Some("Bearer admin:anything"), None).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        // The normal dev bearer still works there.
        let dev = format!("Bearer tenant:{}", crate::api::admin::ADMIN_DEV_TENANT);
        assert_eq!(send(&app, Method::GET, "/v1/extensions", LAN, Some(&dev), None).await.0, StatusCode::OK);
    }

    async fn login(app: &axum::Router, pw: &str) -> (StatusCode, String) {
        let (st, _, body) = send(app, Method::POST, "/admin/login", LAN, None, Some(serde_json::json!({"password": pw}))).await;
        let tok = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|j| j["token"].as_str().map(str::to_string))
            .unwrap_or_default();
        (st, tok)
    }

    #[tokio::test]
    async fn password_change_round_trip_updates_file_and_login() {
        let dir = std::env::temp_dir().join(format!("commos-pw-test-{}", Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("admin_password");
        std::fs::write(&file, PW).unwrap();
        let app = crate::api::router(state(AdminAuth::new(Some(PW.into())).with_password_file(Some(file.clone()))));

        let (st, tok) = login(&app, PW).await;
        assert_eq!(st, StatusCode::OK);
        let (_, other) = login(&app, PW).await; // a second session that should be revoked
        let bearer = format!("Bearer admin:{tok}");
        let change = |cur: &str, new: &str| serde_json::json!({"current": cur, "new": new});

        // Wrong current -> 401, short new -> 400, nothing changed.
        let r = send(&app, Method::POST, "/admin/password", LAN, Some(&bearer), Some(change("wrong", "a-new-password"))).await;
        assert_eq!(r.0, StatusCode::UNAUTHORIZED);
        let r = send(&app, Method::POST, "/admin/password", LAN, Some(&bearer), Some(change(PW, "short"))).await;
        assert_eq!(r.0, StatusCode::BAD_REQUEST);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), PW);
        // Unauthenticated -> 401.
        let r = send(&app, Method::POST, "/admin/password", LAN, None, Some(change(PW, "a-new-password"))).await;
        assert_eq!(r.0, StatusCode::UNAUTHORIZED);

        // Success: file rewritten, new password logs in, old one doesn't, other sessions die.
        let r = send(&app, Method::POST, "/admin/password", LAN, Some(&bearer), Some(change(PW, "a-new-password"))).await;
        assert_eq!(r.0, StatusCode::OK, "{}", r.2);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "a-new-password");
        assert_eq!(login(&app, PW).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(login(&app, "a-new-password").await.0, StatusCode::OK);
        assert_eq!(send(&app, Method::GET, "/admin/whoami", LAN, Some(&bearer), None).await.0, StatusCode::OK);
        let revoked = format!("Bearer admin:{other}");
        assert_eq!(send(&app, Method::GET, "/admin/whoami", LAN, Some(&revoked), None).await.0, StatusCode::UNAUTHORIZED);
        // The Basic-auth console guard follows the new password too.
        assert_eq!(send(&app, Method::GET, "/admin", LAN, Some(&basic(PW)), None).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(send(&app, Method::GET, "/admin", LAN, Some(&basic("a-new-password")), None).await.0, StatusCode::OK);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn password_change_is_409_when_managed_externally() {
        // No password file (e.g. an env:// reference): CommOS can't rewrite it.
        let app = crate::api::router(state(AdminAuth::new(Some(PW.into()))));
        let (_, tok) = login(&app, PW).await;
        let bearer = format!("Bearer admin:{tok}");
        let (st, _, body) = send(&app, Method::POST, "/admin/password", LAN, Some(&bearer),
            Some(serde_json::json!({"current": PW, "new": "a-new-password"}))).await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert!(body.contains("managed outside CommOS"), "{body}");
        // Still the old password.
        assert_eq!(login(&app, PW).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn sip_password_reset_returns_a_fresh_secret_once() {
        let app = crate::api::router(state(AdminAuth::new(Some(PW.into()))));
        let (_, tok) = login(&app, PW).await;
        let bearer = format!("Bearer admin:{tok}");
        let (st, _, body) = send(&app, Method::POST, "/v1/extensions", LAN, Some(&bearer),
            Some(serde_json::json!({"number": "105", "destination_ref": "sip:105@commos.local"}))).await;
        assert_eq!(st, StatusCode::CREATED, "{body}");
        let id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"].as_str().unwrap().to_string();
        let uri = format!("/v1/extensions/{id}/sip-password");
        let (st, _, b1) = send(&app, Method::POST, &uri, LAN, Some(&bearer), None).await;
        assert_eq!(st, StatusCode::OK, "{b1}");
        let (_, _, b2) = send(&app, Method::POST, &uri, LAN, Some(&bearer), None).await;
        let p1 = serde_json::from_str::<serde_json::Value>(&b1).unwrap();
        let p2 = serde_json::from_str::<serde_json::Value>(&b2).unwrap();
        assert_eq!(p1["number"], "105");
        assert!(p1["password"].as_str().unwrap().len() >= 12);
        assert_ne!(p1["password"], p2["password"], "each reset mints a new secret");
        // Unknown extension -> 404; no auth -> 401.
        let missing = format!("/v1/extensions/{}/sip-password", Uuid::now_v7());
        assert_eq!(send(&app, Method::POST, &missing, LAN, Some(&bearer), None).await.0, StatusCode::NOT_FOUND);
        assert_eq!(send(&app, Method::POST, &uri, LAN, None, None).await.0, StatusCode::UNAUTHORIZED);
    }
}
