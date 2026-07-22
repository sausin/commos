//! `/provision` — phone auto-provisioning (the URL DHCP option 66 points at).
//!
//! The onboarding wizard tells the operator to set DHCP option 66 to
//! `http://<host>:<port>/provision/` (trailing slash so the phone joins the filename cleanly).
//! A freshly-booted phone appends its **own** filename — Grandstream fetches `cfg<mac>` and
//! `cfg<mac>.xml`, Yealink and the generic convention `<mac>.cfg` — and gets a SIP account it
//! can register with, zero-touch. DHCP option 67 is deliberately not used: phones derive the
//! filename themselves, and dnsmasq cannot expand a `{mac}` macro into option 67 anyway. This
//! handler accepts every one of those filename shapes ([`parse_provision_target`]).
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

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Path, State};
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
/// `file` is any filename a phone asks for — `<mac>.cfg`, bare `<mac>`, or Grandstream's
/// `cfg<mac>` / `cfg<mac>.xml` / `cfg<mac>.cfg`; the MAC may carry `:`/`-`/`.` separators and any
/// case. We parse out the MAC (and whether the XML form was requested), find the Extension bound
/// to it, and return the vendor's config. Panic-free: bad input → 404, store error → 500, both
/// as plain text a phone's log can surface.
pub async fn provision(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(file): Path<String>,
) -> Response {
    // 0. Phone auto-provisioning hands back the extension's *real* SIP secret in cleartext, so
    //    it must never be reachable from the public internet — a guessed MAC would otherwise
    //    leak a working credential (registration hijack / toll fraud). Phones provision from the
    //    LAN, so restrict this to trusted (loopback/private) peers. An unknown peer fails closed.
    if !crate::api::peer::is_trusted_ip(&peer.ip()) {
        return text(
            StatusCode::FORBIDDEN,
            "provisioning is restricted to the local/private network\n".to_string(),
        );
    }

    // The phone identifies itself in the request User-Agent (e.g. "Grandstream GXP2170 …",
    // "Yealink SIP-T46S …"). We use it both to log which handset asked and as a vendor fallback
    // when the bound Device's stored vendor is unknown (e.g. an OUI not yet in the table).
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // 1. Parse the MAC (and requested format) out of the requested filename. Phones name their
    //    own config file — Grandstream fetches `cfg<mac>` / `cfg<mac>.xml`, Yealink and others
    //    `<mac>.cfg` — so we accept every one of those shapes rather than a single fixed name.
    let req = match parse_provision_target(&file) {
        Some(r) => r,
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
    let mac = req.mac;
    tracing::debug!(mac = %mac, wants_xml = req.wants_xml, "provisioning request received");

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

    // 5b. The NTP time server the phone should sync its clock from. We point it at the CommOS
    //     host itself so a freshly-provisioned handset shows the right time (and stamps call
    //     history / logs correctly) with no extra operator step — the host just needs an NTP
    //     service reachable there (e.g. chrony with `allow`).
    let ntp_server = registrar_host.clone();

    // 6. Render the vendor-specific config. Format, body, and Content-Type depend on the vendor
    //    and on the filename the phone asked for (Grandstream's `cfg<mac>.xml` wants XML).
    let (content_type, body) = render(
        &vendor,
        req.wants_xml,
        &mac,
        &account.number,
        &display_name,
        &secret,
        &registrar_host,
        registrar_port,
        &ntp_server,
    );
    tracing::info!(
        mac = %mac,
        extension = %account.number,
        vendor = %vendor,
        registrar = %registrar_host,
        wants_xml = req.wants_xml,
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
/// through to the generic vendor-neutral INI block. `wants_xml` (set when the phone asked for a
/// `.xml` file) picks Grandstream's XML document over its plain-text P-value form; `mac` is
/// embedded in that XML's `<mac>` element so the handset can validate it.
///
/// The return carries the Content-Type so each vendor form is served as the phone expects —
/// plain text for the P-value / INI shapes, `text/xml` for the Grandstream XML document.
///
/// The SIP password is the real per-device secret resolved at delivery (`CHANGEME` placeholder
/// only when none was minted); `ntp_server` is the time source the phone syncs its clock from.
#[allow(clippy::too_many_arguments)]
fn render(
    vendor: &str,
    wants_xml: bool,
    mac: &str,
    ext_number: &str,
    display_name: &str,
    secret: &str,
    registrar: &str,
    port: u16,
    ntp_server: &str,
) -> (String, String) {
    let plain = "text/plain; charset=utf-8".to_string();
    match vendor {
        "yealink" => (plain, render_yealink(ext_number, display_name, secret, registrar, port, ntp_server)),
        // Grandstream fetches `cfg<mac>.xml` (XML) and `cfg<mac>` (plain-text P-values); serve the
        // shape it asked for so both the modern (XML) and legacy (plain P-value) paths register.
        "grandstream" if wants_xml => (
            "text/xml; charset=utf-8".to_string(),
            render_grandstream_xml(mac, ext_number, display_name, secret, registrar, port, ntp_server),
        ),
        "grandstream" => (
            plain,
            render_grandstream(ext_number, display_name, secret, registrar, port, ntp_server),
        ),
        // polycom / unknown / no device / anything else → generic fallback.
        _ => (plain, render_generic(ext_number, display_name, secret, registrar, port, ntp_server)),
    }
}

/// Yealink-style `.cfg` (`account.1.*` dotted keys). Yealink phones consume this format
/// directly. `account.1.label` / `display_name` are what the handset shows on its LCD.
/// `local_time.ntp_server1` points the phone's clock at the CommOS host. Signature line:
/// `account.1.sip_server.1.address`.
fn render_yealink(ext_number: &str, display_name: &str, secret: &str, registrar: &str, port: u16, ntp_server: &str) -> String {
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
         account.1.sip_server.1.expires = 3600\n\
         # Sync the clock from the CommOS host.\n\
         local_time.ntp_server1 = {ntp}\n",
        number = ext_number,
        display_name = display_name,
        secret = secret,
        registrar = registrar,
        port = port,
        ntp = ntp_server,
    )
}

/// The account-1 (plus time-server) P-values a GXP/GRP/DP/HT handset provisions from, shared by
/// the plain-text and XML Grandstream renderers so the two forms can never drift. The P-codes are
/// stable across the current GXP/GRP families: `P271` account active, `P270` account name (LCD
/// label), `P3` display name (caller-ID), `P35` SIP User ID, `P36` Authenticate ID, `P34`
/// Authenticate Password, `P47` SIP Server (a non-default server port rides here as `host:port` —
/// there is no separate per-account server-port code; `P40` is the phone's own local SIP port),
/// `P130` SIP transport (`0` = UDP), `P32` register expiration in **minutes**, and `P30` the NTP
/// time server (so the phone picks its clock up from CommOS).
fn grandstream_pvalues(
    ext_number: &str,
    display_name: &str,
    secret: &str,
    registrar: &str,
    port: u16,
    ntp_server: &str,
) -> Vec<(&'static str, String)> {
    let sip_server = if port == 5060 { registrar.to_string() } else { format!("{registrar}:{port}") };
    vec![
        ("P271", "1".to_string()),
        ("P270", display_name.to_string()),
        ("P3", display_name.to_string()),
        ("P35", ext_number.to_string()),
        ("P36", ext_number.to_string()),
        ("P34", secret.to_string()),
        ("P47", sip_server),
        ("P130", "0".to_string()),
        ("P32", "60".to_string()),
        ("P30", ntp_server.to_string()),
    ]
}

/// Grandstream **plain-text P-value file** — `Pxxx = value` lines with `#` comments, the form a
/// handset consumes from the no-extension `cfg<mac>` (and `cfg<mac>.cfg`) request. Signature line:
/// `P47 = <registrar>`.
fn render_grandstream(ext_number: &str, display_name: &str, secret: &str, registrar: &str, port: u16, ntp_server: &str) -> String {
    let mut out = String::from("# CommOS auto-provisioning config (Grandstream, plain-text P-values)\n");
    for (code, value) in grandstream_pvalues(ext_number, display_name, secret, registrar, port, ntp_server) {
        out.push_str(&format!("{code} = {value}\n"));
    }
    out
}

/// Grandstream **XML config document** — the `<gs_provision>` form newer GXP/GRP firmware fetches
/// as `cfg<mac>.xml`. Same P-values as the plain-text form, wrapped per Grandstream's XML
/// provisioning schema. The `<mac>` element lets the handset validate the file is meant for it;
/// values are XML-escaped. Signature line: `<P47>`.
fn render_grandstream_xml(mac: &str, ext_number: &str, display_name: &str, secret: &str, registrar: &str, port: u16, ntp_server: &str) -> String {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n");
    out.push_str("<!-- CommOS auto-provisioning config (Grandstream XML) -->\n");
    out.push_str("<gs_provision version=\"1\">\n");
    out.push_str(&format!("  <mac>{}</mac>\n", xml_escape(mac)));
    out.push_str("  <config version=\"1\">\n");
    for (code, value) in grandstream_pvalues(ext_number, display_name, secret, registrar, port, ntp_server) {
        out.push_str(&format!("    <{code}>{}</{code}>\n", xml_escape(&value)));
    }
    out.push_str("  </config>\n");
    out.push_str("</gs_provision>\n");
    out
}

/// Minimal XML text escaping for P-value content (display names, secrets) placed between tags.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The original generic, vendor-neutral INI block — the fallback for polycom / unknown / no
/// device. INI-style `key=value` with `#` comments, a shape most phones and config converters
/// accept. Signature line: `[account]`.
fn render_generic(ext_number: &str, display_name: &str, secret: &str, registrar_host: &str, registrar_port: u16, ntp_server: &str) -> String {
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
         enabled=1\n\
         \n\
         [time]\n\
         # NTP time source — the CommOS host, so the phone's clock is correct.\n\
         ntp_server={ntp}\n",
        number = ext_number,
        secret = secret,
        display_name = display_name,
        registrar_host = registrar_host,
        registrar_port = registrar_port,
        ntp = ntp_server,
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

/// A parsed provisioning request: the device MAC and whether the phone asked for the XML form.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ProvisionRequest {
    /// The MAC, normalised to 12 lowercase hex chars, no separators.
    mac: String,
    /// True when the requested filename ended in `.xml` (Grandstream's XML config form).
    wants_xml: bool,
}

/// Parse a provisioning filename into the MAC it addresses and the format the phone wants.
///
/// Phones name their own config file, so we accept every shape they ask for:
///   * `<mac>.cfg` / bare `<mac>` — Yealink and the generic convention,
///   * `cfg<mac>` / `cfg<mac>.cfg` / `cfg<mac>.xml` — Grandstream (it prefixes `cfg` and, on
///     newer firmware, appends `.xml`).
///
/// The MAC may carry `:`/`-`/`.` separators and any case. A trailing `.xml` selects the XML form;
/// the leading `cfg` is Grandstream's prefix — and since a valid 12-hex MAC can never begin with
/// the literal `cfg` (`g` is not a hex digit), stripping it is unambiguous. Returns `None` for
/// anything that is not a MAC (so the handler can 404).
fn parse_provision_target(s: &str) -> Option<ProvisionRequest> {
    let lower = s.to_ascii_lowercase();
    let wants_xml = lower.ends_with(".xml");
    // Strip a trailing extension the phone appends (`.xml` or `.cfg`).
    let stem = lower
        .strip_suffix(".xml")
        .or_else(|| lower.strip_suffix(".cfg"))
        .unwrap_or(&lower);
    // Strip Grandstream's leading `cfg` prefix when present (unambiguous: `g` is not hex).
    let stem = stem.strip_prefix("cfg").unwrap_or(stem);
    // Remove the accepted MAC separators.
    let cleaned: String = stem
        .chars()
        .filter(|c| *c != ':' && *c != '-' && *c != '.')
        .collect();
    // Must be exactly 12 hex digits.
    if cleaned.len() == 12 && cleaned.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(ProvisionRequest { mac: cleaned, wants_xml })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_colon_separated_with_cfg() {
        let r = parse_provision_target("AA:BB:CC:DD:EE:FF.cfg").unwrap();
        assert_eq!(r.mac, "aabbccddeeff");
        assert!(!r.wants_xml);
    }

    #[test]
    fn normalizes_hyphen_and_dot_separators() {
        assert_eq!(parse_provision_target("aa-bb-cc-dd-ee-ff").unwrap().mac, "aabbccddeeff");
        assert_eq!(parse_provision_target("aabb.ccdd.eeff").unwrap().mac, "aabbccddeeff");
    }

    #[test]
    fn normalizes_bare_lowercase_no_separators() {
        assert_eq!(parse_provision_target("0123456789ab").unwrap().mac, "0123456789ab");
    }

    #[test]
    fn uppercase_is_lowercased() {
        assert_eq!(parse_provision_target("0123456789AB.cfg").unwrap().mac, "0123456789ab");
    }

    #[test]
    fn accepts_grandstream_cfg_prefixed_names() {
        // Grandstream fetches cfg<mac>, cfg<mac>.cfg, and (newer firmware) cfg<mac>.xml. The MAC
        // it uses may be upper-case; all normalise to the same 12 lowercase hex.
        let bare = parse_provision_target("cfg000b82a1b2c3").unwrap();
        assert_eq!(bare.mac, "000b82a1b2c3");
        assert!(!bare.wants_xml);

        let cfg = parse_provision_target("cfg000b82a1b2c3.cfg").unwrap();
        assert_eq!(cfg.mac, "000b82a1b2c3");
        assert!(!cfg.wants_xml);

        let xml = parse_provision_target("cfg000B82A1B2C3.xml").unwrap();
        assert_eq!(xml.mac, "000b82a1b2c3");
        assert!(xml.wants_xml);
    }

    #[test]
    fn cfg_prefix_stripping_does_not_corrupt_a_bare_mac() {
        // A 12-hex MAC can never begin with the literal "cfg" (g is not hex), so stripping the
        // prefix is safe: "cf"-leading MACs are untouched.
        assert_eq!(parse_provision_target("cfaabbccddee").unwrap().mac, "cfaabbccddee");
    }

    #[test]
    fn junk_is_rejected() {
        assert_eq!(parse_provision_target("hello.cfg"), None);
        assert_eq!(parse_provision_target("not-a-mac"), None);
        assert_eq!(parse_provision_target(""), None);
        // Grandstream's model-specific / generic fallbacks are not MAC-addressable.
        assert_eq!(parse_provision_target("cfggrp2614.xml"), None);
        assert_eq!(parse_provision_target("cfg.xml"), None);
    }

    #[test]
    fn wrong_length_is_rejected() {
        // 10 hex chars — too short.
        assert_eq!(parse_provision_target("aabbccddee"), None);
        // 14 hex chars — too long.
        assert_eq!(parse_provision_target("aabbccddeeff00"), None);
    }

    #[test]
    fn non_hex_of_right_length_is_rejected() {
        // 12 chars but 'g'/'z' are not hex.
        assert_eq!(parse_provision_target("aabbccddeegg"), None);
        assert_eq!(parse_provision_target("zzzzzzzzzzzz"), None);
    }

    #[test]
    fn render_yealink_has_signature_and_account() {
        let (ct, body) = render("yealink", false, "aabbccddeeff", "1001", "Front Desk", "s3cr3t", "10.0.0.5", 5060, "10.0.0.5");
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
        // The phone is pointed at the CommOS host for time.
        assert!(body.contains("local_time.ntp_server1 = 10.0.0.5"));
    }

    #[test]
    fn render_grandstream_emits_real_pvalues() {
        let (ct, body) = render("grandstream", false, "aabbccddeeff", "1002", "Room 101", "s3cr3t", "10.0.0.6", 5060, "10.0.0.6");
        assert_eq!(ct, "text/plain; charset=utf-8");
        // Real Grandstream plain-text P-values the handset consumes — not the old readable block.
        assert!(body.contains("P271 = 1")); // account active
        assert!(body.contains("P35 = 1002")); // SIP User ID
        assert!(body.contains("P36 = 1002")); // Authenticate ID
        assert!(body.contains("P34 = s3cr3t")); // Authenticate Password
        assert!(body.contains("P47 = 10.0.0.6")); // SIP Server (signature line)
        assert!(body.contains("P270 = Room 101")); // account name (LCD label)
        assert!(body.contains("P3 = Room 101")); // display name
        assert!(body.contains("P30 = 10.0.0.6")); // NTP time server → the CommOS host
        // A standard 5060 is left bare (no host:port) on P47.
        assert!(!body.contains("P47 = 10.0.0.6:5060"));
        // Must not regress to the old, non-consumable account.1.* block.
        assert!(!body.contains("account.1."));
    }

    #[test]
    fn render_grandstream_appends_nonstandard_port_to_p47() {
        let (_ct, body) = render("grandstream", false, "aabbccddeeff", "1002", "Room 101", "s3cr3t", "10.0.0.6", 5070, "10.0.0.6");
        // A non-default SIP port rides on the server field as host:port.
        assert!(body.contains("P47 = 10.0.0.6:5070"));
    }

    #[test]
    fn render_grandstream_xml_wraps_the_same_pvalues() {
        let (ct, body) = render("grandstream", true, "000b82a1b2c3", "1002", "Room 101", "s3cr3t", "10.0.0.6", 5060, "10.0.0.6");
        // Served as XML so the handset parses it as its cfg<mac>.xml document.
        assert_eq!(ct, "text/xml; charset=utf-8");
        assert!(body.starts_with("<?xml"));
        assert!(body.contains("<gs_provision version=\"1\">"));
        // The MAC element lets the phone validate the file is for it.
        assert!(body.contains("<mac>000b82a1b2c3</mac>"));
        // Same P-values, in XML element form (signature line P47, plus SIP + NTP).
        assert!(body.contains("<P271>1</P271>"));
        assert!(body.contains("<P35>1002</P35>"));
        assert!(body.contains("<P47>10.0.0.6</P47>"));
        assert!(body.contains("<P30>10.0.0.6</P30>"));
        // No plain-text `P.. = ..` lines leaked into the XML.
        assert!(!body.contains("P271 = 1"));
    }

    #[test]
    fn grandstream_xml_escapes_special_characters() {
        // A display name with XML metacharacters must not break the document.
        let (_ct, body) = render("grandstream", true, "000b82a1b2c3", "1002", "A&B <Front>", "s3cr3t", "10.0.0.6", 5060, "10.0.0.6");
        assert!(body.contains("<P270>A&amp;B &lt;Front&gt;</P270>"));
        assert!(!body.contains("<Front>"));
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
        let (ct, body) = render("polycom", false, "aabbccddeeff", "1003", "Ext 1003", "s3cr3t", "10.0.0.7", 5060, "10.0.0.7");
        assert_eq!(ct, "text/plain; charset=utf-8");
        // Generic signature: INI [account] section, not vendor-specific keys.
        assert!(body.contains("[account]"));
        assert!(body.contains("username=1003"));
        assert!(body.contains("password=s3cr3t"));
        assert!(!body.contains("account.1."));
        // Time source flows into the generic block too.
        assert!(body.contains("ntp_server=10.0.0.7"));
    }

    #[test]
    fn render_unknown_vendor_falls_back_to_generic() {
        let (_ct, body) = render("unknown", false, "aabbccddeeff", "1004", "Warehouse", "s3cr3t", "10.0.0.8", 5060, "10.0.0.8");
        assert!(body.contains("[account]"));
        assert!(body.contains("registrar=10.0.0.8"));
        // Display name flows into the generic block too.
        assert!(body.contains("display_name=Warehouse"));
    }
}
