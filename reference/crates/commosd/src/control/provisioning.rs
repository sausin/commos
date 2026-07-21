//! Provisioning (control plane) — the **directory**: the write path for people, phones,
//! extensions, and routes, and their lifecycle (Volume 3 Provisioning subsystem; Volume 8).
//!
//! This is the "manage comms" surface the operator actually drives: add a person, add or
//! approve a phone, hand out an extension, wire a route. It is the provisioning peer of
//! [`crate::control::routing::Routing`] on the same substrate — the same
//! commit-entity-with-its-event-atomically spine (CMOS-03-ARCH-030).
//!
//! Lifecycle transitions emit their canonical events (User: Created/Updated/Activated/
//! Deactivated/Suspended; Device: Detected/Approved/Rejected/Retired/ReplacementStarted).
//! Extensions and Routes are pure configuration (no lifecycle state, no catalogued event —
//! peers of Queue in this respect), so their writes commit without an event.

use std::sync::Arc;

use commos_core::common::Uuid;
use commos_core::entities::device::{Device, DeviceState};
use commos_core::entities::extension::Extension;
use commos_core::entities::route::Route;
use commos_core::entities::user::{User, UserState};
use commos_core::event::{Correlation, Envelope, EventPayload};
use commos_core::events::device_approved::DeviceApproved;
use commos_core::events::device_detected::DeviceDetected;
use commos_core::events::device_rejected::DeviceRejected;
use commos_core::events::device_replacement_started::DeviceReplacementStarted;
use commos_core::events::device_retired::DeviceRetired;
use commos_core::events::user_activated::UserActivated;
use commos_core::events::user_created::UserCreated;
use commos_core::events::user_deactivated::UserDeactivated;
use commos_core::events::user_suspended::UserSuspended;
use commos_core::events::user_updated::UserUpdated;

use crate::relay::RelaySignal;
use crate::store::{Store, StoreError, Tx};

#[derive(Debug, thiserror::Error)]
pub enum ProvisioningError {
    #[error("not found")]
    NotFound,
    #[error("illegal lifecycle transition: {0}")]
    IllegalState(String),
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

// --- Request payloads (shared with the API edge) -----------------------------------------

/// Intent for creating a User. Server-managed fields are assigned by the platform.
#[derive(Default)]
pub struct NewUser {
    pub display_name: String,
    pub email: Option<String>,
    pub capabilities: Vec<String>,
}

/// A partial update to a User. Only `Some` fields are applied.
#[derive(Default)]
pub struct UserPatch {
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub capabilities: Option<Vec<String>>,
}

/// Intent for creating (registering) a Device.
#[derive(Default)]
pub struct NewDevice {
    pub vendor_key: String,
    pub model: String,
    pub mac: Option<String>,
    pub assigned_user_id: Option<Uuid>,
}

/// A partial update to a Device's descriptive fields (not its lifecycle state).
#[derive(Default)]
pub struct DevicePatch {
    pub model: Option<String>,
    pub mac: Option<String>,
    pub assigned_user_id: Option<Uuid>,
    pub firmware: Option<String>,
}

/// Intent for creating an Extension: a dialable `number` whose route points at
/// `destination_ref` (e.g. `sip:100@commos.local`, `queue:<uuid>`).
#[derive(Default)]
pub struct NewExtension {
    pub number: String,
    pub destination_ref: String,
    pub label: Option<String>,
}

/// A partial update to an Extension. `destination_ref`, if given, re-points its route.
#[derive(Default)]
pub struct ExtensionPatch {
    pub number: Option<String>,
    pub label: Option<String>,
    pub destination_ref: Option<String>,
}

/// The Provisioning service. Stateless between requests — all state lives in the [`Store`]
/// (CMOS-03-ARCH-010).
#[derive(Clone)]
pub struct Provisioning {
    store: Arc<dyn Store>,
    signal: RelaySignal,
}

impl Provisioning {
    pub fn new(store: Arc<dyn Store>, signal: RelaySignal) -> Self {
        Provisioning { store, signal }
    }

    /// Commit a transaction and wake the relay so any events surface promptly.
    async fn commit(&self, tx: Tx) -> Result<(), ProvisioningError> {
        self.store.commit(tx).await?;
        self.signal.wake();
        Ok(())
    }

    fn event<P: EventPayload>(tenant: Uuid, payload: P, idem: String) -> serde_json::Value {
        let ctx = Correlation::root(tenant);
        Envelope::new(payload, &ctx, idem).to_json()
    }

    // --- Users ---------------------------------------------------------------------------

    /// Create a User in `ACTIVE`, emit `UserCreated`, commit atomically.
    pub async fn create_user(&self, tenant: Uuid, req: NewUser) -> Result<User, ProvisioningError> {
        if req.display_name.trim().is_empty() {
            return Err(ProvisioningError::Invalid("display_name is required".into()));
        }
        let mut user = User::new(tenant, req.display_name);
        user.email = req.email;
        user.capabilities = req.capabilities;

        let ev = Self::event(
            tenant,
            UserCreated {
                user_id: user.base.id,
                display_name: user.display_name.clone(),
                state: user.state,
            },
            format!("{}:UserCreated", user.base.id),
        );
        self.commit(Tx { users: vec![user.clone()], events: vec![ev], ..Default::default() })
            .await?;
        Ok(user)
    }

    /// Apply a partial update to a User, emit `UserUpdated` listing the changed fields.
    pub async fn update_user(
        &self,
        tenant: Uuid,
        id: Uuid,
        patch: UserPatch,
    ) -> Result<User, ProvisioningError> {
        let mut user = self.store.get_user(tenant, id).await?.ok_or(ProvisioningError::NotFound)?;
        let mut changed = Vec::new();
        if let Some(name) = patch.display_name {
            if name.trim().is_empty() {
                return Err(ProvisioningError::Invalid("display_name cannot be empty".into()));
            }
            user.display_name = name;
            changed.push("display_name".to_string());
        }
        if let Some(email) = patch.email {
            user.email = Some(email);
            changed.push("email".to_string());
        }
        if let Some(caps) = patch.capabilities {
            user.capabilities = caps;
            changed.push("capabilities".to_string());
        }
        if changed.is_empty() {
            return Ok(user); // no-op update; nothing to version or emit
        }
        user.base.touch();
        let ev = Self::event(
            tenant,
            UserUpdated { user_id: user.base.id, changed },
            format!("{}:UserUpdated:{}", user.base.id, user.base.version),
        );
        self.commit(Tx { users: vec![user.clone()], events: vec![ev], ..Default::default() })
            .await?;
        Ok(user)
    }

    /// Move a User to `ACTIVE` and emit `UserActivated`.
    pub async fn activate_user(&self, tenant: Uuid, id: Uuid) -> Result<User, ProvisioningError> {
        let mut user = self.store.get_user(tenant, id).await?.ok_or(ProvisioningError::NotFound)?;
        if user.state == UserState::Active {
            return Ok(user);
        }
        user.state = UserState::Active;
        user.base.touch();
        let ev = Self::event(
            tenant,
            UserActivated { user_id: user.base.id },
            format!("{}:UserActivated:{}", user.base.id, user.base.version),
        );
        self.commit(Tx { users: vec![user.clone()], events: vec![ev], ..Default::default() })
            .await?;
        Ok(user)
    }

    /// Move a User to `DEACTIVATED` (the soft-delete: deletion is a state transition,
    /// CMOS-00-ENG-012) and emit `UserDeactivated`.
    pub async fn deactivate_user(&self, tenant: Uuid, id: Uuid) -> Result<User, ProvisioningError> {
        let mut user = self.store.get_user(tenant, id).await?.ok_or(ProvisioningError::NotFound)?;
        if user.state == UserState::Deactivated {
            return Ok(user);
        }
        user.state = UserState::Deactivated;
        user.base.touch();
        let ev = Self::event(
            tenant,
            UserDeactivated { user_id: user.base.id },
            format!("{}:UserDeactivated:{}", user.base.id, user.base.version),
        );
        self.commit(Tx { users: vec![user.clone()], events: vec![ev], ..Default::default() })
            .await?;
        Ok(user)
    }

    /// Move a User to `SUSPENDED` with an optional reason and emit `UserSuspended`.
    pub async fn suspend_user(
        &self,
        tenant: Uuid,
        id: Uuid,
        reason: Option<String>,
    ) -> Result<User, ProvisioningError> {
        let mut user = self.store.get_user(tenant, id).await?.ok_or(ProvisioningError::NotFound)?;
        if user.state == UserState::Deactivated {
            return Err(ProvisioningError::IllegalState(
                "a deactivated user cannot be suspended".into(),
            ));
        }
        user.state = UserState::Suspended;
        user.base.touch();
        let ev = Self::event(
            tenant,
            UserSuspended { user_id: user.base.id, reason },
            format!("{}:UserSuspended:{}", user.base.id, user.base.version),
        );
        self.commit(Tx { users: vec![user.clone()], events: vec![ev], ..Default::default() })
            .await?;
        Ok(user)
    }

    // --- Devices -------------------------------------------------------------------------

    /// Register a Device and emit `DeviceDetected` (the platform now knows this endpoint).
    pub async fn create_device(
        &self,
        tenant: Uuid,
        req: NewDevice,
    ) -> Result<Device, ProvisioningError> {
        if req.vendor_key.trim().is_empty() || req.model.trim().is_empty() {
            return Err(ProvisioningError::Invalid("vendor_key and model are required".into()));
        }
        let mut device = Device::new(tenant, req.vendor_key, req.model);
        device.mac = req.mac;
        device.assigned_user_id = req.assigned_user_id;

        let ev = Self::event(
            tenant,
            DeviceDetected {
                device_id: device.base.id,
                vendor_key: device.vendor_key.clone(),
                model: device.model.clone(),
                mac: device.mac.clone(),
                state: device.state,
            },
            format!("{}:DeviceDetected", device.base.id),
        );
        self.commit(Tx { devices: vec![device.clone()], events: vec![ev], ..Default::default() })
            .await?;
        Ok(device)
    }

    /// Apply a partial update to a Device's descriptive fields (not its lifecycle state).
    pub async fn update_device(
        &self,
        tenant: Uuid,
        id: Uuid,
        patch: DevicePatch,
    ) -> Result<Device, ProvisioningError> {
        let mut device =
            self.store.get_device(tenant, id).await?.ok_or(ProvisioningError::NotFound)?;
        let mut touched = false;
        if let Some(model) = patch.model {
            device.model = model;
            touched = true;
        }
        if let Some(mac) = patch.mac {
            device.mac = Some(mac);
            touched = true;
        }
        if let Some(uid) = patch.assigned_user_id {
            device.assigned_user_id = Some(uid);
            touched = true;
        }
        if let Some(fw) = patch.firmware {
            device.firmware = Some(fw);
            touched = true;
        }
        if !touched {
            return Ok(device);
        }
        device.base.touch();
        // Descriptive edits are not a catalogued lifecycle event; persist without one.
        self.commit(Tx { devices: vec![device.clone()], ..Default::default() }).await?;
        Ok(device)
    }

    /// Approve a Device for provisioning (`→ APPROVED`), emit `DeviceApproved`.
    pub async fn approve_device(&self, tenant: Uuid, id: Uuid) -> Result<Device, ProvisioningError> {
        let mut device =
            self.store.get_device(tenant, id).await?.ok_or(ProvisioningError::NotFound)?;
        match device.state {
            DeviceState::Detected | DeviceState::Pending | DeviceState::Approved => {}
            other => {
                return Err(ProvisioningError::IllegalState(format!(
                    "cannot approve a device in state {other:?}"
                )))
            }
        }
        device.state = DeviceState::Approved;
        device.base.touch();
        let ev = Self::event(
            tenant,
            DeviceApproved { device_id: device.base.id },
            format!("{}:DeviceApproved:{}", device.base.id, device.base.version),
        );
        self.commit(Tx { devices: vec![device.clone()], events: vec![ev], ..Default::default() })
            .await?;
        Ok(device)
    }

    /// Reject a Device (`→ REJECTED`) with an optional reason, emit `DeviceRejected`.
    pub async fn reject_device(
        &self,
        tenant: Uuid,
        id: Uuid,
        reason: Option<String>,
    ) -> Result<Device, ProvisioningError> {
        let mut device =
            self.store.get_device(tenant, id).await?.ok_or(ProvisioningError::NotFound)?;
        if device.state == DeviceState::Retired {
            return Err(ProvisioningError::IllegalState("a retired device cannot be rejected".into()));
        }
        device.state = DeviceState::Rejected;
        device.base.touch();
        let ev = Self::event(
            tenant,
            DeviceRejected { device_id: device.base.id, reason },
            format!("{}:DeviceRejected:{}", device.base.id, device.base.version),
        );
        self.commit(Tx { devices: vec![device.clone()], events: vec![ev], ..Default::default() })
            .await?;
        Ok(device)
    }

    /// Retire a Device (`→ RETIRED`, the soft-delete), emit `DeviceRetired`.
    pub async fn retire_device(&self, tenant: Uuid, id: Uuid) -> Result<Device, ProvisioningError> {
        let mut device =
            self.store.get_device(tenant, id).await?.ok_or(ProvisioningError::NotFound)?;
        if device.state == DeviceState::Retired {
            return Ok(device);
        }
        device.state = DeviceState::Retired;
        device.base.touch();
        let ev = Self::event(
            tenant,
            DeviceRetired { device_id: device.base.id },
            format!("{}:DeviceRetired:{}", device.base.id, device.base.version),
        );
        self.commit(Tx { devices: vec![device.clone()], events: vec![ev], ..Default::default() })
            .await?;
        Ok(device)
    }

    /// Begin replacing a Device: move the old one to `REPLACING`, mint a fresh Device
    /// (same vendor/model, in `APPROVED`, no MAC yet) as its replacement, and emit
    /// `DeviceReplacementStarted` — the one-click cross-vendor swap (Volume 8 §replacement).
    /// Both devices commit atomically.
    pub async fn replace_device(
        &self,
        tenant: Uuid,
        id: Uuid,
    ) -> Result<(Device, Device), ProvisioningError> {
        let mut old = self.store.get_device(tenant, id).await?.ok_or(ProvisioningError::NotFound)?;
        match old.state {
            DeviceState::Retired | DeviceState::Rejected => {
                return Err(ProvisioningError::IllegalState(format!(
                    "cannot replace a device in state {:?}",
                    old.state
                )))
            }
            _ => {}
        }
        // The replacement inherits the assignment so the person keeps their extension.
        let mut replacement = Device::new(tenant, old.vendor_key.clone(), old.model.clone());
        replacement.assigned_user_id = old.assigned_user_id;

        old.state = DeviceState::Replacing;
        old.base.touch();

        let ev = Self::event(
            tenant,
            DeviceReplacementStarted {
                device_id: old.base.id,
                replacement_device_id: replacement.base.id,
            },
            format!("{}:DeviceReplacementStarted:{}", old.base.id, old.base.version),
        );
        self.commit(Tx {
            devices: vec![old.clone(), replacement.clone()],
            events: vec![ev],
            ..Default::default()
        })
        .await?;
        Ok((old, replacement))
    }

    // --- Extensions & Routes (pure config: no lifecycle event) ---------------------------

    /// Create an Extension and the Route it points at, committed atomically.
    pub async fn create_extension(
        &self,
        tenant: Uuid,
        req: NewExtension,
    ) -> Result<Extension, ProvisioningError> {
        if req.number.trim().is_empty() {
            return Err(ProvisioningError::Invalid("number is required".into()));
        }
        if req.destination_ref.trim().is_empty() {
            return Err(ProvisioningError::Invalid("destination_ref is required".into()));
        }
        let route = Route::new(tenant, req.destination_ref);
        let mut ext = Extension::new(tenant, req.number, route.base.id);
        ext.label = req.label;
        self.commit(Tx {
            routes: vec![route],
            extensions: vec![ext.clone()],
            ..Default::default()
        })
        .await?;
        Ok(ext)
    }

    /// Apply a partial update to an Extension. A new `destination_ref` re-points its Route.
    pub async fn update_extension(
        &self,
        tenant: Uuid,
        id: Uuid,
        patch: ExtensionPatch,
    ) -> Result<Extension, ProvisioningError> {
        let mut ext =
            self.store.get_extension(tenant, id).await?.ok_or(ProvisioningError::NotFound)?;
        let mut tx = Tx::default();
        let mut touched = false;
        if let Some(number) = patch.number {
            if number.trim().is_empty() {
                return Err(ProvisioningError::Invalid("number cannot be empty".into()));
            }
            ext.number = number;
            touched = true;
        }
        if let Some(label) = patch.label {
            ext.label = Some(label);
            touched = true;
        }
        if let Some(dest) = patch.destination_ref {
            let mut route = self
                .store
                .get_route(tenant, ext.route_id)
                .await?
                .ok_or_else(|| ProvisioningError::Invalid("extension's route is missing".into()))?;
            route.destination_ref = dest;
            route.base.touch();
            tx.routes.push(route);
            touched = true;
        }
        if !touched {
            return Ok(ext);
        }
        ext.base.touch();
        tx.extensions.push(ext.clone());
        self.commit(tx).await?;
        Ok(ext)
    }

    /// Delete an Extension (config has no history, so this is a hard delete).
    pub async fn delete_extension(&self, tenant: Uuid, id: Uuid) -> Result<(), ProvisioningError> {
        if self.store.delete_extension(tenant, id).await? {
            Ok(())
        } else {
            Err(ProvisioningError::NotFound)
        }
    }

    /// Create a standalone Route (a destination not yet bound to an extension).
    pub async fn create_route(
        &self,
        tenant: Uuid,
        destination_ref: String,
        priority: Option<i64>,
    ) -> Result<Route, ProvisioningError> {
        if destination_ref.trim().is_empty() {
            return Err(ProvisioningError::Invalid("destination_ref is required".into()));
        }
        let mut route = Route::new(tenant, destination_ref);
        route.priority = priority;
        self.commit(Tx { routes: vec![route.clone()], ..Default::default() }).await?;
        Ok(route)
    }

    /// Re-point / re-prioritise a Route.
    pub async fn update_route(
        &self,
        tenant: Uuid,
        id: Uuid,
        destination_ref: Option<String>,
        priority: Option<i64>,
    ) -> Result<Route, ProvisioningError> {
        let mut route =
            self.store.get_route(tenant, id).await?.ok_or(ProvisioningError::NotFound)?;
        let mut touched = false;
        if let Some(dest) = destination_ref {
            if dest.trim().is_empty() {
                return Err(ProvisioningError::Invalid("destination_ref cannot be empty".into()));
            }
            route.destination_ref = dest;
            touched = true;
        }
        if let Some(p) = priority {
            route.priority = Some(p);
            touched = true;
        }
        if !touched {
            return Ok(route);
        }
        route.base.touch();
        self.commit(Tx { routes: vec![route.clone()], ..Default::default() }).await?;
        Ok(route)
    }

    /// Delete a Route (config hard delete).
    pub async fn delete_route(&self, tenant: Uuid, id: Uuid) -> Result<(), ProvisioningError> {
        if self.store.delete_route(tenant, id).await? {
            Ok(())
        } else {
            Err(ProvisioningError::NotFound)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{MemStore, Store};

    fn svc() -> (Provisioning, Arc<dyn Store>) {
        let store: Arc<dyn Store> = Arc::new(MemStore::new());
        let p = Provisioning::new(store.clone(), RelaySignal::new());
        (p, store)
    }

    #[tokio::test]
    async fn user_create_update_lifecycle_emits_events() {
        let (p, store) = svc();
        let t = Uuid::now_v7();
        let u = p
            .create_user(t, NewUser { display_name: "Ada".into(), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(u.state, UserState::Active);
        assert_eq!(u.base.version, 0);

        let u = p
            .update_user(
                t,
                u.base.id,
                UserPatch { email: Some("ada@x.io".into()), ..Default::default() },
            )
            .await
            .unwrap();
        assert_eq!(u.base.version, 1);
        assert_eq!(u.email.as_deref(), Some("ada@x.io"));

        let u = p.suspend_user(t, u.base.id, Some("policy".into())).await.unwrap();
        assert_eq!(u.state, UserState::Suspended);
        let u = p.deactivate_user(t, u.base.id).await.unwrap();
        assert_eq!(u.state, UserState::Deactivated);
        // A deactivated user cannot be suspended.
        assert!(matches!(
            p.suspend_user(t, u.base.id, None).await,
            Err(ProvisioningError::IllegalState(_))
        ));

        // Events: Created, Updated, Suspended, Deactivated = 4.
        assert_eq!(store.peek_outbox(100).await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn device_inbox_approve_reject_retire() {
        let (p, store) = svc();
        let t = Uuid::now_v7();
        let d = p
            .create_device(t, NewDevice { vendor_key: "yealink".into(), model: "T46".into(), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(d.state, DeviceState::Approved); // Device::new starts APPROVED
        let d = p.approve_device(t, d.base.id).await.unwrap();
        assert_eq!(d.state, DeviceState::Approved);
        let (old, replacement) = p.replace_device(t, d.base.id).await.unwrap();
        assert_eq!(old.state, DeviceState::Replacing);
        assert_eq!(replacement.state, DeviceState::Approved);
        assert_eq!(replacement.assigned_user_id, old.assigned_user_id);
        let r = p.retire_device(t, replacement.base.id).await.unwrap();
        assert_eq!(r.state, DeviceState::Retired);
        assert!(matches!(
            p.reject_device(t, r.base.id, None).await,
            Err(ProvisioningError::IllegalState(_))
        ));
        // Events: Detected, Approved, ReplacementStarted, Retired = 4.
        assert_eq!(store.peek_outbox(100).await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn extension_and_route_crud() {
        let (p, store) = svc();
        let t = Uuid::now_v7();
        let ext = p
            .create_extension(
                t,
                NewExtension { number: "100".into(), destination_ref: "sip:100@x".into(), ..Default::default() },
            )
            .await
            .unwrap();
        // The route it points at exists.
        let route = store.get_route(t, ext.route_id).await.unwrap().unwrap();
        assert_eq!(route.destination_ref, "sip:100@x");

        // Re-point via the extension patch.
        let ext = p
            .update_extension(
                t,
                ext.base.id,
                ExtensionPatch { destination_ref: Some("queue:abc".into()), ..Default::default() },
            )
            .await
            .unwrap();
        let route = store.get_route(t, ext.route_id).await.unwrap().unwrap();
        assert_eq!(route.destination_ref, "queue:abc");

        // Delete removes it; a second delete is NotFound.
        p.delete_extension(t, ext.base.id).await.unwrap();
        assert!(store.get_extension(t, ext.base.id).await.unwrap().is_none());
        assert!(matches!(
            p.delete_extension(t, ext.base.id).await,
            Err(ProvisioningError::NotFound)
        ));

        // Config writes emit no events.
        assert_eq!(store.peek_outbox(100).await.unwrap().len(), 0);
    }
}
