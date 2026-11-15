//! BLF / presence subscriptions (RFC 6665 SUBSCRIBE/NOTIFY; RFC 4235 `dialog` event package).
//!
//! This module is the **pure** half of the feature: the in-memory subscription table, the
//! per-extension call-dialog tracker fed from call events on the bus, and the XML renderers
//! (`application/dialog-info+xml`, PIDF). It never touches a socket — the SIP glue in
//! `sip/server/subscribe.rs` owns request parsing and in-dialog NOTIFY construction.
//!
//! Call state comes from the existing event bus (`CallStarted`/`CallRinging`/`CallAnswered`/
//! `CallEnded`); the bus carries only the call's `from_ref`/`to_ref`, so a lamp tracks the
//! *dialled* extension and the calling extension.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::sip::transport::Responder;

/// Shortest / longest subscription lifetime we grant (seconds); `0` means unsubscribe.
pub const MIN_EXPIRES: u64 = 60;
pub const MAX_EXPIRES: u64 = 3600;
/// Maximum live subscriptions per watcher (the authenticated digest username, or the source
/// address when auth is off), per source IP, and in total.
pub const MAX_PER_WATCHER: usize = 200;
pub const MAX_PER_IP: usize = 1000;
pub const MAX_TOTAL: usize = 10_000;

/// Clamp a requested lifetime into the granted range (`0` is preserved = terminate).
pub fn clamp_expires(requested: u64) -> u64 {
    if requested == 0 {
        0
    } else {
        requested.clamp(MIN_EXPIRES, MAX_EXPIRES)
    }
}

/// Supported event packages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    Dialog,
    MessageSummary,
    Presence,
}

impl EventKind {
    /// Parse an `Event` header value (`dialog;id=1` → `Dialog`).
    pub fn parse(header: &str) -> Option<Self> {
        let token = header.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
        match token.as_str() {
            "dialog" => Some(Self::Dialog),
            "message-summary" => Some(Self::MessageSummary),
            "presence" => Some(Self::Presence),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Dialog => "dialog",
            Self::MessageSummary => "message-summary",
            Self::Presence => "presence",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Self::Dialog => "application/dialog-info+xml",
            Self::MessageSummary => "application/simple-message-summary",
            Self::Presence => "application/pidf+xml",
        }
    }
}

/// Subscription identity: `Call-ID` + the subscriber's `From` tag (our tag is generated).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SubKey {
    pub call_id: String,
    pub remote_tag: String,
}

/// One live subscription (a dialog we are the notifier of).
#[derive(Clone)]
pub struct Subscription {
    pub key: SubKey,
    pub event: EventKind,
    /// The watched extension (user-part).
    pub target: String,
    /// The watching identity for the per-watcher cap: the authenticated digest username
    /// (never the attacker-controlled `From`), or `ip:port` of the source when auth is off.
    pub watcher: String,
    /// Source IP of the SUBSCRIBE, for the per-IP cap.
    pub src_ip: std::net::IpAddr,
    /// NOTIFY `From` header value (the SUBSCRIBE's `To` with our tag).
    pub local_hdr: String,
    /// NOTIFY `To` header value (the SUBSCRIBE's `From`, with the subscriber's tag).
    pub remote_hdr: String,
    /// NOTIFY request-URI (the subscriber's Contact).
    pub remote_uri: String,
    /// `entity` URI used in dialog-info / presence documents.
    pub entity: String,
    /// Where NOTIFYs go (same transport/connection the SUBSCRIBE arrived on).
    pub sink: Responder,
    pub expires_at: Instant,
    /// Next dialog-info `version` and NOTIFY `CSeq` (both strictly increasing).
    pub version: u32,
    pub cseq: u32,
}

/// What the SIP layer needs to emit one NOTIFY (counters already advanced).
pub struct NotifyTicket {
    pub sub: Subscription,
    pub version: u32,
    pub cseq: u32,
    pub remaining_secs: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum BlfError {
    TooManySubscriptions,
}

/// `state` of one dialog in a dialog-info document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialogState {
    Trying,
    Early,
    Confirmed,
    #[allow(dead_code)] // rendered/tested; live calls are dropped from the set on end
    Terminated,
}

impl DialogState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Trying => "trying",
            Self::Early => "early",
            Self::Confirmed => "confirmed",
            Self::Terminated => "terminated",
        }
    }
}

/// One dialog an extension is party to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialogInfo {
    pub id: String,
    pub state: DialogState,
    /// True when the watched extension placed the call.
    pub initiator: bool,
    pub local: String,
    pub remote: String,
}

#[derive(Clone)]
struct CallEntry {
    from: String,
    to: String,
    state: DialogState,
}

#[derive(Default)]
struct Inner {
    subs: HashMap<SubKey, Subscription>,
    calls: HashMap<String, CallEntry>,
}

/// The subscription table + call-dialog tracker. Cheap to share behind an `Arc`.
#[derive(Default)]
pub struct BlfHub {
    inner: Mutex<Inner>,
}

/// User-part of a SIP URI / name-addr (`<sip:200@host>` → `200`); a bare token passes through.
pub fn ext_of(uri: &str) -> Option<String> {
    let s = uri.trim();
    let s = s.split('<').next_back().unwrap_or(s);
    let s = s.trim_start_matches("sips:").trim_start_matches("sip:").trim_start_matches("tel:");
    let user = match s.split_once('@') {
        Some((u, _)) => u,
        None => s,
    };
    let user = user.split([';', '>']).next().unwrap_or(user).trim();
    (!user.is_empty()).then(|| user.to_string())
}

impl BlfHub {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("blf mutex")
    }

    #[cfg(test)]
    pub fn count(&self) -> usize {
        self.lock().subs.len()
    }

    /// Insert (or replace, keeping counters) a subscription, enforcing the caps.
    pub fn subscribe(&self, sub: Subscription) -> Result<(), BlfError> {
        let mut g = self.lock();
        if let Some(existing) = g.subs.get_mut(&sub.key) {
            let (version, cseq) = (existing.version, existing.cseq);
            *existing = Subscription { version, cseq, ..sub };
            return Ok(());
        }
        let per_watcher = g.subs.values().filter(|s| s.watcher == sub.watcher).count();
        let per_ip = g.subs.values().filter(|s| s.src_ip == sub.src_ip).count();
        if per_watcher >= MAX_PER_WATCHER || per_ip >= MAX_PER_IP || g.subs.len() >= MAX_TOTAL {
            return Err(BlfError::TooManySubscriptions);
        }
        g.subs.insert(sub.key.clone(), sub);
        Ok(())
    }

    /// Refresh an existing subscription's expiry; `None` if it is unknown (→ 481).
    pub fn refresh(&self, key: &SubKey, expires: u64, now: Instant) -> Option<()> {
        let mut g = self.lock();
        let s = g.subs.get_mut(key)?;
        s.expires_at = now + Duration::from_secs(expires);
        Some(())
    }

    /// A snapshot of one subscription (no counter change).
    pub fn get(&self, key: &SubKey) -> Option<Subscription> {
        self.lock().subs.get(key).cloned()
    }

    pub fn remove(&self, key: &SubKey) -> Option<Subscription> {
        self.lock().subs.remove(key)
    }

    /// Advance the counters for the next NOTIFY on `key`.
    pub fn prepare_notify(&self, key: &SubKey, now: Instant) -> Option<NotifyTicket> {
        let mut g = self.lock();
        let s = g.subs.get_mut(key)?;
        s.cseq += 1;
        s.version += 1;
        let remaining = s.expires_at.saturating_duration_since(now).as_secs();
        Some(NotifyTicket { sub: s.clone(), version: s.version, cseq: s.cseq, remaining_secs: remaining })
    }

    /// Keys of all watchers of `ext` for `event`.
    pub fn watchers_of(&self, ext: &str, event: EventKind) -> Vec<SubKey> {
        self.lock()
            .subs
            .values()
            .filter(|s| s.event == event && s.target == ext)
            .map(|s| s.key.clone())
            .collect()
    }

    /// Remove and return every subscription whose lifetime has elapsed.
    pub fn sweep(&self, now: Instant) -> Vec<Subscription> {
        let mut g = self.lock();
        let dead: Vec<SubKey> =
            g.subs.values().filter(|s| s.expires_at <= now).map(|s| s.key.clone()).collect();
        dead.iter().filter_map(|k| g.subs.remove(k)).collect()
    }

    // ---- call tracking (fed from the event bus) ---------------------------------------

    /// Apply one bus event; returns the extensions whose dialog set changed.
    pub fn on_event(&self, event: &serde_json::Value) -> Vec<String> {
        let ty = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let data = event.get("data");
        let call_id = data.and_then(|d| d.get("call_id")).and_then(|v| v.as_str());
        let Some(call_id) = call_id else { return Vec::new() };
        let mut g = self.lock();
        let mut changed = Vec::new();
        match ty {
            "CallStarted" => {
                let get =
                    |k: &str| data.and_then(|d| d.get(k)).and_then(|v| v.as_str()).and_then(ext_of);
                if let (Some(from), Some(to)) = (get("from_ref"), get("to_ref")) {
                    changed.push(from.clone());
                    changed.push(to.clone());
                    g.calls
                        .insert(call_id.to_string(), CallEntry { from, to, state: DialogState::Trying });
                }
            }
            "CallRinging" | "CallAnswered" => {
                let new =
                    if ty == "CallRinging" { DialogState::Early } else { DialogState::Confirmed };
                if let Some(c) = g.calls.get_mut(call_id) {
                    if c.state != new {
                        c.state = new;
                        changed.push(c.from.clone());
                        changed.push(c.to.clone());
                    }
                }
            }
            "CallTransferred" => {
                // `from_ref` is the transferor (Referred-By), `to_ref` the new target. The
                // transferor's leg is replaced by the target; the other party stays.
                let get =
                    |k: &str| data.and_then(|d| d.get(k)).and_then(|v| v.as_str()).and_then(ext_of);
                if let (Some(c), Some(target)) = (g.calls.get_mut(call_id), get("to_ref")) {
                    let transferor = get("from_ref");
                    let old = if transferor.as_deref() == Some(c.to.as_str()) {
                        Some(std::mem::replace(&mut c.to, target.clone()))
                    } else if transferor.as_deref() == Some(c.from.as_str()) {
                        Some(std::mem::replace(&mut c.from, target.clone()))
                    } else {
                        None
                    };
                    if let Some(old) = old {
                        changed.push(old);
                        changed.push(target);
                        changed.push(c.from.clone());
                        changed.push(c.to.clone());
                    }
                }
            }
            "CallEnded" => {
                if let Some(c) = g.calls.remove(call_id) {
                    changed.push(c.from);
                    changed.push(c.to);
                }
            }
            _ => {}
        }
        changed.sort();
        changed.dedup();
        changed
    }

    /// Dialogs `ext` is currently party to (empty = idle).
    pub fn dialogs_for(&self, ext: &str) -> Vec<DialogInfo> {
        let g = self.lock();
        let mut v: Vec<DialogInfo> = g
            .calls
            .iter()
            .filter(|(_, c)| c.from == ext || c.to == ext)
            .map(|(id, c)| {
                let initiator = c.from == ext;
                let (local, remote) = if initiator { (&c.from, &c.to) } else { (&c.to, &c.from) };
                DialogInfo {
                    id: id.clone(),
                    state: c.state,
                    initiator,
                    local: local.clone(),
                    remote: remote.clone(),
                }
            })
            .collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// Render a full-state `application/dialog-info+xml` document (RFC 4235). `host` is the SIP
/// domain used to build identity URIs for the dialogs' parties. No dialogs = idle lamp.
pub fn render_dialog_info(version: u32, entity: &str, host: &str, dialogs: &[DialogInfo]) -> String {
    let mut out = format!(
        "<?xml version=\"1.0\"?>\r\n<dialog-info xmlns=\"urn:ietf:params:xml:ns:dialog-info\" \
         version=\"{version}\" state=\"full\" entity=\"{}\">\r\n",
        xml_escape(entity)
    );
    for d in dialogs {
        let dir = if d.initiator { "initiator" } else { "recipient" };
        out.push_str(&format!(
            "<dialog id=\"{id}\" call-id=\"{id}\" direction=\"{dir}\">\r\n<state>{st}</state>\r\n\
             <local><identity>sip:{l}@{host}</identity><target uri=\"sip:{l}@{host}\"/></local>\r\n\
             <remote><identity>sip:{r}@{host}</identity><target uri=\"sip:{r}@{host}\"/></remote>\r\n\
             </dialog>\r\n",
            id = xml_escape(&d.id),
            st = d.state.as_str(),
            l = xml_escape(&d.local),
            r = xml_escape(&d.remote),
            host = xml_escape(host),
        ));
    }
    out.push_str("</dialog-info>\r\n");
    out
}

/// Render a minimal PIDF presence document: `open` when the extension is registered.
pub fn render_pidf(entity: &str, open: bool) -> String {
    let basic = if open { "open" } else { "closed" };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\r\n\
         <presence xmlns=\"urn:ietf:params:xml:ns:pidf\" entity=\"{e}\">\r\n\
         <tuple id=\"t1\"><status><basic>{basic}</basic></status></tuple>\r\n</presence>\r\n",
        e = xml_escape(entity)
    )
}

/// RFC 3842 message-summary body.
pub fn render_message_summary(aor: &str, new: u32, old: u32) -> String {
    let waiting = if new > 0 { "yes" } else { "no" };
    format!(
        "Messages-Waiting: {waiting}\r\nMessage-Account: {aor}\r\nVoice-Message: {new}/{old} (0/0)\r\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sub(call: &str, watcher: &str, target: &str, expires_in: u64, now: Instant) -> Subscription {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        Subscription {
            key: SubKey { call_id: call.into(), remote_tag: "r".into() },
            event: EventKind::Dialog,
            target: target.into(),
            watcher: watcher.into(),
            src_ip: "127.0.0.1".parse().unwrap(),
            local_hdr: "<sip:200@h>;tag=l".into(),
            remote_hdr: "<sip:100@h>;tag=r".into(),
            remote_uri: "sip:100@10.0.0.1".into(),
            entity: format!("sip:{target}@h"),
            sink: Responder::Stream { tx, peer: "127.0.0.1:5060".parse().unwrap() },
            expires_at: now + Duration::from_secs(expires_in),
            version: 0,
            cseq: 0,
        }
    }

    fn ev(ty: &str, call: &str, from: &str, to: &str) -> serde_json::Value {
        json!({"type": ty, "data": {"call_id": call, "from_ref": from, "to_ref": to}})
    }

    #[test]
    fn idle_dialog_info_is_empty_full_state() {
        let x = render_dialog_info(3, "sip:200@h", "h", &[]);
        assert!(x.contains("version=\"3\" state=\"full\" entity=\"sip:200@h\""));
        assert!(!x.contains("<dialog "));
    }

    #[test]
    fn dialog_info_renders_each_state_and_direction() {
        for (st, name) in [
            (DialogState::Trying, "trying"),
            (DialogState::Early, "early"),
            (DialogState::Confirmed, "confirmed"),
            (DialogState::Terminated, "terminated"),
        ] {
            let d = DialogInfo {
                id: "c1".into(),
                state: st,
                initiator: false,
                local: "200".into(),
                remote: "100".into(),
            };
            let x = render_dialog_info(1, "sip:200@h", "h", &[d]);
            assert!(x.contains(&format!("<state>{name}</state>")), "{x}");
            assert!(x.contains("direction=\"recipient\""));
            assert!(x.contains("<identity>sip:100@h</identity>"));
        }
    }

    #[test]
    fn xml_is_escaped() {
        let x = render_dialog_info(1, "sip:a\"b@h", "h", &[]);
        assert!(x.contains("sip:a&quot;b@h"));
        assert!(render_pidf("sip:1@h", true).contains("<basic>open</basic>"));
        assert!(render_pidf("sip:1@h", false).contains("<basic>closed</basic>"));
    }

    #[test]
    fn call_events_drive_dialog_states() {
        let hub = BlfHub::new();
        let ch = hub.on_event(&ev("CallStarted", "c1", "sip:100@h", "sip:200@h"));
        assert_eq!(ch, vec!["100", "200"]);
        assert_eq!(hub.dialogs_for("200")[0].state, DialogState::Trying);
        hub.on_event(&ev("CallRinging", "c1", "", ""));
        let d = hub.dialogs_for("200");
        assert_eq!(d[0].state, DialogState::Early);
        assert!(!d[0].initiator);
        assert!(hub.dialogs_for("100")[0].initiator);
        hub.on_event(&ev("CallAnswered", "c1", "", ""));
        assert_eq!(hub.dialogs_for("200")[0].state, DialogState::Confirmed);
        assert_eq!(hub.on_event(&ev("CallEnded", "c1", "", "")), vec!["100", "200"]);
        assert!(hub.dialogs_for("200").is_empty());
        // Unknown call / unrelated events are no-ops.
        assert!(hub.on_event(&ev("CallEnded", "zz", "", "")).is_empty());
        assert!(hub.on_event(&json!({"type": "X"})).is_empty());
    }

    #[test]
    fn transfer_moves_the_lamp_to_the_target() {
        let hub = BlfHub::new();
        hub.on_event(&ev("CallStarted", "c1", "100", "200"));
        hub.on_event(&ev("CallAnswered", "c1", "", ""));
        // 200 (callee) blind-transfers 100 to 300.
        let ch = hub.on_event(&ev("CallTransferred", "c1", "sip:200@h", "sip:300@h"));
        assert_eq!(ch, vec!["100", "200", "300"]);
        assert!(hub.dialogs_for("200").is_empty());
        assert_eq!(hub.dialogs_for("300").len(), 1);
        assert_eq!(hub.dialogs_for("100")[0].remote, "300");
        // Unknown transferor: no change.
        assert!(hub.on_event(&ev("CallTransferred", "c1", "999", "400")).is_empty());
    }

    #[test]
    fn per_ip_cap_applies_across_watcher_identities() {
        let hub = BlfHub::new();
        let now = Instant::now();
        for i in 0..MAX_PER_IP {
            hub.subscribe(sub(&format!("c{i}"), &format!("w{i}"), "200", 60, now)).unwrap();
        }
        assert_eq!(
            hub.subscribe(sub("over", "fresh", "200", 60, now)),
            Err(BlfError::TooManySubscriptions)
        );
    }

    #[test]
    fn expiry_sweep_and_refresh() {
        let hub = BlfHub::new();
        let now = Instant::now();
        hub.subscribe(sub("a", "100", "200", 60, now)).unwrap();
        hub.subscribe(sub("b", "100", "201", 600, now)).unwrap();
        assert_eq!(hub.count(), 2);
        let later = now + Duration::from_secs(61);
        let dead = hub.sweep(later);
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].key.call_id, "a");
        assert_eq!(hub.count(), 1);
        let key = SubKey { call_id: "b".into(), remote_tag: "r".into() };
        hub.refresh(&key, 10, later).unwrap();
        assert_eq!(hub.sweep(later + Duration::from_secs(11)).len(), 1);
        assert!(hub.refresh(&key, 10, later).is_none());
    }

    #[test]
    fn per_watcher_cap_and_counters() {
        let hub = BlfHub::new();
        let now = Instant::now();
        for i in 0..MAX_PER_WATCHER {
            hub.subscribe(sub(&format!("c{i}"), "100", "200", 60, now)).unwrap();
        }
        assert_eq!(
            hub.subscribe(sub("over", "100", "200", 60, now)),
            Err(BlfError::TooManySubscriptions)
        );
        // Another watcher is unaffected; a refresh of an existing key is not capped.
        hub.subscribe(sub("x", "101", "200", 60, now)).unwrap();
        hub.subscribe(sub("c0", "100", "200", 60, now)).unwrap();
        let key = SubKey { call_id: "c0".into(), remote_tag: "r".into() };
        let t1 = hub.prepare_notify(&key, now).unwrap();
        let t2 = hub.prepare_notify(&key, now).unwrap();
        assert_eq!((t1.version, t1.cseq, t2.version, t2.cseq), (1, 1, 2, 2));
        assert_eq!(hub.watchers_of("200", EventKind::Dialog).len(), MAX_PER_WATCHER + 1);
    }

    #[test]
    fn parsing_helpers() {
        assert_eq!(EventKind::parse("Dialog;foo=1"), Some(EventKind::Dialog));
        assert_eq!(EventKind::parse("presence"), Some(EventKind::Presence));
        assert_eq!(EventKind::parse("refer"), None);
        assert_eq!(clamp_expires(0), 0);
        assert_eq!(clamp_expires(5), MIN_EXPIRES);
        assert_eq!(clamp_expires(99999), MAX_EXPIRES);
        assert_eq!(ext_of("\"A\" <sip:200@h:5060>;tag=1").as_deref(), Some("200"));
        assert_eq!(ext_of("sip:200@h").as_deref(), Some("200"));
    }
}
