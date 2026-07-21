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
//! We emit a **generic, vendor-neutral** `text/plain` key/value block (INI-style with
//! `#` comments) that a phone or a config converter can consume. Vendor-specific templates
//! (Yealink/Grandstream XML, Polycom, etc.) with their own MIME types are a documented
//! follow-up; the generic form carries everything those templates need: account username,
//! registrar host/port, transport, and display name.

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use commos_core::common::Uuid;
use commos_core::entities::extension::Extension;

use crate::state::AppState;

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
pub async fn provision(State(st): State<AppState>, Path(file): Path<String>) -> Response {
    // 1. Parse the MAC out of the requested filename.
    let mac = match normalize_mac(&file) {
        Some(m) => m,
        None => {
            return text(
                StatusCode::NOT_FOUND,
                format!("not a provisioning request: {file:?} is not a MAC address\n"),
            );
        }
    };

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

    // 3. Registrar the phone should REGISTER against. Read optimistically from AppState;
    //    a real deployment may instead derive these from the request Host header.
    let registrar_host = st.media_ip.to_string();
    let registrar_port = st.sip_port;

    // 4. Render the generic, vendor-neutral config.
    let body = render_config(&mac, &account, &registrar_host, registrar_port);
    text(StatusCode::OK, body)
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

/// Render the generic provisioning block. INI-style `key=value` with `#` comments — a shape
/// most phones and config converters accept, carrying everything a vendor template needs.
fn render_config(mac: &str, account: &Extension, registrar_host: &str, registrar_port: u16) -> String {
    // Display name: the extension's own label is the MAC (the binding key), so it is a poor
    // human name; fall back to "Ext <number>".
    let display_name = format!("Ext {}", account.number);
    format!(
        "# CommOS auto-provisioning config\n\
         # Generated for device MAC {mac}\n\
         # Generic, vendor-neutral form. Vendor templates (Yealink/Grandstream XML, etc.)\n\
         # are a documented follow-up; a config converter can map these keys as needed.\n\
         \n\
         [account]\n\
         # SIP username / auth identity — the Extension number bound to this device.\n\
         username={number}\n\
         auth_user={number}\n\
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
        mac = mac,
        number = account.number,
        display_name = display_name,
        registrar_host = registrar_host,
        registrar_port = registrar_port,
    )
}

/// Build a `text/plain; charset=utf-8` response. A real deployment negotiates vendor MIME
/// types; the reference form is always plain text.
fn text(status: StatusCode, body: String) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body,
    )
        .into_response()
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
}
