//! SIP-style device registrations — **deliberately ephemeral, in-memory only** state
//! (CMOS-14-DEP-021).
//!
//! Registrations are the classic "who is reachable at which contact right now" binding a
//! SIP `REGISTER` establishes. They are high-churn, short-lived, and reconstructable from
//! the network on the next re-register, so they are intentionally kept OUT of the durable
//! system of record. Per the spec, "registrations live in the Redis/NATS-class / in-process
//! layer, not the system of record."
//!
//! The reference implementation collapses that class into this in-process registry: a plain
//! `Arc<Mutex<HashMap<..>>>`. This keeps write volume near zero — a device re-registering
//! every 60s must never translate into a durable disk write — so an SD-card Raspberry Pi
//! deployment survives for years instead of burning out its flash. There are, by design,
//! **NO database writes and NO events** on this path.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use commos_core::common::{Timestamp, Uuid};

/// A live device registration: the binding between an address-of-record and the contact
/// URI where that AOR is currently reachable. Ephemeral — never persisted.
#[derive(Clone, Debug, Serialize)]
pub struct Registration {
    pub id: Uuid,
    pub tenant_id: Uuid,
    /// Address-of-record, e.g. `sip:100@example.com`.
    pub aor: String,
    /// Where the AOR is reachable right now, e.g. `sip:100@192.168.1.5:5060`.
    pub contact: String,
    pub user_agent: Option<String>,
    pub expires_at: Timestamp,
    pub created_at: Timestamp,
}

/// In-memory, tenant-scoped registration store. Cheap to clone (`Arc` handle); the whole
/// map is guarded by a single `Mutex` since the working set (devices per hub) is small.
///
/// Keyed by `(tenant_id, aor)` so a fresh REGISTER for an already-registered AOR is an
/// upsert (refresh) rather than a duplicate — matching SIP semantics.
#[derive(Clone)]
pub struct RegistrationRegistry {
    inner: Arc<Mutex<HashMap<(Uuid, String), Registration>>>,
}

impl RegistrationRegistry {
    pub fn new() -> Self {
        RegistrationRegistry {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Upsert a registration by `(tenant, aor)`.
    ///
    /// If the AOR is already registered, refresh its `contact`, `user_agent`, and
    /// `expires_at` while preserving the original `id` and `created_at` (a re-register is
    /// the same binding renewed). Otherwise mint a new registration.
    ///
    /// `expires_at` is computed as `now + expires_secs`. NO durable writes, NO events.
    pub fn register(
        &self,
        tenant: Uuid,
        aor: String,
        contact: String,
        user_agent: Option<String>,
        expires_secs: u64,
    ) -> Registration {
        let now = Timestamp::now();
        // now + expires_secs, staying on the contract's Timestamp type.
        let expires_at = Timestamp::from_offset(
            now.into_offset() + time::Duration::seconds(expires_secs as i64),
        );

        let mut map = self.inner.lock().expect("registration mutex not poisoned");
        let key = (tenant, aor.clone());
        let reg = match map.get(&key) {
            Some(existing) => Registration {
                // Refresh: keep identity + birth time, renew the binding.
                id: existing.id,
                tenant_id: tenant,
                aor,
                contact,
                user_agent,
                expires_at,
                created_at: existing.created_at,
            },
            None => Registration {
                id: Uuid::now_v7(),
                tenant_id: tenant,
                aor,
                contact,
                user_agent,
                expires_at,
                created_at: now,
            },
        };
        map.insert(key, reg.clone());
        reg
    }

    /// All registrations for a tenant (tenant-scoped; other tenants are invisible).
    pub fn list(&self, tenant: Uuid) -> Vec<Registration> {
        let map = self.inner.lock().expect("registration mutex not poisoned");
        map.values()
            .filter(|r| r.tenant_id == tenant)
            .cloned()
            .collect()
    }

    /// Fetch a single registration by id, scoped to the tenant.
    pub fn get(&self, tenant: Uuid, id: Uuid) -> Option<Registration> {
        let map = self.inner.lock().expect("registration mutex not poisoned");
        map.values()
            .find(|r| r.tenant_id == tenant && r.id == id)
            .cloned()
    }

    /// Remove a registration by id within the tenant. Returns whether one was found.
    pub fn unregister(&self, tenant: Uuid, id: Uuid) -> bool {
        let mut map = self.inner.lock().expect("registration mutex not poisoned");
        let key = map
            .iter()
            .find(|(_, r)| r.tenant_id == tenant && r.id == id)
            .map(|(k, _)| k.clone());
        match key {
            Some(k) => map.remove(&k).is_some(),
            None => false,
        }
    }
}

impl Default for RegistrationRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tenant() -> Uuid {
        Uuid::now_v7()
    }

    #[test]
    fn register_then_list() {
        let reg = RegistrationRegistry::new();
        let t = tenant();
        let r = reg.register(
            t,
            "sip:100@example.com".to_string(),
            "sip:100@192.168.1.5:5060".to_string(),
            Some("Acme/1.0".to_string()),
            3600,
        );
        let items = reg.list(t);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, r.id);
        assert_eq!(items[0].aor, "sip:100@example.com");
        assert_eq!(items[0].contact, "sip:100@192.168.1.5:5060");
    }

    #[test]
    fn refresh_keeps_id_and_created_at() {
        let reg = RegistrationRegistry::new();
        let t = tenant();
        let first = reg.register(
            t,
            "sip:100@example.com".to_string(),
            "sip:100@192.168.1.5:5060".to_string(),
            None,
            3600,
        );
        // Re-register the same AOR from a new contact.
        let second = reg.register(
            t,
            "sip:100@example.com".to_string(),
            "sip:100@10.0.0.9:5060".to_string(),
            Some("Acme/2.0".to_string()),
            120,
        );
        assert_eq!(first.id, second.id, "refresh keeps the same id");
        assert_eq!(first.created_at, second.created_at, "created_at is preserved");
        assert_eq!(second.contact, "sip:100@10.0.0.9:5060", "contact is refreshed");
        // Still a single binding for the AOR, not a duplicate.
        assert_eq!(reg.list(t).len(), 1);
    }

    #[test]
    fn unregister_removes_and_reports() {
        let reg = RegistrationRegistry::new();
        let t = tenant();
        let r = reg.register(
            t,
            "sip:100@example.com".to_string(),
            "sip:100@192.168.1.5:5060".to_string(),
            None,
            3600,
        );
        assert!(reg.get(t, r.id).is_some());
        assert!(reg.unregister(t, r.id), "found and removed");
        assert!(reg.get(t, r.id).is_none());
        assert!(!reg.unregister(t, r.id), "second remove finds nothing");
        assert!(reg.list(t).is_empty());
    }

    #[test]
    fn registrations_are_tenant_scoped() {
        let reg = RegistrationRegistry::new();
        let a = tenant();
        let b = tenant();
        let ra = reg.register(
            a,
            "sip:100@example.com".to_string(),
            "sip:100@192.168.1.5:5060".to_string(),
            None,
            3600,
        );
        assert!(reg.list(b).is_empty(), "other tenant sees nothing");
        assert!(reg.get(b, ra.id).is_none(), "cannot read across tenants");
        assert!(!reg.unregister(b, ra.id), "cannot delete across tenants");
    }
}
