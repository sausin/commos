//! Voicemail-to-email — email a copy of each new voicemail to the mailbox owner.
//!
//! This is a **bus subscriber**, the same shape as the webhook dispatcher and the metrics
//! collector: it watches the event stream and reacts to `VoicemailReceived` rather than
//! coupling into the voicemail deposit path. When one fires it resolves the mailbox, looks
//! up the configured recipient(s), fetches the stored audio, and sends it through the
//! pure-Rust [`smtp`](crate::control::smtp) client with the recording attached as a WAV.
//!
//! ## Resolving the recipient
//!
//! There is no `extension number → User → email` link in the domain today (an `Extension`
//! carries no owner, and `Voicemail.user_id` is unset on the SIP deposit path). So — exactly
//! like Asterisk's `voicemail.conf` `mailbox => …,email` mapping — the recipient is resolved
//! from a **config map keyed by the mailbox (extension) number**, which is recovered from the
//! originating Call's `to_ref` user-part, the same key `mailbox_summary` /
//! `list_for_mailbox` already use. A voicemail whose mailbox has no configured recipient is
//! simply skipped.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::watch;

use commos_core::common::Uuid;
use commos_core::entities::user::User;

use crate::bus::EventBus;
use crate::control::smtp::{self, Attachment, Email, SmtpTransport};
use crate::control::voicemail::VoicemailService;
use crate::store::Store;

/// Emails new voicemails to their mailbox owner. Cheap to clone (handles + small maps).
#[derive(Clone)]
pub struct VoicemailEmailer {
    transport: SmtpTransport,
    /// Mailbox (extension) number → recipient email address(es).
    mailboxes: HashMap<String, Vec<String>>,
    attach_audio: bool,
    store: Arc<dyn Store>,
    voicemails: VoicemailService,
}

/// The resolved context for one deliverable voicemail.
struct Resolved {
    recipients: Vec<String>,
    mailbox: String,
    caller: String,
    duration_ms: Option<u64>,
    object_id: Uuid,
}

impl VoicemailEmailer {
    pub fn new(
        transport: SmtpTransport,
        mailboxes: HashMap<String, Vec<String>>,
        attach_audio: bool,
        store: Arc<dyn Store>,
        voicemails: VoicemailService,
    ) -> Self {
        VoicemailEmailer { transport, mailboxes, attach_audio, store, voicemails }
    }

    /// Split a config `"a@x.com, b@y.com"` recipient string into a de-blanked list.
    pub fn parse_recipients(raw: &str) -> Vec<String> {
        raw.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Run the dispatcher: subscribe to the bus and email each `VoicemailReceived` until
    /// shutdown. Spawned once from `main` (only when `smtp:` is configured).
    pub async fn run(self, bus: EventBus, mut shutdown: watch::Receiver<bool>) {
        let mut rx = bus.subscribe();
        tracing::info!(relay = %self.transport.host, "voicemail-to-email dispatcher started");
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                recv = rx.recv() => match recv {
                    Ok(event) => self.handle_event(&event).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "voicemail-email dispatcher lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
        tracing::info!("voicemail-to-email dispatcher stopped");
    }

    async fn handle_event(&self, event: &serde_json::Value) {
        if event.get("type").and_then(|v| v.as_str()) != Some("VoicemailReceived") {
            return;
        }
        let tenant = event.get("tenant_id").and_then(|v| v.as_str()).and_then(|s| Uuid::parse(s).ok());
        let vm_id = event
            .get("data")
            .and_then(|d| d.get("voicemail_id"))
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse(s).ok());
        let (Some(tenant), Some(vm_id)) = (tenant, vm_id) else {
            tracing::warn!("VoicemailReceived event missing tenant_id/voicemail_id");
            return;
        };
        if let Err(e) = self.deliver(tenant, vm_id).await {
            tracing::warn!(error = %e, %vm_id, "voicemail-to-email delivery failed");
        }
    }

    /// Resolve, build, and send the email for one voicemail. Returns whether an email was
    /// sent (`false` = intentionally skipped: no mailbox / no configured recipient).
    async fn deliver(&self, tenant: Uuid, vm_id: Uuid) -> anyhow::Result<bool> {
        let Some(r) = self.resolve(tenant, vm_id).await? else {
            return Ok(false);
        };
        let attachment = if self.attach_audio {
            match self.voicemails.get_audio(tenant, vm_id).await {
                Ok((_vm, ulaw)) => Some(Attachment {
                    filename: "voicemail.wav".to_string(),
                    content_type: "audio/wav".to_string(),
                    bytes: smtp::wav_ulaw(&ulaw),
                }),
                Err(e) => {
                    // Still send the notification even if the audio could not be fetched.
                    tracing::warn!(error = %e, %vm_id, "voicemail audio fetch failed; sending without attachment");
                    None
                }
            }
        } else {
            None
        };
        let _ = r.object_id; // object id is reached via get_audio; kept for observability/future.
        let email = build_email(&r, attachment);
        smtp::send(&self.transport, &email).await?;
        tracing::info!(%vm_id, mailbox = %r.mailbox, recipients = r.recipients.len(), "voicemail emailed");
        Ok(true)
    }

    /// The directory email of the user who owns extension `number`.
    ///
    /// Preferred: the explicit link the admin console writes - an `ext:<number>` entry in
    /// `User.capabilities`. Fallback: the User whose `display_name` equals the extension's
    /// `label` (or the onboarding default `"Extension <number>"`), used only when exactly one
    /// user matches (names are not unique). Ambiguity => no email is sent (logged).
    async fn owner_email(&self, tenant: Uuid, number: &str) -> anyhow::Result<Option<String>> {
        let mut names: Vec<String> = vec![format!("Extension {number}")];
        let mut cursor: Option<String> = None;
        loop {
            let page = self.store.list_extensions(tenant, 500, cursor).await?;
            if let Some(e) = page.items.iter().find(|e| e.number == number) {
                if let Some(l) = &e.label {
                    names.insert(0, l.clone());
                }
                break;
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        let mut users = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = self.store.list_users(tenant, 500, cursor).await?;
            users.extend(page.items);
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        Ok(pick_owner_email(&users, number, &names))
    }

    /// Look up the mailbox + recipients for a voicemail, or `None` if it is not deliverable.
    async fn resolve(&self, tenant: Uuid, vm_id: Uuid) -> anyhow::Result<Option<Resolved>> {
        let Some(vm) = self.store.get_voicemail(tenant, vm_id).await? else {
            return Ok(None);
        };
        // The mailbox is the dialled extension — the originating Call's to_ref user-part.
        let call = match vm.call_id {
            Some(cid) => self.store.get_call(tenant, cid).await?,
            None => None,
        };
        let Some(call) = call else {
            return Ok(None);
        };
        let Some(mailbox) = user_part(&call.to_ref) else {
            return Ok(None);
        };
        let mut recipients = self.mailboxes.get(&mailbox).cloned().unwrap_or_default();
        if recipients.is_empty() {
            // Not listed in `smtp.mailboxes`: fall back to the owning user's directory email.
            if let Some(email) = self.owner_email(tenant, &mailbox).await? {
                recipients = vec![email];
            }
        }
        if recipients.is_empty() {
            return Ok(None);
        }
        let caller = user_part(&call.from_ref).unwrap_or_else(|| "unknown".to_string());
        Ok(Some(Resolved {
            recipients,
            mailbox,
            caller,
            duration_ms: vm.duration_ms,
            object_id: vm.object_id,
        }))
    }
}

/// Compose the notification email (subject + body) for a resolved voicemail.
fn build_email(r: &Resolved, attachment: Option<Attachment>) -> Email {
    let secs = r.duration_ms.map(|ms| ms.div_ceil(1000)).unwrap_or(0);
    let subject = format!("New voicemail from {} ({}s)", r.caller, secs);
    let body = format!(
        "You have a new voicemail in mailbox {}.\r\n\r\nFrom: {}\r\nDuration: {} seconds\r\n{}",
        r.mailbox,
        r.caller,
        secs,
        if attachment.is_some() {
            "\r\nThe message is attached as an audio file."
        } else {
            ""
        },
    );
    Email { to: r.recipients.clone(), subject, text_body: body, attachment }
}

/// Extract a SIP/tel URI's user-part (the extension number), mirroring the voicemail
/// service's own matcher. Returns an owned string for convenience across await points.
fn user_part(uri: &str) -> Option<String> {
    let s = uri
        .trim()
        .trim_start_matches('<')
        .trim_start_matches("sips:")
        .trim_start_matches("sip:")
        .trim_start_matches("tel:");
    let user = s.split(['@', ';', '>']).next().unwrap_or(s).trim();
    (!user.is_empty()).then(|| user.to_string())
}

/// Pure owner selection for `owner_email`; see its docs for the policy.
fn pick_owner_email(users: &[User], number: &str, names: &[String]) -> Option<String> {
    let email_of = |u: &User| {
        u.email.as_ref().map(|e| e.trim()).filter(|e| !e.is_empty()).map(str::to_string)
    };
    let link = format!("ext:{number}");
    let linked: Vec<&User> =
        users.iter().filter(|u| u.capabilities.contains(&link)).collect();
    if !linked.is_empty() {
        let mut emails: Vec<String> = linked.iter().filter_map(|u| email_of(u)).collect();
        emails.sort();
        emails.dedup();
        return match emails.len() {
            1 => emails.pop(),
            0 => None,
            _ => {
                tracing::warn!(%number, "several users are linked to this extension; not sending voicemail email");
                None
            }
        };
    }
    let matches: Vec<&User> =
        users.iter().filter(|u| names.iter().any(|n| &u.display_name == n)).collect();
    match matches.len() {
        0 => None,
        1 => email_of(matches[0]),
        _ => {
            tracing::warn!(%number, "display-name match is ambiguous; not sending voicemail email");
            None
        }
    }
}

#[cfg(test)]
mod owner_tests {
    use super::*;

    fn user(name: &str, email: &str, caps: &[&str]) -> User {
        let mut u = User::new(Uuid::now_v7(), name);
        u.email = Some(email.into());
        u.capabilities = caps.iter().map(|c| c.to_string()).collect();
        u
    }

    #[test]
    fn explicit_ext_link_wins_over_display_name() {
        let users = vec![
            user("Sam", "wrong@x", &[]),
            user("Samantha", "right@x", &["ext:200"]),
        ];
        assert_eq!(pick_owner_email(&users, "200", &["Sam".into()]).as_deref(), Some("right@x"));
    }

    #[test]
    fn display_name_only_when_unique() {
        let names = vec!["Sam".to_string()];
        let one = vec![user("Sam", "a@x", &[])];
        assert_eq!(pick_owner_email(&one, "200", &names).as_deref(), Some("a@x"));
        let two = vec![user("Sam", "a@x", &[]), user("Sam", "b@x", &[])];
        assert_eq!(pick_owner_email(&two, "200", &names), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::objects::ObjectService;
    use crate::control::voicemail::VoicemailService;
    use crate::relay::RelaySignal;
    use crate::store::{MemStore, Tx};
    use commos_core::entities::call::{Call, Direction};
    use commos_core::entities::voicemail::Voicemail;

    fn transport() -> SmtpTransport {
        SmtpTransport {
            host: "127.0.0.1".into(),
            port: 25,
            from: "voicemail@commos.local".into(),
            helo: "commos.local".into(),
            auth: None,
        }
    }

    fn emailer(store: Arc<dyn Store>, mailboxes: HashMap<String, Vec<String>>) -> VoicemailEmailer {
        // A voicemail service backed by a throwaway on-disk object store (only get_audio uses
        // it, which the resolution tests do not exercise).
        let blob = Arc::new(crate::objectstore::LocalObjectStore::new(
            std::env::temp_dir().join(format!("commos-vm-email-test-{}", Uuid::now_v7())),
        ));
        let objects = ObjectService::new(blob, store.clone(), RelaySignal::new());
        let voicemails = VoicemailService::new(objects, store.clone(), RelaySignal::new());
        VoicemailEmailer::new(transport(), mailboxes, false, store, voicemails)
    }

    #[test]
    fn parse_recipients_splits_and_trims() {
        assert_eq!(
            VoicemailEmailer::parse_recipients(" a@x.com , b@y.com ,,"),
            vec!["a@x.com".to_string(), "b@y.com".to_string()]
        );
        assert!(VoicemailEmailer::parse_recipients("  ").is_empty());
    }

    #[test]
    fn user_part_extracts_extension() {
        assert_eq!(user_part("sip:100@host").as_deref(), Some("100"));
        assert_eq!(user_part("<sip:101@host;tag=x>").as_deref(), Some("101"));
        assert_eq!(user_part("102").as_deref(), Some("102"));
        assert_eq!(user_part("").as_deref(), None);
    }

    #[test]
    fn build_email_names_caller_and_mailbox_and_attaches() {
        let r = Resolved {
            recipients: vec!["alice@example.com".into()],
            mailbox: "100".into(),
            caller: "5551234".into(),
            duration_ms: Some(11_500),
            object_id: Uuid::now_v7(),
        };
        let att = Attachment { filename: "voicemail.wav".into(), content_type: "audio/wav".into(), bytes: vec![1] };
        let email = build_email(&r, Some(att));
        assert_eq!(email.to, vec!["alice@example.com".to_string()]);
        // 11_500 ms rounds up to 12 s.
        assert_eq!(email.subject, "New voicemail from 5551234 (12s)");
        assert!(email.text_body.contains("mailbox 100"));
        assert!(email.text_body.contains("attached"));
        assert!(email.attachment.is_some());
    }

    async fn seed_voicemail(store: &Arc<dyn Store>, tenant: Uuid, from: &str, to: &str) -> Uuid {
        let call = Call::originate(tenant, Direction::Inbound, from, to);
        let mut vm = Voicemail::new(tenant, Uuid::now_v7());
        vm.call_id = Some(call.base.id);
        vm.duration_ms = Some(8000);
        let vm_id = vm.base.id;
        store
            .commit(Tx { calls: vec![call], voicemails: vec![vm], ..Default::default() })
            .await
            .unwrap();
        vm_id
    }

    #[tokio::test]
    async fn resolve_matches_mailbox_to_configured_recipient() {
        let store: Arc<dyn Store> = Arc::new(MemStore::new());
        let tenant = Uuid::now_v7();
        let vm_id = seed_voicemail(&store, tenant, "sip:5551234@pstn", "sip:100@commos").await;

        let mut map = HashMap::new();
        map.insert("100".to_string(), vec!["alice@example.com".to_string()]);
        let e = emailer(store.clone(), map);

        let r = e.resolve(tenant, vm_id).await.unwrap().expect("deliverable");
        assert_eq!(r.mailbox, "100");
        assert_eq!(r.caller, "5551234");
        assert_eq!(r.recipients, vec!["alice@example.com".to_string()]);
    }

    #[tokio::test]
    async fn resolve_skips_mailbox_with_no_recipient() {
        let store: Arc<dyn Store> = Arc::new(MemStore::new());
        let tenant = Uuid::now_v7();
        let vm_id = seed_voicemail(&store, tenant, "sip:5551234@pstn", "sip:999@commos").await;
        // Map only has 100, not 999.
        let mut map = HashMap::new();
        map.insert("100".to_string(), vec!["alice@example.com".to_string()]);
        let e = emailer(store.clone(), map);
        assert!(e.resolve(tenant, vm_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn resolve_falls_back_to_owning_users_email() {
        use commos_core::entities::extension::Extension;
        use commos_core::entities::user::User;
        let store: Arc<dyn Store> = Arc::new(MemStore::new());
        let tenant = Uuid::now_v7();
        let mut ext = Extension::new(tenant, "100", Uuid::now_v7());
        ext.label = Some("Ada Lovelace".into());
        let mut user = User::new(tenant, "Ada Lovelace");
        user.email = Some("ada@example.com".into());
        store
            .commit(Tx { extensions: vec![ext], users: vec![user], ..Default::default() })
            .await
            .unwrap();
        let vm_id = seed_voicemail(&store, tenant, "sip:555@pstn", "sip:100@commos").await;
        let e = emailer(store.clone(), HashMap::new());
        let r = e.resolve(tenant, vm_id).await.unwrap().expect("falls back to directory email");
        assert_eq!(r.recipients, vec!["ada@example.com".to_string()]);
    }

    #[tokio::test]
    async fn resolve_none_for_missing_voicemail() {
        let store: Arc<dyn Store> = Arc::new(MemStore::new());
        let e = emailer(store.clone(), HashMap::new());
        assert!(e.resolve(Uuid::now_v7(), Uuid::now_v7()).await.unwrap().is_none());
    }
}
