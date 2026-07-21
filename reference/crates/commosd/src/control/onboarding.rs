//! Admin onboarding intelligence — capture as much as possible automatically and propose
//! good defaults so bringing CommOS up is fast (Volume 13 UX; the operability mandate,
//! CMOS-00-ENG-005). The philosophy: **ask the operator almost nothing.** We detect the
//! network, discover phones, and pre-fill everything; the operator confirms.
//!
//! Nothing here is SIP mechanics — it is comms *management*: what extensions to hand out,
//! what IP plan to use, which phones are on the wire, and the exact DNS/DHCP lines the
//! operator must paste so phones auto-provision.
//!
//! Network discovery is Linux-native (a UDP-connect trick for the primary IP; `/proc/net/arp`
//! for the neighbour table) — no external commands, works on a Raspberry Pi.

use std::net::{IpAddr, Ipv4Addr, UdpSocket};

use serde::{Deserialize, Serialize};

use commos_core::common::Uuid;
use commos_core::entities::device::{Device, DeviceNetwork};
use commos_core::entities::extension::Extension;
use commos_core::entities::route::Route;
use commos_core::entities::user::User;

/// The kind of place CommOS is being deployed. Drives the default suggestions — the one
/// early question worth asking, because it changes almost every good default.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    Office,
    Hospitality,
    Hospital,
    Home,
}

impl Environment {
    pub fn parse(s: &str) -> Option<Environment> {
        match s.to_ascii_lowercase().as_str() {
            "office" => Some(Environment::Office),
            "hospitality" | "hotel" => Some(Environment::Hospitality),
            "hospital" | "clinic" | "healthcare" => Some(Environment::Hospital),
            "home" | "residential" => Some(Environment::Home),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Environment::Office => "office",
            Environment::Hospitality => "hospitality",
            Environment::Hospital => "hospital",
            Environment::Home => "home",
        }
    }
}

/// A short, opinionated defaults set for an [`Environment`] — the "sensible defaults" that
/// mean the operator never opens Advanced settings for the common case.
#[derive(Clone, Debug, Serialize)]
pub struct EnvironmentProfile {
    pub environment: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    /// Extension the operator should publish for reception / help desk (the "dial this for a
    /// human" number). 9 in most places; 0 where guests expect a front desk.
    pub reception_extension: &'static str,
    pub operator_extension: &'static str,
    /// Preferred number of digits in a user extension.
    pub extension_digits: u8,
    /// Whether guest/room phones default to no outbound international (hospitality/hospital).
    pub restrict_international_by_default: bool,
    /// Voicemail enabled by default.
    pub voicemail_default: bool,
    /// Auto-attendant / IVR on the main number by default.
    pub auto_attendant_default: bool,
    /// One-line hint on how extensions are usually numbered here.
    pub numbering_hint: &'static str,
}

pub fn profiles() -> Vec<EnvironmentProfile> {
    vec![
        EnvironmentProfile {
            environment: "office",
            title: "Office / Business",
            description: "Desks and shared spaces; staff have named extensions.",
            reception_extension: "9",
            operator_extension: "0",
            extension_digits: 3,
            restrict_international_by_default: false,
            voicemail_default: true,
            auto_attendant_default: true,
            numbering_hint: "3-digit extensions per person, e.g. 100, 101, 102…",
        },
        EnvironmentProfile {
            environment: "hospitality",
            title: "Hotel / Hospitality",
            description: "Guest room phones plus front-desk and back-office staff.",
            reception_extension: "0",
            operator_extension: "0",
            extension_digits: 4,
            restrict_international_by_default: true,
            voicemail_default: true,
            auto_attendant_default: true,
            numbering_hint: "Extensions mirror room numbers, e.g. 101 = room 101; 0 = front desk.",
        },
        EnvironmentProfile {
            environment: "hospital",
            title: "Hospital / Healthcare",
            description: "Ward, bed, and station phones; strong access control.",
            reception_extension: "0",
            operator_extension: "0",
            extension_digits: 4,
            restrict_international_by_default: true,
            voicemail_default: false,
            auto_attendant_default: true,
            numbering_hint: "Extensions by ward+room, e.g. 3201 = ward 3, room 201.",
        },
        EnvironmentProfile {
            environment: "home",
            title: "Home / Small",
            description: "A handful of handsets; keep it simple.",
            reception_extension: "9",
            operator_extension: "0",
            extension_digits: 2,
            restrict_international_by_default: false,
            voicemail_default: true,
            auto_attendant_default: false,
            numbering_hint: "Short 2-digit extensions, e.g. 10, 11, 12…",
        },
    ]
}

pub fn profile_for(env: Environment) -> EnvironmentProfile {
    profiles()
        .into_iter()
        .find(|p| p.environment == env.as_str())
        .expect("every Environment has a profile")
}

// --- Extension plan -----------------------------------------------------------------------

/// A concrete, ready-to-accept extension numbering plan.
#[derive(Clone, Debug, Serialize)]
pub struct ExtensionPlan {
    pub digits: u8,
    /// The recommended starting series (e.g. `"100"`), pre-selected in the UI.
    pub recommended_series: String,
    /// The dropdown of series the operator can pick instead (e.g. 100/200/…/900 or 1000/2000).
    pub series_options: Vec<String>,
    /// How many extensions a single series holds at this digit length.
    pub capacity_per_series: u32,
    /// The first few extensions the plan would mint, so the choice is concrete.
    pub example_extensions: Vec<String>,
    /// Reserved service numbers (reception/operator/voicemail/pickup/park) — good defaults.
    pub reception_extension: String,
    pub operator_extension: String,
    pub feature_codes: Vec<FeatureCode>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FeatureCode {
    pub code: &'static str,
    pub purpose: &'static str,
}

/// Suggest an extension plan from the environment and how many devices will be deployed.
/// The digit length grows with the fleet so numbers stay short but never run out.
pub fn suggest_extension_plan(env: Environment, device_count: u32) -> ExtensionPlan {
    let profile = profile_for(env);
    // Pick digit length: honour the environment preference, but grow if the fleet needs it.
    let digits: u8 = if device_count > 900 {
        4
    } else if device_count > 80 {
        3.max(profile.extension_digits)
    } else {
        profile.extension_digits.max(2)
    };

    let (series_options, recommended, capacity): (Vec<String>, String, u32) = match digits {
        2 => (
            vec!["10".into(), "20".into(), "30".into(), "40".into()],
            "10".into(),
            90, // 10..99
        ),
        3 => (
            (1..=9).map(|n| format!("{n}00")).collect(),
            "100".into(),
            100, // e.g. 100..199
        ),
        _ => (
            (1..=9).map(|n| format!("{n}000")).collect(),
            "1000".into(),
            1000, // e.g. 1000..1999
        ),
    };

    let start: u32 = recommended.parse().unwrap_or(100);
    let example_extensions = (0..device_count.min(5))
        .map(|i| (start + i).to_string())
        .collect();

    ExtensionPlan {
        digits,
        recommended_series: recommended,
        series_options,
        capacity_per_series: capacity,
        example_extensions,
        reception_extension: profile.reception_extension.to_string(),
        operator_extension: profile.operator_extension.to_string(),
        feature_codes: vec![
            FeatureCode { code: "*97", purpose: "Check your voicemail" },
            FeatureCode { code: "*98", purpose: "Check another extension's voicemail" },
            FeatureCode { code: "*8", purpose: "Pick up a ringing call nearby" },
            FeatureCode { code: "70", purpose: "Park a call" },
            FeatureCode { code: profile.reception_extension, purpose: "Reach reception / help desk" },
        ],
    }
}

// --- IP plan ------------------------------------------------------------------------------

/// A suggested addressing plan for the phone fleet, derived from the host's own address.
#[derive(Clone, Debug, Serialize)]
pub struct IpPlan {
    pub detected_host_ip: Option<String>,
    /// The /24 the host sits on, assumed for the LAN (e.g. `192.168.1.0/24`).
    pub detected_subnet: Option<String>,
    pub suggested_gateway: Option<String>,
    /// Suggested DHCP pool to reserve for phones.
    pub phone_pool_start: Option<String>,
    pub phone_pool_end: Option<String>,
    pub phone_pool_capacity: u32,
    /// Whether the fleet fits the suggested pool.
    pub fits: bool,
    /// If it doesn't fit, a bigger recommendation and why.
    pub recommendation: Option<String>,
}

/// The host's primary outbound IPv4, via the standard UDP-connect trick (no packets sent —
/// `connect` on a UDP socket just selects the source address for the given destination).
pub fn primary_host_ip() -> Option<IpAddr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    sock.local_addr().ok().map(|a| a.ip())
}

pub fn suggest_ip_plan(device_count: u32) -> IpPlan {
    let host = primary_host_ip();
    match host {
        Some(IpAddr::V4(v4)) => {
            let o = v4.octets();
            let subnet = format!("{}.{}.{}.0/24", o[0], o[1], o[2]);
            let gateway = format!("{}.{}.{}.1", o[0], o[1], o[2]);
            // Reserve .50–.250 for phones (201 addresses), leaving room for infra + DHCP lease
            // headroom below .50 and above .250.
            let pool_start = Ipv4Addr::new(o[0], o[1], o[2], 50);
            let pool_end = Ipv4Addr::new(o[0], o[1], o[2], 250);
            let capacity = 201u32;
            let fits = device_count <= capacity;
            let recommendation = if fits {
                None
            } else {
                Some(format!(
                    "The fleet ({device_count}) is larger than a single /24 phone pool (~{capacity}). \
                     Use a larger subnet (e.g. a /23 = ~500 hosts) or a dedicated voice VLAN/subnet \
                     so phones and workstations don't compete for addresses."
                ))
            };
            IpPlan {
                detected_host_ip: Some(v4.to_string()),
                detected_subnet: Some(subnet),
                suggested_gateway: Some(gateway),
                phone_pool_start: Some(pool_start.to_string()),
                phone_pool_end: Some(pool_end.to_string()),
                phone_pool_capacity: capacity,
                fits,
                recommendation,
            }
        }
        _ => IpPlan {
            detected_host_ip: host.map(|h| h.to_string()),
            detected_subnet: None,
            suggested_gateway: None,
            phone_pool_start: None,
            phone_pool_end: None,
            phone_pool_capacity: 0,
            fits: false,
            recommendation: Some(
                "Could not detect an IPv4 LAN address automatically; enter the phone subnet manually."
                    .into(),
            ),
        },
    }
}

// --- MAC / device discovery ---------------------------------------------------------------

/// A neighbour found on the LAN, with a best-effort guess at whether it's a SIP phone.
#[derive(Clone, Debug, Serialize)]
pub struct DiscoveredDevice {
    pub ip: String,
    pub mac: String,
    pub interface: String,
    /// Vendor guessed from the MAC OUI, when recognised.
    pub vendor: Option<String>,
    /// True when the OUI matches a known IP-phone vendor.
    pub likely_phone: bool,
}

/// Known IP-phone vendor OUIs (first 3 MAC octets, lowercase). A small, illustrative set —
/// enough to flag the common desk phones on a typical LAN.
fn vendor_for_oui(oui: &str) -> Option<&'static str> {
    match oui {
        "00:15:65" | "80:5e:c0" | "24:9a:d8" | "00:1f:c1" => Some("Yealink"),
        "00:0b:82" | "c0:74:ad" | "00:0b:83" => Some("Grandstream"),
        "00:04:f2" | "64:16:7f" | "48:25:67" => Some("Polycom"),
        "00:04:13" | "00:1a:4b" => Some("Snom"),
        "0c:38:3e" | "70:2a:d5" => Some("Fanvil"),
        "00:1a:a0" | "00:0e:08" | "88:75:56" => Some("Cisco"),
        _ => None,
    }
}

/// Parse the Linux ARP table (`/proc/net/arp`) into discovered devices. Incomplete entries
/// (flags `0x0`) and the loopback are skipped. On non-Linux hosts this returns empty.
pub fn discovered_devices() -> Vec<DiscoveredDevice> {
    let raw = match std::fs::read_to_string("/proc/net/arp") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for line in raw.lines().skip(1) {
        // Columns: IP address, HW type, Flags, HW address, Mask, Device
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 6 {
            continue;
        }
        let (ip, flags, mac, iface) = (cols[0], cols[2], cols[3], cols[5]);
        if flags == "0x0" || mac == "00:00:00:00:00:00" {
            continue; // incomplete / unresolved
        }
        let oui: String = mac.split(':').take(3).collect::<Vec<_>>().join(":").to_ascii_lowercase();
        let vendor = vendor_for_oui(&oui).map(str::to_string);
        out.push(DiscoveredDevice {
            ip: ip.to_string(),
            likely_phone: vendor.is_some(),
            vendor,
            mac: mac.to_string(),
            interface: iface.to_string(),
        });
    }
    out
}

// --- Auto-provisioning (DNS / DHCP) guidance ----------------------------------------------

/// Ready-to-paste records the operator sets on their DNS/DHCP so phones auto-provision and
/// find CommOS. Generated from the detected host address and the running ports — exactly the
/// format they'll need, so there's no guesswork.
#[derive(Clone, Debug, Serialize)]
pub struct ProvisioningGuide {
    pub domain: String,
    pub provisioning_url: String,
    /// DHCP option 66 value (provisioning/config server).
    pub dhcp_option_66: String,
    /// DHCP option 67 value (per-device bootfile name pattern).
    pub dhcp_option_67: String,
    /// Ready-to-paste ISC `dhcpd`/`dnsmasq` lines.
    pub dhcp_dnsmasq: Vec<String>,
    /// Ready-to-paste BIND zone lines (A + SRV so phones find the SIP service).
    pub dns_bind_zone: Vec<String>,
    pub note: String,
}

pub fn provisioning_guide(domain: &str, host_ip: &str, http_port: u16, sip_port: u16) -> ProvisioningGuide {
    let provisioning_url = format!("http://{host_ip}:{http_port}/provision");
    ProvisioningGuide {
        domain: domain.to_string(),
        provisioning_url: provisioning_url.clone(),
        dhcp_option_66: provisioning_url.clone(),
        dhcp_option_67: "{mac}.cfg".to_string(),
        dhcp_dnsmasq: vec![
            format!("# dnsmasq: point phones at CommOS for auto-provisioning"),
            format!("dhcp-option=66,\"{provisioning_url}\""),
            format!("dhcp-option=67,\"{{mac}}.cfg\""),
        ],
        dns_bind_zone: vec![
            format!("; CommOS records for {domain}"),
            format!("pbx.{domain}.            300 IN A    {host_ip}"),
            format!("_sip._udp.{domain}.      300 IN SRV  0 0 {sip_port} pbx.{domain}."),
        ],
        note: format!(
            "Set DHCP option 66 to the provisioning URL and add the DNS records above. Phones \
             that support DHCP option 66 will fetch their config from CommOS on next boot; \
             others can be pointed at {provisioning_url} manually."
        ),
    }
}

// --- Apply: turn the suggestion into real entities ----------------------------------------

/// The entities an "apply" would create — built in memory, then committed in one transaction.
pub struct BuiltEntities {
    pub users: Vec<User>,
    pub extensions: Vec<Extension>,
    pub devices: Vec<Device>,
    /// One Route per extension: the dialable number's destination (`sip:<number>@<domain>`),
    /// which the control plane resolves when a call comes in (Volume 3 Routing).
    pub routes: Vec<Route>,
}

/// Normalise a MAC to 12 lowercase hex chars (`00:15:65:AA:BB:CC` → `001565aabbcc`), or `None`.
fn mac_hex(s: &str) -> Option<String> {
    let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect::<String>().to_ascii_lowercase();
    (hex.len() == 12).then_some(hex)
}

/// Build the people, extensions, routes, and phones for a confirmed onboarding choice. Each
/// extension gets a person and a **real Route** (`sip:<number>@<domain>`) that the control
/// plane resolves on an inbound call; the extension's `route_id` points at it. Discovered
/// phones (from ARP) are bound to the first extensions by writing the device MAC into the
/// extension `label` — exactly what the provisioning endpoint keys on, so a phone that fetches
/// its config gets that account.
pub fn build_entities(
    tenant: Uuid,
    device_count: u32,
    series_start: &str,
    domain: &str,
) -> BuiltEntities {
    let start: u32 = series_start.parse().unwrap_or(100);
    let phones: Vec<DiscoveredDevice> =
        discovered_devices().into_iter().filter(|d| d.likely_phone).collect();

    let mut users = Vec::new();
    let mut extensions = Vec::new();
    let mut devices = Vec::new();
    let mut routes = Vec::new();

    for i in 0..device_count {
        let number = (start + i).to_string();
        let user = User::new(tenant, format!("Extension {number}"));

        // A real route: dialing this number reaches the SIP endpoint that registers as it.
        let route = Route::new(tenant, format!("sip:{number}@{domain}"));
        let mut ext = Extension::new(tenant, number.clone(), route.base.id);

        // Bind a discovered phone to this extension when one is available.
        if let Some(phone) = phones.get(i as usize) {
            if let Some(mac) = mac_hex(&phone.mac) {
                ext.label = Some(mac.clone());
                let vendor = phone
                    .vendor
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string())
                    .to_ascii_lowercase();
                let mut dev = Device::new(tenant, vendor, "unknown");
                dev.mac = Some(mac);
                dev.assigned_user_id = Some(user.base.id);
                dev.network = Some(DeviceNetwork { ip: Some(phone.ip.clone()), ..Default::default() });
                devices.push(dev);
            }
        }
        routes.push(route);
        extensions.push(ext);
        users.push(user);
    }

    BuiltEntities { users, extensions, devices, routes }
}

// --- The full suggestion ------------------------------------------------------------------

/// Everything the wizard computes for the operator to confirm — one round-trip, minimal asks.
#[derive(Clone, Debug, Serialize)]
pub struct OnboardingSuggestion {
    pub environment: EnvironmentProfile,
    pub device_count: u32,
    pub extension_plan: ExtensionPlan,
    pub ip_plan: IpPlan,
    pub discovered_devices: Vec<DiscoveredDevice>,
    pub provisioning: ProvisioningGuide,
}

/// Build the whole suggestion. Auto-detects host IP and LAN devices; everything else follows
/// from the environment + fleet size.
pub fn suggest(
    env: Environment,
    device_count: u32,
    domain: &str,
    http_port: u16,
    sip_port: u16,
) -> OnboardingSuggestion {
    let ip_plan = suggest_ip_plan(device_count);
    let host_ip = ip_plan
        .detected_host_ip
        .clone()
        .unwrap_or_else(|| "192.168.1.10".to_string());
    OnboardingSuggestion {
        environment: profile_for(env),
        device_count,
        extension_plan: suggest_extension_plan(env, device_count),
        provisioning: provisioning_guide(domain, &host_ip, http_port, sip_port),
        discovered_devices: discovered_devices(),
        ip_plan,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_plan_scales_with_fleet() {
        assert_eq!(suggest_extension_plan(Environment::Home, 5).digits, 2);
        assert_eq!(suggest_extension_plan(Environment::Office, 20).digits, 3);
        assert_eq!(suggest_extension_plan(Environment::Office, 1500).digits, 4);
        // Office reception default is 9.
        assert_eq!(suggest_extension_plan(Environment::Office, 20).reception_extension, "9");
        // Hospitality guests dial 0 for the front desk.
        assert_eq!(suggest_extension_plan(Environment::Hospitality, 40).reception_extension, "0");
    }

    #[test]
    fn ip_plan_flags_oversized_fleet() {
        // 300 phones can't fit a single /24 phone pool.
        let plan = suggest_ip_plan(300);
        assert!(!plan.fits);
        assert!(plan.recommendation.is_some());
    }

    #[test]
    fn provisioning_guide_is_paste_ready() {
        let g = provisioning_guide("commos.local", "192.168.1.10", 8080, 5060);
        assert_eq!(g.dhcp_option_66, "http://192.168.1.10:8080/provision");
        assert!(g.dns_bind_zone.iter().any(|l| l.contains("_sip._udp.commos.local")));
        assert!(g.dns_bind_zone.iter().any(|l| l.contains("A    192.168.1.10")));
    }

    #[test]
    fn environments_parse() {
        assert_eq!(Environment::parse("hotel"), Some(Environment::Hospitality));
        assert_eq!(Environment::parse("nope"), None);
    }
}
