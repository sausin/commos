//! Remote phone reboot via an unsolicited SIP `NOTIFY … Event: check-sync;reboot=true` — the
//! de-facto "resync/reboot now" nudge desk phones honour.
//!
//! This is the reliable counterpart to the onboarding *sweep* ([`crate::control::onboarding::
//! reboot_phones`]), which fires one datagram at a discovered IP from an ephemeral port and never
//! reads a reply. That works for a freshly-discovered Yealink (its config carries
//! `sip.notify_reboot_enable = 1` and our NOTIFY carries `reboot=true`), but a Grandstream
//! **authenticates** the NOTIFY by default and silently drops an unauthenticated one — so the
//! sweep can't reboot it. This path instead targets a *registered* extension at its current
//! contact and, crucially, **answers the digest challenge** with the phone's own SIP credential.
//! That is what makes remote reboot work for guest check-in/checkout, where the phone is
//! registered and re-provisions to its locked baseline on the reboot.
//!
//! Isolated + best-effort: it binds its own ephemeral socket at `media_ip:0` — carrying a
//! reachable `Via`/`Contact` sent-by per the outbound-UAC rule (see `sip::message::via_header`) so
//! the phone's response returns to us — and runs a single outbound transaction. It never touches
//! the main SIP receive loop, so a failure here cannot affect call handling.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;

use commos_core::common::Uuid;

use crate::control::registrations::RegistrationRegistry;
use crate::sip::message::{self, SipMessage};
use crate::sip::server::resolve_contact_addr;
use crate::sip::digest;
use crate::store::Store;

/// How long to wait for a response to one NOTIFY before giving up (a phone that simply reboots on
/// receipt never answers, so this is short — a lost/ignored reply must not stall a checkout).
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);

/// Largest SIP response we read back (a 401 challenge is tiny; 4 KiB is ample).
const MAX_DATAGRAM: usize = 4096;

/// The outcome of a registration-targeted reboot request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RebootResult {
    /// No live registration for this extension — nothing to send to (power the phone on / let it
    /// register first).
    NotRegistered,
    /// The phone demanded digest auth but no SIP credential was on file to answer with.
    Challenged,
    /// Could not resolve the contact or bind a socket to send from.
    Unreachable(String),
    /// The reboot NOTIFY was delivered. `authenticated` is true when the phone challenged us and
    /// we answered with its credential (the Grandstream path); false when it accepted the
    /// unauthenticated NOTIFY or rebooted without replying.
    Sent { authenticated: bool },
}

/// Ask the phone registered as `ext_number` to reboot and re-provision now.
///
/// Looks up the extension's live registration, sends a `check-sync;reboot=true` NOTIFY to its
/// contact, and — if the phone answers `401`/`407` — re-sends it authenticated with the phone's
/// SIP credential. Best-effort: a phone that reboots without a final response is still reported
/// `Sent`.
pub async fn reboot_registered(
    registrations: &RegistrationRegistry,
    store: &Arc<dyn Store>,
    media_ip: IpAddr,
    tenant: Uuid,
    ext_number: &str,
) -> RebootResult {
    // 1. Find the live registration whose AoR user-part is this extension.
    let Some(reg) = registrations
        .list(tenant)
        .into_iter()
        .find(|r| aor_user(&r.aor).as_deref() == Some(ext_number))
    else {
        return RebootResult::NotRegistered;
    };

    // 2. Where to send: the registered contact URI (an IP:port on the phone LAN).
    let Some(dst) = resolve_contact_addr(&reg.contact).await else {
        return RebootResult::Unreachable(format!("cannot resolve contact {}", reg.contact));
    };

    // 3. Our own socket, bound to the media IP so the Via/Contact carry a reachable sent-by and
    //    the phone's response comes back here. Ephemeral port; isolated from the SIP receive loop.
    let bind = SocketAddr::new(media_ip, 0);
    let sock = match UdpSocket::bind(bind).await {
        Ok(s) => s,
        Err(e) => return RebootResult::Unreachable(format!("bind {bind}: {e}")),
    };
    let sent_by = sock.local_addr().unwrap_or(bind);

    // Stable dialog identifiers, reused across the initial and (if challenged) authenticated send.
    let id = Uuid::now_v7().to_string().replace('-', "");
    let call_id = format!("{id}@{media_ip}");
    let from_tag = &id[0..16];

    // 4. First NOTIFY, unauthenticated.
    let notify = build_check_sync_notify(sent_by, &reg.contact, &reg.aor, &call_id, from_tag, 1, None);
    let resp = send_and_await_response(&sock, notify.as_bytes(), dst).await;

    // 5. Grandstream (and any phone with SIP NOTIFY auth on) answers 401/407. Parse the challenge,
    //    fetch the phone's credential, and re-send authenticated. A phone that just reboots on the
    //    first NOTIFY returns None here — still a best-effort success.
    if let Some(m) = &resp {
        if matches!(m.status(), Some(401) | Some(407)) {
            let hdr = if m.status() == Some(407) {
                "Proxy-Authenticate"
            } else {
                "WWW-Authenticate"
            };
            let challenge = m.header(hdr).and_then(digest::parse_challenge);
            let secret = store.get_sip_credential(tenant, ext_number).await.ok().flatten();
            let (Some(challenge), Some(secret)) = (challenge, secret) else {
                // Challenged but we cannot answer (no credential, or unparseable challenge).
                return RebootResult::Challenged;
            };
            let cnonce = &Uuid::now_v7().to_string().replace('-', "")[0..16];
            let auth = digest::authorization_value(
                ext_number, &secret, "NOTIFY", &reg.contact, &challenge, cnonce,
            );
            let authed =
                build_check_sync_notify(sent_by, &reg.contact, &reg.aor, &call_id, from_tag, 2, Some(&auth));
            let _ = send_and_await_response(&sock, authed.as_bytes(), dst).await;
            return RebootResult::Sent { authenticated: true };
        }
    }
    RebootResult::Sent { authenticated: false }
}

/// Build a `NOTIFY … Event: check-sync;reboot=true`. `sent_by` (our `media_ip:port`) fills the
/// `Via`/`Contact` so the phone can reply to us; `request_uri`/`to_aor` address the phone;
/// `call_id`/`from_tag` are stable across a challenge; `cseq` increments on the authenticated
/// re-send; `authorization` carries the digest answer on that re-send.
fn build_check_sync_notify(
    sent_by: SocketAddr,
    request_uri: &str,
    to_aor: &str,
    call_id: &str,
    from_tag: &str,
    cseq: u32,
    authorization: Option<&str>,
) -> String {
    let mut headers = vec![
        ("Via", message::via_header(sent_by)),
        ("From", format!("<sip:commos@{}>;tag={from_tag}", sent_by.ip())),
        ("To", format!("<{to_aor}>")),
        ("Call-ID", call_id.to_string()),
        ("CSeq", format!("{cseq} NOTIFY")),
        ("Contact", format!("<sip:commos@{sent_by}>")),
        ("Event", "check-sync;reboot=true".to_string()),
    ];
    if let Some(auth) = authorization {
        headers.push(("Authorization", auth.to_string()));
    }
    message::request("NOTIFY", request_uri, &headers, None)
}

/// Send `msg` to `dst` and return the first final (`>= 200`) response, retransmitting per the
/// non-INVITE client transaction until one arrives or [`ATTEMPT_TIMEOUT`] elapses. `None` means
/// nothing came back — expected when the phone reboots on receipt rather than answering.
async fn send_and_await_response(sock: &UdpSocket, msg: &[u8], dst: SocketAddr) -> Option<SipMessage> {
    if sock.send_to(msg, dst).await.is_err() {
        return None;
    }
    let deadline = tokio::time::sleep(ATTEMPT_TIMEOUT);
    tokio::pin!(deadline);
    let mut buf = vec![0u8; MAX_DATAGRAM];
    let mut interval = Duration::from_millis(500);
    loop {
        let retx = tokio::time::sleep(interval);
        tokio::select! {
            _ = &mut deadline => return None,
            _ = retx => {
                let _ = sock.send_to(msg, dst).await;
                interval = (interval * 2).min(Duration::from_secs(1));
            }
            r = sock.recv_from(&mut buf) => {
                let Ok((n, _)) = r else { return None };
                if let Ok(m) = message::parse(&buf[..n]) {
                    match m.status() {
                        Some(s) if s >= 200 => return Some(m),
                        _ => continue, // provisional / stray — keep waiting
                    }
                }
            }
        }
    }
}

/// The user-part of an AoR (`sip:100@host` → `100`), or `None` if there isn't one.
fn aor_user(aor: &str) -> Option<String> {
    let s = aor
        .trim()
        .trim_start_matches('<')
        .trim_start_matches("sips:")
        .trim_start_matches("sip:");
    let user = s.split('@').next().unwrap_or("");
    (!user.is_empty()).then(|| user.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent_by() -> SocketAddr {
        "192.168.1.10:5062".parse().unwrap()
    }

    #[test]
    fn notify_is_a_well_formed_check_sync() {
        let msg = build_check_sync_notify(sent_by(), "sip:100@192.168.1.55:5060", "sip:100@192.168.1.10", "cid@h", "abcdef0123456789", 1, None);
        assert!(msg.starts_with("NOTIFY sip:100@192.168.1.55:5060 SIP/2.0\r\n"));
        assert!(msg.contains("Event: check-sync;reboot=true\r\n"));
        // The Via/Contact carry our reachable sent-by so the reply routes back.
        assert!(msg.contains("Via: SIP/2.0/UDP 192.168.1.10:5062"));
        assert!(msg.contains("Contact: <sip:commos@192.168.1.10:5062>"));
        assert!(msg.contains("CSeq: 1 NOTIFY\r\n"));
        // Unauthenticated first attempt carries no Authorization.
        assert!(!msg.contains("Authorization"));
    }

    #[test]
    fn authenticated_resend_carries_authorization_and_next_cseq() {
        let msg = build_check_sync_notify(sent_by(), "sip:100@192.168.1.55:5060", "sip:100@192.168.1.10", "cid@h", "abcdef0123456789", 2, Some("Digest username=\"100\", realm=\"commos\""));
        assert!(msg.contains("CSeq: 2 NOTIFY\r\n"));
        assert!(msg.contains("Authorization: Digest username=\"100\", realm=\"commos\"\r\n"));
    }

    #[test]
    fn aor_user_extracts_the_extension() {
        assert_eq!(aor_user("sip:100@host").as_deref(), Some("100"));
        assert_eq!(aor_user("<sips:200@1.2.3.4:5060>").as_deref(), Some("200"));
        assert_eq!(aor_user("garbage").as_deref(), Some("garbage"));
        assert_eq!(aor_user("sip:@host"), None);
    }

    #[tokio::test]
    async fn not_registered_when_no_registration() {
        let regs = RegistrationRegistry::new();
        let store: Arc<dyn Store> = Arc::new(crate::store::MemStore::new());
        let tenant = Uuid::now_v7();
        let r = reboot_registered(&regs, &store, "127.0.0.1".parse().unwrap(), tenant, "100").await;
        assert_eq!(r, RebootResult::NotRegistered);
    }

    // End-to-end over loopback: a fake phone that challenges the reboot NOTIFY with 401 must
    // receive a second, digest-authenticated NOTIFY carrying its credential — the Grandstream path.
    #[tokio::test]
    async fn challenges_are_answered_with_digest() {
        let phone = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let phone_addr = phone.local_addr().unwrap();

        let store: Arc<dyn Store> = Arc::new(crate::store::MemStore::new());
        let tenant = Uuid::now_v7();
        store.put_sip_credential(tenant, "100", "s3cr3t").await.unwrap();

        let regs = RegistrationRegistry::new();
        regs.register(tenant, "sip:100@127.0.0.1".to_string(), format!("sip:100@{phone_addr}"), None, 3600);

        // The fake phone: read NOTIFY #1, reply 401 with a challenge, then capture NOTIFY #2.
        let phone_task = tokio::spawn(async move {
            let mut buf = vec![0u8; MAX_DATAGRAM];
            let (n, from) = phone.recv_from(&mut buf).await.unwrap();
            let first = String::from_utf8_lossy(&buf[..n]).to_string();
            let resp = "SIP/2.0 401 Unauthorized\r\n\
                        Via: SIP/2.0/UDP 127.0.0.1\r\n\
                        WWW-Authenticate: Digest realm=\"commos\", nonce=\"abc123\", qop=\"auth\"\r\n\
                        Content-Length: 0\r\n\r\n";
            phone.send_to(resp.as_bytes(), from).await.unwrap();
            let (n2, _) = phone.recv_from(&mut buf).await.unwrap();
            let second = String::from_utf8_lossy(&buf[..n2]).to_string();
            (first, second)
        });

        let result = reboot_registered(&regs, &store, "127.0.0.1".parse().unwrap(), tenant, "100").await;
        assert_eq!(result, RebootResult::Sent { authenticated: true });

        let (first, second) = phone_task.await.unwrap();
        assert!(first.contains("NOTIFY"));
        assert!(first.contains("Event: check-sync;reboot=true"));
        assert!(!first.contains("Authorization"));
        // The re-send is authenticated with the phone's credential and a fresh CSeq.
        assert!(second.contains("Authorization: Digest"));
        assert!(second.contains("username=\"100\""));
        assert!(second.contains("CSeq: 2 NOTIFY"));
    }
}
