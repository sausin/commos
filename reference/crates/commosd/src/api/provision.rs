//! `/provision` — phone auto-provisioning (the URL DHCP option 66 points at).
//!
//! The onboarding wizard tells the operator to set DHCP option 66 to
//! `http://<host>:<port>/provision`. A freshly-booted phone appends its own identity
//! (`/provision/<mac>.cfg`) and fetches a SIP account it can register with — zero-touch
//! provisioning. This handler is the server side of that exchange.
//!
//! It is deliberately **unauthenticated**: a cold phone has no bearer token to present,
//! so it lives outside the `/v1` contract surface next to `/dashboard` and `/onboarding`.
//! A real deployment authenticates the request some other way (MACs are guessable) —
//! typically by mapping the requesting subnet/VLAN to a tenant and pinning the expected
//! MAC — but the reference implementation keeps it simple and single-tenant.
//!
//! ## Device ↔ Extension binding
//! The onboarding "apply" step binds a physical device to a SIP account by writing the
//! device's MAC (12 lowercase hex, no separators) into the **Extension's `label`**. So the
//! lookup here is: normalise the requested MAC, then find the Extension whose `label ==
//! mac` — that Extension *is* the phone's account (its `number` is the auth/username).
//!
//! ## Config format
//! The emitted config **format and Content-Type are vendor-specific**, keyed off the bound
//! `Device.vendor_key` (a lowercase OUI-derived key like `"yealink"`, `"grandstream"`,
//! `"polycom"`, `"unknown"`, set by onboarding). We render:
//!   * `yealink`     → Yealink-style `account.1.*` `.cfg`,
//!   * `grandstream` → a readable Grandstream `P`-value / parameter block,
//!   * anything else (polycom, unknown, no device) → the original generic vendor-neutral
//!     INI block as the fallback.
//!
//! All three are served as `text/plain; charset=utf-8` for the reference implementation.
//!
//! Vendor detection prefers the bound `Device.vendor_key`, and falls back to the requesting
//! phone's `User-Agent` when that key is missing or `unknown` (e.g. an OUI not yet in the
//! onboarding table) — so a handset still gets its native config rather than the generic block.
//! A real fleet may additionally negotiate the `Accept`/MIME type and hand back vendor-native
//! MIME types; this reference keeps the body as plain text.
//!
//! ## Secrets
//! The SIP password is a placeholder (`CHANGEME`): per-device credentials are not stored yet.
//! Real per-device secrets (and their rotation) come from Volume 9.

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use commos_core::common::Uuid;
use commos_core::entities::device::Device;
use commos_core::entities::extension::Extension;

use crate::state::AppState;

/// Placeholder SIP secret. Per-device credentials are not persisted yet; real secrets come
/// from Volume 9. Phones provisioned with this must have it overwritten before registering.
const PLACEHOLDER_SECRET: &str = "CHANGEME";

/// The well-known dev tenant provisioning is scoped to. Single-tenant simplification: a
/// real deployment maps the requesting subnet/VLAN (or a signed request) to a tenant rather
/// than hard-coding one. Matches `SIP_DEFAULT_TENANT` / the dashboard's dev token.
const DEV_TENANT: &str = "01920000-0000-7000-8000-000000000001";

/// How many extensions to pull per page while scanning for the MAC binding.
const PAGE_SIZE: usize = 200;

/// `GET /provision/:file` — serve a phone its SIP account config.
///
/// `file` is `<mac>.cfg` (or bare `<mac>`); the MAC may carry `:`/`-`/`.` separators and any
/// case. We normalise it to 12 lowercase hex chars, find the Extension bound to it, and
/// return a generic provisioning block. Panic-free: bad input → 404, store error → 500,
/// both as plain text a phone's log can surface.
pub async fn provision(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(file): Path<String>,
) -> Response {
    // The phone identifies itself in the request User-Agent (e.g. "Grandstream GXP2170 …",
    // "Yealink SIP-T46S …"). We use it both to log which handset asked and as a vendor fallback
    // when the bound Device's stored vendor is unknown (e.g. an OUI not yet in the table).
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // 1. Parse the MAC out of the requested filename.
    let mac = match normalize_mac(&file) {
        Some(m) => m,
        None => {
            // Benign (favicon probes, path scans) but cheap to see; keep it at debug so it does
            // not drown the log, while still being available when chasing a provisioning issue.
            tracing::debug!(file = %file, "provisioning request ignored: filename is not a MAC address");
            return text(
                StatusCode::NOT_FOUND,
                format!("not a provisioning request: {file:?} is not a MAC address\n"),
            );
        }
    };
    tracing::debug!(mac = %mac, "provisioning request received");

    let tenant = match Uuid::parse(DEV_TENANT) {
        Ok(t) => t,
        // Compile-time-known constant; treat any parse failure defensively, never panic.
        Err(_) => {
            return text(
                StatusCode::INTERNAL_SERVER_ERROR,
                "provisioning misconfigured: invalid dev tenant\n".to_string(),
            );
        }
    };

    // 2. Find the Extension whose label == mac (the device binding written by onboarding).
    let account = match find_account(&st, tenant, &mac).await {
        Ok(Some(ext)) => ext,
        Ok(None) => {
            // A phone asked for a config but no extension is bound to its MAC. This is the most
            // common provisioning failure — and it used to be invisible: the phone got a 404 and
            // the daemon logged nothing. Warn (with the MAC) so it shows up in `journalctl -u
            // commosd`, and point at the fix (align it in onboarding, or set the extension label).
            tracing::warn!(
                mac = %mac,
                user_agent = ?user_agent,
                "provisioning: no extension is bound to this MAC — the phone cannot auto-provision. \
                 Align it in /onboarding (its number must be one that gets created), or set that \
                 extension's label to the MAC. Returning 404."
            );
            return text(
                StatusCode::NOT_FOUND,
                format!("no account provisioned for {mac}\n"),
            );
        }
        Err(e) => {
            // Do not leak store internals to an unauthenticated caller.
            tracing::warn!(error = %e, mac = %mac, "provisioning lookup failed");
            return text(
                StatusCode::INTERNAL_SERVER_ERROR,
                "provisioning temporarily unavailable\n".to_string(),
            );
        }
    };

    // 3. Look up the Device bound to this MAC to learn its vendor and which person it belongs to.
    //    Absent device (or store error) is non-fatal: fall back to the generic vendor-neutral form
    //    rather than fail the phone. If the device is missing we simply treat the vendor as generic.
    let device = match find_device(&st, tenant, &mac).await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, mac = %mac, "device lookup failed; using generic config");
            None
        }
    };
    // Resolve the vendor: the bound Device's stored `vendor_key` first, but fall back to the
    // request's User-Agent when that is missing or `unknown`. This is what makes a Grandstream
    // whose OUI wasn't recognised at onboarding still get its native config instead of the
    // generic INI block (which no desk phone can auto-provision from).
    let stored_vendor = device
        .as_ref()
        .map(|d| d.vendor_key.clone())
        .filter(|v| !v.trim().is_empty() && v != "unknown");
    let vendor = stored_vendor
        .or_else(|| {
            user_agent
                .as_deref()
                .and_then(vendor_from_user_agent)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".to_string());

    // The display name shown on the phone's LCD: the assigned person's name (set from the
    // operator's onboarding choice), falling back to `Ext <number>` when there is none.
    let display_name = resolve_display_name(&st, tenant, device.as_ref(), &account.number).await;

    // 4. Registrar the phone should REGISTER against. Read optimistically from AppState;
    //    a real deployment may instead derive these from the request Host header.
    let registrar_host = st.media_ip.to_string();
    let registrar_port = st.sip_port;

    // 5. The phone's SIP secret — the real per-device credential minted during provisioning.
    //    Falls back to the placeholder only if none was generated (e.g. a hand-made extension).
    let secret = st
        .store
        .get_sip_credential(tenant, &account.number)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| PLACEHOLDER_SECRET.to_string());

    // 6. Render the vendor-specific config (format + Content-Type depend on the vendor).
    let (content_type, body) = render(
        &vendor,
        &account.number,
        &display_name,
        &secret,
        &registrar_host,
        registrar_port,
    );
    tracing::info!(
        mac = %mac,
        extension = %account.number,
        vendor = %vendor,
        registrar = %registrar_host,
        user_agent = ?user_agent,
        "provisioning: served {vendor} config for {mac} → extension {}",
        account.number
    );
    text_with(StatusCode::OK, content_type, body)
}

/// Scan the tenant's devices for the one whose `mac == mac`.
///
/// Devices are paged; we stop at the first MAC match. Returns `Ok(None)` when no device
/// carries this MAC (the binding may exist only on the Extension). Store errors are
/// propagated so the caller can decide (here: fall back to generic).
async fn find_device(
    st: &AppState,
    tenant: Uuid,
    mac: &str,
) -> Result<Option<Device>, crate::store::StoreError> {
    let mut cursor: Option<String> = None;
    loop {
        let page: crate::store::Page<Device> =
            st.store.list_devices(tenant, PAGE_SIZE, cursor).await?;
        if let Some(dev) = page
            .items
            .into_iter()
            .find(|d| d.mac.as_deref() == Some(mac))
        {
            return Ok(Some(dev));
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(None),
        }
    }
}

/// The human display name for the phone's LCD. Prefers the assigned person's name (set during
/// onboarding from the operator's choice); falls back to `Ext <number>`. A missing user or store
/// error is non-fatal — the phone still provisions with the default name.
async fn resolve_display_name(
    st: &AppState,
    tenant: Uuid,
    device: Option<&Device>,
    ext_number: &str,
) -> String {
    let fallback = || format!("Ext {ext_number}");
    let Some(user_id) = device.and_then(|d| d.assigned_user_id) else {
        return fallback();
    };
    match st.store.get_user(tenant, user_id).await {
        Ok(Some(u)) if !u.display_name.trim().is_empty() => u.display_name,
        Ok(_) => fallback(),
        Err(e) => {
            tracing::warn!(error = %e, mac_user = %user_id, "display-name lookup failed; using default");
            fallback()
        }
    }
}

/// Scan the tenant's extensions for the one bound to `mac` (its `label`).
///
/// Extensions are paged; we stop at the first match. Returns `Ok(None)` if no extension
/// carries this MAC. Any store error is propagated so the caller can answer 500.
async fn find_account(
    st: &AppState,
    tenant: Uuid,
    mac: &str,
) -> Result<Option<Extension>, crate::store::StoreError> {
    let mut cursor: Option<String> = None;
    loop {
        let page = st.store.list_extensions(tenant, PAGE_SIZE, cursor).await?;
        if let Some(ext) = page
            .items
            .into_iter()
            .find(|e| e.label.as_deref() == Some(mac))
        {
            return Ok(Some(ext));
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(None),
        }
    }
}

/// Render a provisioning config for `vendor`, returning `(content_type, body)`.
///
/// Data-driven: the `match` on `vendor` (a lowercase `Device.vendor_key`) selects the format.
/// Known vendors get their native shape; `polycom`, `unknown`, and anything unrecognised fall
/// through to the generic vendor-neutral INI block. All arms currently return
/// `text/plain; charset=utf-8`, but the return carries the type so a real deployment can hand
/// back vendor-native MIME types (e.g. Yealink `.cfg`, Grandstream XML) without touching the
/// call site.
///
/// The SIP password is the `CHANGEME` placeholder — per-device secrets are not stored yet and
/// arrive in Volume 9.
fn render(
    vendor: &str,
    ext_number: &str,
    display_name: &str,
    secret: &str,
    registrar: &str,
    port: u16,
) -> (String, String) {
    let content_type = "text/plain; charset=utf-8".to_string();
    let body = match vendor {
        "yealink" => render_yealink(ext_number, display_name, secret, registrar, port),
        "grandstream" => render_grandstream(ext_number, display_name, secret, registrar, port),
        // polycom / unknown / no device / anything else → generic fallback.
        _ => render_generic(ext_number, display_name, secret, registrar, port),
    };
    (content_type, body)
}

/// Yealink-style `.cfg` (`account.1.*` dotted keys). Yealink phones consume this format
/// directly. `account.1.label` / `display_name` are what the handset shows on its LCD.
/// Signature line: `account.1.sip_server.1.address`.
fn render_yealink(ext_number: &str, display_name: &str, secret: &str, registrar: &str, port: u16) -> String {
    format!(
        "#!version:1.0.0.1\n\
         # CommOS auto-provisioning config (Yealink)\n\
         account.1.enable = 1\n\
         account.1.label = {display_name}\n\
         account.1.display_name = {display_name}\n\
         account.1.user_name = {number}\n\
         account.1.auth_name = {number}\n\
         account.1.password = {secret}\n\
         account.1.sip_server.1.address = {registrar}\n\
         account.1.sip_server.1.port = {port}\n\
         account.1.sip_server.1.transport_type = 0\n\
         account.1.sip_server.1.expires = 3600\n",
        number = ext_number,
        display_name = display_name,
        secret = secret,
        registrar = registrar,
        port = port,
    )
}

/// Grandstream config — the **plain-text P-value file** a GXP/GRP/DP/HT handset actually
/// auto-provisions from: `Pxxx = value` lines with `#` comments. (The previous readable
/// `account.1.*` block was documentation, not a config the phone consumes — a phone fed that
/// would fetch it and never register.) The account-1 P-codes are stable across the current GXP/
/// GRP families: `P271` account active, `P270` account name (LCD label), `P3` display name
/// (caller-ID), `P35` SIP User ID, `P36` Authenticate ID, `P34` Authenticate Password, `P47`
/// SIP Server (the server port rides here as `host:port` — there is no separate per-account
/// server-port code; `P40` is the phone's own local SIP port), `P130` SIP transport (`0` = UDP),
/// and `P32` register expiration in **minutes**. Signature line: `P47 = <registrar>`.
fn render_grandstream(ext_number: &str, display_name: &str, secret: &str, registrar: &str, port: u16) -> String {
    // The SIP server field carries a non-default port as `host:port`; a standard 5060 is left
    // bare so it matches what operators expect to see.
    let sip_server = if port == 5060 { registrar.to_string() } else { format!("{registrar}:{port}") };
    format!(
        "# CommOS auto-provisioning config (Grandstream, plain-text P-values)\n\
         P271 = 1\n\
         P270 = {display_name}\n\
         P3 = {display_name}\n\
         P35 = {number}\n\
         P36 = {number}\n\
         P34 = {secret}\n\
         P47 = {sip_server}\n\
         P130 = 0\n\
         P32 = 60\n",
        display_name = display_name,
        number = ext_number,
        secret = secret,
        sip_server = sip_server,
    )
}

/// The original generic, vendor-neutral INI block — the fallback for polycom / unknown / no
/// device. INI-style `key=value` with `#` comments, a shape most phones and config converters
/// accept. Signature line: `[account]`.
fn render_generic(ext_number: &str, display_name: &str, secret: &str, registrar_host: &str, registrar_port: u16) -> String {
    format!(
        "# CommOS auto-provisioning config\n\
         # Generic, vendor-neutral form. Vendor-specific formats (Yealink/Grandstream) are\n\
         # served when the bound Device's vendor_key is known; a converter can map these keys.\n\
         \n\
         [account]\n\
         # SIP username / auth identity — the Extension number bound to this device.\n\
         username={number}\n\
         auth_user={number}\n\
         password={secret}\n\
         display_name={display_name}\n\
         \n\
         [sip]\n\
         # Registrar / outbound proxy the phone REGISTERs against.\n\
         registrar={registrar_host}\n\
         port={registrar_port}\n\
         transport=UDP\n\
         # Re-registration interval (seconds).\n\
         register_expiry=3600\n\
         enabled=1\n",
        number = ext_number,
        secret = secret,
        display_name = display_name,
        registrar_host = registrar_host,
        registrar_port = registrar_port,
    )
}

/// Build a `text/plain; charset=utf-8` response. Used for all error paths.
fn text(status: StatusCode, body: String) -> Response {
    text_with(status, "text/plain; charset=utf-8".to_string(), body)
}

/// Build a response with an explicit Content-Type. A real deployment negotiates vendor MIME
/// types here; the reference form is always plain text.
fn text_with(status: StatusCode, content_type: String, body: String) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, content_type)],
        body,
    )
        .into_response()
}

/// Best-effort vendor detection from a phone's HTTP `User-Agent`. Desk phones fetch their config
/// with a vendor-identifying UA (e.g. `Grandstream GXP2170 1.0.11.34`, `Yealink SIP-T46S …`), so
/// this recovers the vendor when the OUI wasn't recognised at onboarding. Returns a lowercase
/// `vendor_key` matching what [`render`] dispatches on, or `None` when unrecognised.
fn vendor_from_user_agent(ua: &str) -> Option<&'static str> {
    let ua = ua.to_ascii_lowercase();
    if ua.contains("yealink") {
        Some("yealink")
    } else if ua.contains("grandstream") {
        Some("grandstream")
    } else if ua.contains("fanvil") {
        Some("fanvil")
    } else if ua.contains("polycom") || ua.contains("poly") {
        Some("polycom")
    } else {
        None
    }
}

/// Normalise a provisioning filename to a canonical MAC: strip a trailing `.cfg`, lowercase,
/// drop `:`/`-`/`.` separators, and require exactly 12 hex chars. Returns `None` for anything
/// that is not a MAC (so the handler can 404).
fn normalize_mac(s: &str) -> Option<String> {
    // Strip a trailing ".cfg" (case-insensitive) if present.
    let stem = {
        let lower = s.to_ascii_lowercase();
        match lower.strip_suffix(".cfg") {
            Some(rest) => rest.to_string(),
            None => lower,
        }
    };
    // Remove the accepted MAC separators.
    let cleaned: String = stem
        .chars()
        .filter(|c| *c != ':' && *c != '-' && *c != '.')
        .collect();
    // Must be exactly 12 hex digits.
    if cleaned.len() == 12 && cleaned.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(cleaned)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_colon_separated_with_cfg() {
        assert_eq!(
            normalize_mac("AA:BB:CC:DD:EE:FF.cfg").as_deref(),
            Some("aabbccddeeff")
        );
    }

    #[test]
    fn normalizes_hyphen_and_dot_separators() {
        assert_eq!(normalize_mac("aa-bb-cc-dd-ee-ff").as_deref(), Some("aabbccddeeff"));
        assert_eq!(normalize_mac("aabb.ccdd.eeff").as_deref(), Some("aabbccddeeff"));
    }

    #[test]
    fn normalizes_bare_lowercase_no_separators() {
        assert_eq!(normalize_mac("0123456789ab").as_deref(), Some("0123456789ab"));
    }

    #[test]
    fn uppercase_is_lowercased() {
        assert_eq!(normalize_mac("0123456789AB.cfg").as_deref(), Some("0123456789ab"));
    }

    #[test]
    fn junk_is_rejected() {
        assert_eq!(normalize_mac("hello.cfg"), None);
        assert_eq!(normalize_mac("not-a-mac"), None);
        assert_eq!(normalize_mac(""), None);
    }

    #[test]
    fn wrong_length_is_rejected() {
        // 10 hex chars — too short.
        assert_eq!(normalize_mac("aabbccddee"), None);
        // 14 hex chars — too long.
        assert_eq!(normalize_mac("aabbccddeeff00"), None);
    }

    #[test]
    fn non_hex_of_right_length_is_rejected() {
        // 12 chars but 'g'/'z' are not hex.
        assert_eq!(normalize_mac("aabbccddeegg"), None);
        assert_eq!(normalize_mac("zzzzzzzzzzzz"), None);
    }

    #[test]
    fn render_yealink_has_signature_and_account() {
        let (ct, body) = render("yealink", "1001", "Front Desk", "s3cr3t", "10.0.0.5", 5060);
        assert_eq!(ct, "text/plain; charset=utf-8");
        // Yealink signature line.
        assert!(body.contains("account.1.sip_server.1.address = 10.0.0.5"));
        assert!(body.contains("account.1.user_name = 1001"));
        assert!(body.contains("account.1.sip_server.1.port = 5060"));
        // The real per-device secret is served, not a placeholder.
        assert!(body.contains("account.1.password = s3cr3t"));
        // The operator's display name lands on the LCD fields.
        assert!(body.contains("account.1.label = Front Desk"));
        assert!(body.contains("account.1.display_name = Front Desk"));
    }

    #[test]
    fn render_grandstream_emits_real_pvalues() {
        let (ct, body) = render("grandstream", "1002", "Room 101", "s3cr3t", "10.0.0.6", 5060);
        assert_eq!(ct, "text/plain; charset=utf-8");
        // Real Grandstream plain-text P-values the handset consumes — not the old readable block.
        assert!(body.contains("P271 = 1")); // account active
        assert!(body.contains("P35 = 1002")); // SIP User ID
        assert!(body.contains("P36 = 1002")); // Authenticate ID
        assert!(body.contains("P34 = s3cr3t")); // Authenticate Password
        assert!(body.contains("P47 = 10.0.0.6")); // SIP Server (signature line)
        assert!(body.contains("P270 = Room 101")); // account name (LCD label)
        assert!(body.contains("P3 = Room 101")); // display name
        // A standard 5060 is left bare (no host:port) on P47.
        assert!(!body.contains("P47 = 10.0.0.6:5060"));
        // Must not regress to the old, non-consumable account.1.* block.
        assert!(!body.contains("account.1."));
    }

    #[test]
    fn render_grandstream_appends_nonstandard_port_to_p47() {
        let (_ct, body) = render("grandstream", "1002", "Room 101", "s3cr3t", "10.0.0.6", 5070);
        // A non-default SIP port rides on the server field as host:port.
        assert!(body.contains("P47 = 10.0.0.6:5070"));
    }

    #[test]
    fn vendor_detected_from_user_agent() {
        assert_eq!(vendor_from_user_agent("Grandstream GXP2170 1.0.11.34"), Some("grandstream"));
        assert_eq!(vendor_from_user_agent("Yealink SIP-T46S 66.85.0.5"), Some("yealink"));
        assert_eq!(vendor_from_user_agent("Fanvil X4"), Some("fanvil"));
        assert_eq!(vendor_from_user_agent("curl/8.0"), None);
    }

    #[test]
    fn render_polycom_falls_back_to_generic() {
        let (ct, body) = render("polycom", "1003", "Ext 1003", "s3cr3t", "10.0.0.7", 5060);
        assert_eq!(ct, "text/plain; charset=utf-8");
        // Generic signature: INI [account] section, not vendor-specific keys.
        assert!(body.contains("[account]"));
        assert!(body.contains("username=1003"));
        assert!(body.contains("password=s3cr3t"));
        assert!(!body.contains("account.1."));
    }

    #[test]
    fn render_unknown_vendor_falls_back_to_generic() {
        let (_ct, body) = render("unknown", "1004", "Warehouse", "s3cr3t", "10.0.0.8", 5060);
        assert!(body.contains("[account]"));
        assert!(body.contains("registrar=10.0.0.8"));
        // Display name flows into the generic block too.
        assert!(body.contains("display_name=Warehouse"));
    }
}
