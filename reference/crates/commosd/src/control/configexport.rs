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
//! **MVP scope.** Import *creates* fresh entities (new ids each run). Reconciliation —
//! matching an incoming intent to an existing row and updating it in place — is the
//! documented follow-up (CMOS-14-DEP-084); until then, re-importing a file duplicates rows.

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

/// What an import applied. Returned to the caller (and serialised on the API) so a GitOps
/// run can report exactly how many rows it created.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportSummary {
    pub users: usize,
    pub extensions: usize,
    pub devices: usize,
    pub queues: usize,
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

/// Apply a [`PbxConfig`] to a tenant, creating fresh entities in one durable transaction.
///
/// Every row is minted new (`EntityBase::new` via each entity constructor); an extension's
/// `route_id` is a fresh placeholder [`Uuid::now_v7`] until a real routing target is wired
/// (CMOS-14-DEP-084). All four entity kinds land in a single [`Tx`] so an import is
/// all-or-nothing. Idempotent reconciliation (update-in-place) is the documented follow-up.
pub async fn import(
    store: &Arc<dyn Store>,
    tenant: Uuid,
    cfg: &PbxConfig,
) -> Result<ImportSummary, StoreError> {
    let mut users: Vec<User> = Vec::new();
    for u in &cfg.users {
        let mut user = User::new(tenant, u.display_name.clone());
        user.email = u.email.clone();
        user.capabilities = u.capabilities.clone();
        users.push(user);
    }

    let mut extensions: Vec<Extension> = Vec::new();
    for e in &cfg.extensions {
        // Placeholder routing target — a real route is bound during reconciliation.
        let route_id = Uuid::now_v7();
        let mut ext = Extension::new(tenant, e.number.clone(), route_id);
        ext.label = e.label.clone();
        extensions.push(ext);
    }

    let mut devices: Vec<Device> = Vec::new();
    for d in &cfg.devices {
        // Device::new sets state APPROVED + version 0; set the intent fields on top.
        let mut device = Device::new(tenant, d.vendor.clone(), d.model.clone());
        device.mac = d.mac.clone();
        device.state = DeviceState::Provisioned;
        devices.push(device);
    }

    let mut queues: Vec<Queue> = Vec::new();
    for q in &cfg.queues {
        let mut queue = Queue::create(tenant, q.strategy);
        queue.members = q.members.clone();
        queues.push(queue);
    }

    let summary = ImportSummary {
        users: users.len(),
        extensions: extensions.len(),
        devices: devices.len(),
        queues: queues.len(),
    };

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
        assert_eq!(summary.users, 2);
        assert_eq!(summary.extensions, 2);
        assert_eq!(summary.devices, 1);
        assert_eq!(summary.queues, 1);

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
}
