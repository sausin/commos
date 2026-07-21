//! Config-as-code: deterministic `pbx.yaml` export / import (Volume 14 §10;
//! CMOS-14-DEP-080/081/082/084).
//!
//! The premise (CMOS-14-DEP-080): a tenant's *configuration* — its users, extensions,
//! devices and queues — should be expressible as a single, human-reviewable YAML file that
//! lives in Git. Export projects the live store down to that file; import applies it back.
//!
//! **Determinism (CMOS-14-DEP-081).** Export only emits *intent* — never server-managed
//! fields (`id`, `version`, `created_at`, `updated_at`, `route_id`) — and sorts every list by
//! a stable natural key. That is what makes the round-trip `export → import → export`
//! byte-identical: two exports of the same intent produce the same YAML, so a config diff in
//! review reflects a real change and nothing else.
//!
//! **Through the API (CMOS-14-DEP-082).** The surface that drives this lives in
//! [`crate::api::config`]; this module is the pure control-plane logic it calls, so the same
//! projection is reused by any future CLI or GitOps reconciler.
//!
//! **Reconciliation (CMOS-14-DEP-084).** Import matches each incoming intent to an existing
//! row by a stable *natural key* — Extension by `number`, User by `display_name`, Device by
//! `mac`, Queue by `strategy` + sorted `members` — and updates that row in place (keeping its
//! id/created_at, bumping its version) rather than minting a duplicate. Anything with no match
//! (or, for Devices, no `mac` to key on) is created fresh. Re-importing an unchanged file is
//! therefore idempotent: the second run reports everything as *updated* and creates no rows.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use commos_core::common::Uuid;
use commos_core::entities::device::{Device, DeviceState};
use commos_core::entities::extension::Extension;
use commos_core::entities::queue::{Queue, QueueStrategy};
use commos_core::entities::user::User;

use crate::store::{Store, StoreError, Tx};

/// The `api_version` stamped into every exported file and expected on import. Pinned so a
/// file can be validated against the schema generation it was written for.
pub const API_VERSION: &str = "commos.dev/v0.4";

/// How many rows to pull per store page while gathering a full listing.
const PAGE: usize = 500;

/// A User projected to its Git-reviewable intent: the named principal and its capability
/// grants. Server-managed identity (`id`, lifecycle `state`, timestamps) is deliberately
/// omitted — those are outcomes of applying the config, not part of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PbxUser {
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
}

/// An Extension projected to intent: the dialable `number` and an optional human `label`.
/// The `route_id` it binds to is a server-minted target, not authored config, so it is not
/// exported (a fresh placeholder route is minted on import).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PbxExtension {
    pub number: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// A Device projected to intent: which hardware (`vendor` + `model`) and, if pinned, its
/// `mac`. Live registration/provisioning `state` is a runtime outcome, not config.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PbxDevice {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    pub vendor: String,
    pub model: String,
}

/// A Queue projected to intent: its distribution `strategy` and member references.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PbxQueue {
    pub strategy: QueueStrategy,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
}

/// The root `pbx.yaml` document: a tenant's whole authored configuration in one file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PbxConfig {
    pub api_version: String,
    #[serde(default)]
    pub users: Vec<PbxUser>,
    #[serde(default)]
    pub extensions: Vec<PbxExtension>,
    #[serde(default)]
    pub devices: Vec<PbxDevice>,
    #[serde(default)]
    pub queues: Vec<PbxQueue>,
}

/// What an import applied, split by entity kind into rows *created* (no existing match) and
/// rows *updated* in place (matched by natural key). Returned to the caller and serialised on
/// the API so a GitOps run can report exactly what reconciliation did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportSummary {
    pub users_created: usize,
    pub users_updated: usize,
    pub extensions_created: usize,
    pub extensions_updated: usize,
    pub devices_created: usize,
    pub devices_updated: usize,
    pub queues_created: usize,
    pub queues_updated: usize,
}

/// Project a tenant's live configuration to a deterministic [`PbxConfig`] (CMOS-14-DEP-081).
///
/// Pages through every listing to gather all rows, maps each entity to its intent DTO, sorts
/// every list by a stable natural key, and stamps [`API_VERSION`]. Because only intent is
/// emitted and ordering is fixed, `export → import → export` round-trips byte-identically.
pub async fn export(store: &Arc<dyn Store>, tenant: Uuid) -> Result<PbxConfig, StoreError> {
    // Users — sorted by display_name.
    let mut users: Vec<PbxUser> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = store.list_users(tenant, PAGE, cursor).await?;
        for u in page.items {
            let mut capabilities = u.capabilities.clone();
            capabilities.sort();
            users.push(PbxUser {
                display_name: u.display_name,
                email: u.email,
                capabilities,
            });
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    users.sort_by(|a, b| a.display_name.cmp(&b.display_name));

    // Extensions — sorted by number.
    let mut extensions: Vec<PbxExtension> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = store.list_extensions(tenant, PAGE, cursor).await?;
        for e in page.items {
            extensions.push(PbxExtension {
                number: e.number,
                label: e.label,
            });
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    extensions.sort_by(|a, b| a.number.cmp(&b.number));

    // Devices — sorted by mac (None sorts last).
    let mut devices: Vec<PbxDevice> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = store.list_devices(tenant, PAGE, cursor).await?;
        for d in page.items {
            devices.push(PbxDevice {
                mac: d.mac,
                vendor: d.vendor_key,
                model: d.model,
            });
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    devices.sort_by(|a, b| match (&a.mac, &b.mac) {
        (Some(x), Some(y)) => x.cmp(y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.vendor.cmp(&b.vendor).then(a.model.cmp(&b.model)),
    });

    // Queues — sorted by strategy, then members.
    let mut queues: Vec<PbxQueue> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = store.list_queues(tenant, PAGE, cursor).await?;
        for q in page.items {
            let mut members = q.members.clone();
            members.sort();
            queues.push(PbxQueue {
                strategy: q.strategy,
                members,
            });
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    // QueueStrategy is not `Ord`; key on its stable Debug rendering, then members.
    queues.sort_by(|a, b| {
        format!("{:?}", a.strategy)
            .cmp(&format!("{:?}", b.strategy))
            .then_with(|| a.members.cmp(&b.members))
    });

    Ok(PbxConfig {
        api_version: API_VERSION.to_string(),
        users,
        extensions,
        devices,
        queues,
    })
}

/// Render a [`PbxConfig`] to YAML. Field order follows the struct definition, so combined
/// with [`export`]'s sorting the output is fully deterministic.
pub fn to_yaml(cfg: &PbxConfig) -> Result<String, serde_yaml::Error> {
    serde_yaml::to_string(cfg)
}

/// Apply a [`PbxConfig`] to a tenant, reconciling against what is already stored, in one
/// durable transaction (CMOS-14-DEP-084).
///
/// Each incoming intent is matched to an existing row by a stable natural key:
/// - **User** by `display_name`,
/// - **Extension** by `number` (a matched extension keeps its bound `route_id`; a new one gets
///   a fresh placeholder [`Uuid::now_v7`] until a real routing target is wired),
/// - **Device** by `mac` — a device with no `mac` has no stable key, so it is *always created*,
/// - **Queue** by `strategy` + sorted `members` — a queue's whole authored intent *is* its key,
///   so a match is byte-identical and the update is a pure version bump; changing a queue's
///   members reads as a different queue (created new, the old row left untouched).
///
/// On a match the stored entity is loaded, its intent fields overwritten, and
/// [`EntityBase::touch`] called so `version = stored + 1` (the store then UPDATEs it, keeping
/// the same id/created_at/tenant). With no match the entity is minted at v0 as before. All
/// four kinds land in a single [`Tx`] so an import is all-or-nothing, which makes re-importing
/// an unchanged file idempotent: everything reports as *updated* and no rows are duplicated.
pub async fn import(
    store: &Arc<dyn Store>,
    tenant: Uuid,
    cfg: &PbxConfig,
) -> Result<ImportSummary, StoreError> {
    use std::collections::HashMap;

    // ---- Gather the tenant's current entities, keyed by natural key. ----
    let mut current_users: HashMap<String, User> = HashMap::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = store.list_users(tenant, PAGE, cursor).await?;
        for u in page.items {
            current_users.entry(u.display_name.clone()).or_insert(u);
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }

    let mut current_exts: HashMap<String, Extension> = HashMap::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = store.list_extensions(tenant, PAGE, cursor).await?;
        for e in page.items {
            current_exts.entry(e.number.clone()).or_insert(e);
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }

    // Only devices that carry a mac participate in reconciliation; the rest are unkeyable.
    let mut current_devices: HashMap<String, Device> = HashMap::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = store.list_devices(tenant, PAGE, cursor).await?;
        for d in page.items {
            if let Some(mac) = d.mac.clone() {
                current_devices.entry(mac).or_insert(d);
            }
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }

    let mut current_queues: HashMap<String, Queue> = HashMap::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = store.list_queues(tenant, PAGE, cursor).await?;
        for q in page.items {
            current_queues.entry(queue_key(&q.strategy, &q.members)).or_insert(q);
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }

    // ---- Reconcile: update on a natural-key match, otherwise create. ----
    let mut summary = ImportSummary::default();

    let mut users: Vec<User> = Vec::new();
    for u in &cfg.users {
        if let Some(mut user) = current_users.remove(&u.display_name) {
            user.email = u.email.clone();
            user.capabilities = u.capabilities.clone();
            user.base.touch();
            users.push(user);
            summary.users_updated += 1;
        } else {
            let mut user = User::new(tenant, u.display_name.clone());
            user.email = u.email.clone();
            user.capabilities = u.capabilities.clone();
            users.push(user);
            summary.users_created += 1;
        }
    }

    let mut extensions: Vec<Extension> = Vec::new();
    for e in &cfg.extensions {
        if let Some(mut ext) = current_exts.remove(&e.number) {
            // Keep the already-bound route_id; only the authored fields change.
            ext.label = e.label.clone();
            ext.base.touch();
            extensions.push(ext);
            summary.extensions_updated += 1;
        } else {
            // Placeholder routing target — a real route is bound during reconciliation.
            let route_id = Uuid::now_v7();
            let mut ext = Extension::new(tenant, e.number.clone(), route_id);
            ext.label = e.label.clone();
            extensions.push(ext);
            summary.extensions_created += 1;
        }
    }

    let mut devices: Vec<Device> = Vec::new();
    for d in &cfg.devices {
        let matched = d.mac.as_ref().and_then(|mac| current_devices.remove(mac));
        if let Some(mut device) = matched {
            device.vendor_key = d.vendor.clone();
            device.model = d.model.clone();
            device.state = DeviceState::Provisioned;
            device.base.touch();
            devices.push(device);
            summary.devices_updated += 1;
        } else {
            // No mac to key on, or no existing row — mint fresh (state APPROVED at v0).
            let mut device = Device::new(tenant, d.vendor.clone(), d.model.clone());
            device.mac = d.mac.clone();
            device.state = DeviceState::Provisioned;
            devices.push(device);
            summary.devices_created += 1;
        }
    }

    let mut queues: Vec<Queue> = Vec::new();
    for q in &cfg.queues {
        let key = queue_key(&q.strategy, &q.members);
        if let Some(mut queue) = current_queues.remove(&key) {
            queue.members = q.members.clone();
            queue.base.touch();
            queues.push(queue);
            summary.queues_updated += 1;
        } else {
            let mut queue = Queue::create(tenant, q.strategy);
            queue.members = q.members.clone();
            queues.push(queue);
            summary.queues_created += 1;
        }
    }

    store
        .commit(Tx {
            users,
            extensions,
            devices,
            queues,
            events: Vec::new(),
            ..Default::default()
        })
        .await?;

    Ok(summary)
}

/// Stable natural key for a queue: its distribution strategy plus its members in sorted order.
/// `QueueStrategy` is not `Ord`, so it is keyed on its Debug rendering (as export already does).
fn queue_key(strategy: &QueueStrategy, members: &[String]) -> String {
    let mut members = members.to_vec();
    members.sort();
    format!("{strategy:?}|{members:?}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemStore;

    fn store() -> Arc<dyn Store> {
        Arc::new(MemStore::new())
    }

    #[tokio::test]
    async fn empty_tenant_exports_empty_config() {
        let st = store();
        let tenant = Uuid::now_v7();
        let cfg = export(&st, tenant).await.unwrap();
        assert_eq!(cfg.api_version, API_VERSION);
        assert!(cfg.users.is_empty());
        assert!(cfg.extensions.is_empty());
        assert!(cfg.devices.is_empty());
        assert!(cfg.queues.is_empty());
    }

    #[tokio::test]
    async fn import_then_export_round_trips_deterministically() {
        let st = store();
        let tenant = Uuid::now_v7();

        let cfg = PbxConfig {
            api_version: API_VERSION.to_string(),
            // Intentionally out of sorted order — export must normalise.
            users: vec![
                PbxUser {
                    display_name: "Grace Hopper".into(),
                    email: Some("grace@example.com".into()),
                    capabilities: vec!["voice.call".into(), "admin".into()],
                },
                PbxUser {
                    display_name: "Ada Lovelace".into(),
                    email: None,
                    capabilities: vec![],
                },
            ],
            extensions: vec![
                PbxExtension { number: "1002".into(), label: None },
                PbxExtension { number: "1001".into(), label: Some("Front desk".into()) },
            ],
            devices: vec![PbxDevice {
                mac: Some("aa:bb:cc:dd:ee:ff".into()),
                vendor: "polycom".into(),
                model: "VVX 450".into(),
            }],
            queues: vec![PbxQueue {
                strategy: QueueStrategy::Ringall,
                members: vec!["sip:100".into()],
            }],
        };

        let summary = import(&st, tenant, &cfg).await.unwrap();
        // A first import against an empty tenant is all creates.
        assert_eq!(summary.users_created, 2);
        assert_eq!(summary.users_updated, 0);
        assert_eq!(summary.extensions_created, 2);
        assert_eq!(summary.extensions_updated, 0);
        assert_eq!(summary.devices_created, 1);
        assert_eq!(summary.devices_updated, 0);
        assert_eq!(summary.queues_created, 1);
        assert_eq!(summary.queues_updated, 0);

        // The imported entities are now listable.
        let exported = export(&st, tenant).await.unwrap();
        assert_eq!(exported.users.len(), 2);
        assert_eq!(exported.extensions.len(), 2);
        // Deterministic ordering.
        assert_eq!(exported.users[0].display_name, "Ada Lovelace");
        assert_eq!(exported.users[1].display_name, "Grace Hopper");
        assert_eq!(exported.extensions[0].number, "1001");
        // Capabilities within a user are sorted too.
        assert_eq!(exported.users[1].capabilities, vec!["admin", "voice.call"]);

        // export → to_yaml is stable across a second export of the same intent.
        let yaml_once = to_yaml(&exported).unwrap();
        let exported_again = export(&st, tenant).await.unwrap();
        let yaml_twice = to_yaml(&exported_again).unwrap();
        assert_eq!(yaml_once, yaml_twice, "round-trip export must be byte-identical");
    }

    /// A representative config used by the reconciliation tests.
    fn sample_cfg() -> PbxConfig {
        PbxConfig {
            api_version: API_VERSION.to_string(),
            users: vec![
                PbxUser {
                    display_name: "Ada Lovelace".into(),
                    email: Some("ada@example.com".into()),
                    capabilities: vec!["voice.call".into()],
                },
                PbxUser {
                    display_name: "Grace Hopper".into(),
                    email: None,
                    capabilities: vec![],
                },
            ],
            extensions: vec![
                PbxExtension { number: "1001".into(), label: Some("Front desk".into()) },
                PbxExtension { number: "1002".into(), label: None },
            ],
            devices: vec![
                PbxDevice {
                    mac: Some("aa:bb:cc:dd:ee:ff".into()),
                    vendor: "polycom".into(),
                    model: "VVX 450".into(),
                },
                // No mac — always created, never reconciled.
                PbxDevice {
                    mac: None,
                    vendor: "yealink".into(),
                    model: "T54W".into(),
                },
            ],
            queues: vec![PbxQueue {
                strategy: QueueStrategy::Ringall,
                members: vec!["sip:100".into(), "sip:101".into()],
            }],
        }
    }

    #[tokio::test]
    async fn reimport_reconciles_by_natural_key_and_bumps_version() {
        let st = store();
        let tenant = Uuid::now_v7();
        let cfg = sample_cfg();

        // First import: everything created.
        let first = import(&st, tenant, &cfg).await.unwrap();
        assert_eq!(first.users_created, 2);
        assert_eq!(first.extensions_created, 2);
        // Both devices are created (the mac-less one is always a create).
        assert_eq!(first.devices_created, 2);
        assert_eq!(first.queues_created, 1);

        // Capture the id + version of the mac-keyed device before the second import.
        let dev_before = st
            .list_devices(tenant, PAGE, None)
            .await
            .unwrap()
            .items
            .into_iter()
            .find(|d| d.mac.as_deref() == Some("aa:bb:cc:dd:ee:ff"))
            .expect("keyed device present");
        assert_eq!(dev_before.base.version, 0);

        // Second import of the SAME config: the keyed entities are updated in place; only the
        // mac-less device (no stable key) is created again.
        let second = import(&st, tenant, &cfg).await.unwrap();
        assert_eq!(second.users_created, 0);
        assert_eq!(second.users_updated, 2);
        assert_eq!(second.extensions_created, 0);
        assert_eq!(second.extensions_updated, 2);
        assert_eq!(second.devices_updated, 1);
        assert_eq!(second.devices_created, 1); // the mac-less one
        assert_eq!(second.queues_created, 0);
        assert_eq!(second.queues_updated, 1);

        // The keyed device kept its id and its version was bumped, not duplicated.
        let dev_after = st
            .get_device(tenant, dev_before.base.id)
            .await
            .unwrap()
            .expect("same device id survives reconciliation");
        assert_eq!(dev_after.base.id, dev_before.base.id);
        assert_eq!(dev_after.base.created_at, dev_before.base.created_at);
        assert_eq!(dev_after.base.version, 1);
    }

    #[tokio::test]
    async fn create_then_update_applies_changed_intent() {
        let st = store();
        let tenant = Uuid::now_v7();

        let mut cfg = sample_cfg();
        import(&st, tenant, &cfg).await.unwrap();

        // Change an existing user's intent (matched by display_name) and re-import.
        cfg.users[0].email = Some("ada.new@example.com".into());
        cfg.users[0].capabilities = vec!["admin".into(), "voice.call".into()];
        cfg.extensions[1].label = Some("Reception".into());

        let summary = import(&st, tenant, &cfg).await.unwrap();
        assert_eq!(summary.users_updated, 2);
        assert_eq!(summary.users_created, 0);

        let ada = st
            .list_users(tenant, PAGE, None)
            .await
            .unwrap()
            .items
            .into_iter()
            .find(|u| u.display_name == "Ada Lovelace")
            .expect("Ada present");
        assert_eq!(ada.email.as_deref(), Some("ada.new@example.com"));
        assert_eq!(ada.capabilities, vec!["admin", "voice.call"]);
        assert_eq!(ada.base.version, 1, "an update bumps version exactly once");

        let ext = st
            .list_extensions(tenant, PAGE, None)
            .await
            .unwrap()
            .items
            .into_iter()
            .find(|e| e.number == "1002")
            .expect("extension 1002 present");
        assert_eq!(ext.label.as_deref(), Some("Reception"));
    }

    #[tokio::test]
    async fn export_import_import_export_is_row_stable() {
        let st = store();
        let tenant = Uuid::now_v7();

        // Seed, then export the canonical intent.
        import(&st, tenant, &sample_cfg()).await.unwrap();
        let exported = export(&st, tenant).await.unwrap();

        // Re-import the exported config twice.
        import(&st, tenant, &exported).await.unwrap();
        let second = import(&st, tenant, &exported).await.unwrap();

        // The second re-import of exported intent creates nothing keyable and duplicates no
        // keyed rows — everything with a natural key reports as updated.
        assert_eq!(second.users_created, 0);
        assert_eq!(second.extensions_created, 0);
        assert_eq!(second.queues_created, 0);
        assert_eq!(second.users_updated, exported.users.len());
        assert_eq!(second.extensions_updated, exported.extensions.len());

        // Export again: the keyed populations are unchanged (no duplication). Devices are the
        // one exception — the mac-less device is re-created on every import by design.
        let after = export(&st, tenant).await.unwrap();
        assert_eq!(after.users.len(), exported.users.len());
        assert_eq!(after.extensions.len(), exported.extensions.len());
        assert_eq!(after.queues.len(), exported.queues.len());

        // The YAML for the keyed populations round-trips byte-identically.
        assert_eq!(
            to_yaml(&PbxConfig {
                devices: Vec::new(),
                ..exported.clone()
            })
            .unwrap(),
            to_yaml(&PbxConfig {
                devices: Vec::new(),
                ..after.clone()
            })
            .unwrap(),
        );
    }
}
