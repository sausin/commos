//! PSTN / SIP trunking (control plane) — config CRUD for carriers, gateways, trunks, and DIDs,
//! plus the routing lookups the SIP plane uses to place outbound calls and route inbound DIDs.
//!
//! These are *configuration* entities (peers of Queue/Extension, CMOS-02-DOM-100): create/
//! update/delete persist the entity with no event (no `*Created` exists in the frozen catalogue;
//! `GatewayOffline`/`GatewayRecovered` are *observed* health transitions, future work). One
//! service owns all four so the SIP plane has a single handle for the trunk/DID lookups.

use std::sync::Arc;

use commos_core::common::Uuid;
use commos_core::entities::carrier::{Carrier, CarrierKind};
use commos_core::entities::did::Did;
use commos_core::entities::gateway::{Gateway, GatewayHealth, GatewayKind};
use commos_core::entities::trunk::Trunk;

use crate::relay::RelaySignal;

/// Optional per-gateway dialling profile (patchable subset of [`Gateway`]).
#[derive(Default, Clone, Debug, serde::Deserialize)]
pub struct GatewayDialing {
    pub country_code: Option<String>,
    pub national_prefix: Option<String>,
    pub international_prefix: Option<String>,
    pub strip_prefix: Option<String>,
    pub outbound_caller_id: Option<String>,
}
impl GatewayDialing {
    pub fn is_empty(&self) -> bool {
        self.country_code.is_none()
            && self.national_prefix.is_none()
            && self.international_prefix.is_none()
            && self.strip_prefix.is_none()
            && self.outbound_caller_id.is_none()
    }
}
use crate::store::{Page, Store, StoreError, Tx};

#[derive(Debug, thiserror::Error)]
pub enum TrunkingError {
    #[error("entity not found")]
    NotFound,
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The trunking service. Stateless between requests — all state lives in the [`Store`].
#[derive(Clone)]
pub struct TrunkingService {
    store: Arc<dyn Store>,
    signal: RelaySignal,
}

impl TrunkingService {
    pub fn new(store: Arc<dyn Store>, signal: RelaySignal) -> Self {
        TrunkingService { store, signal }
    }

    // ---- Carriers ---------------------------------------------------------------------------

    pub async fn create_carrier(
        &self,
        tenant: Uuid,
        name: String,
        kind: CarrierKind,
        rating_profile_id: Option<Uuid>,
    ) -> Result<Carrier, TrunkingError> {
        let mut c = Carrier::new(tenant, name, kind);
        c.rating_profile_id = rating_profile_id;
        self.commit_carrier(c.clone()).await?;
        Ok(c)
    }
    pub async fn get_carrier(&self, tenant: Uuid, id: Uuid) -> Result<Carrier, TrunkingError> {
        self.store.get_carrier(tenant, id).await?.ok_or(TrunkingError::NotFound)
    }
    pub async fn list_carriers(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Carrier>, TrunkingError> {
        Ok(self.store.list_carriers(tenant, limit, cursor).await?)
    }
    pub async fn delete_carrier(&self, tenant: Uuid, id: Uuid) -> Result<(), TrunkingError> {
        self.store.delete_carrier(tenant, id).await?.then_some(()).ok_or(TrunkingError::NotFound)
    }
    async fn commit_carrier(&self, c: Carrier) -> Result<(), TrunkingError> {
        self.store.commit(Tx { carriers: vec![c], ..Default::default() }).await?;
        self.signal.wake();
        Ok(())
    }

    // ---- Gateways ---------------------------------------------------------------------------

    pub async fn create_gateway(
        &self,
        tenant: Uuid,
        carrier_id: Uuid,
        kind: GatewayKind,
        address: Option<String>,
        health: GatewayHealth,
    ) -> Result<Gateway, TrunkingError> {
        let g = Gateway {
            base: commos_core::common::EntityBase::new(tenant),
            carrier_id,
            kind,
            address,
            health,
            country_code: None,
            national_prefix: None,
            international_prefix: None,
            strip_prefix: None,
            outbound_caller_id: None,
        };
        self.store.commit(Tx { gateways: vec![g.clone()], ..Default::default() }).await?;
        self.signal.wake();
        Ok(g)
    }
    pub async fn get_gateway(&self, tenant: Uuid, id: Uuid) -> Result<Gateway, TrunkingError> {
        self.store.get_gateway(tenant, id).await?.ok_or(TrunkingError::NotFound)
    }
    pub async fn list_gateways(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Gateway>, TrunkingError> {
        Ok(self.store.list_gateways(tenant, limit, cursor).await?)
    }
    /// Patch a gateway's health and/or its dialling profile (country code, prefixes, caller ID).
    pub async fn update_gateway(
        &self,
        tenant: Uuid,
        id: Uuid,
        health: Option<GatewayHealth>,
        d: GatewayDialing,
    ) -> Result<Gateway, TrunkingError> {
        let mut g = self.get_gateway(tenant, id).await?;
        if let Some(h) = health {
            g.health = h;
        }
        // An empty string clears an override.
        let set = |slot: &mut Option<String>, v: Option<String>| {
            if let Some(v) = v {
                *slot = (!v.trim().is_empty()).then(|| v.trim().to_string());
            }
        };
        set(&mut g.country_code, d.country_code);
        set(&mut g.national_prefix, d.national_prefix);
        set(&mut g.international_prefix, d.international_prefix);
        set(&mut g.strip_prefix, d.strip_prefix);
        set(&mut g.outbound_caller_id, d.outbound_caller_id);
        g.base.touch();
        self.store.commit(Tx { gateways: vec![g.clone()], ..Default::default() }).await?;
        self.signal.wake();
        Ok(g)
    }
    pub async fn delete_gateway(&self, tenant: Uuid, id: Uuid) -> Result<(), TrunkingError> {
        self.store.delete_gateway(tenant, id).await?.then_some(()).ok_or(TrunkingError::NotFound)
    }

    // ---- Trunks -----------------------------------------------------------------------------

    pub async fn create_trunk(
        &self,
        tenant: Uuid,
        carrier_id: Uuid,
        channels_max: Option<i64>,
        codecs: Vec<String>,
        auth: Option<serde_json::Value>,
    ) -> Result<Trunk, TrunkingError> {
        let mut t = Trunk::new(tenant, carrier_id);
        t.channels_max = channels_max;
        t.codecs = codecs;
        t.auth = auth;
        self.store.commit(Tx { trunks: vec![t.clone()], ..Default::default() }).await?;
        self.signal.wake();
        Ok(t)
    }
    pub async fn get_trunk(&self, tenant: Uuid, id: Uuid) -> Result<Trunk, TrunkingError> {
        self.store.get_trunk(tenant, id).await?.ok_or(TrunkingError::NotFound)
    }
    pub async fn list_trunks(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Trunk>, TrunkingError> {
        Ok(self.store.list_trunks(tenant, limit, cursor).await?)
    }
    pub async fn delete_trunk(&self, tenant: Uuid, id: Uuid) -> Result<(), TrunkingError> {
        self.store.delete_trunk(tenant, id).await?.then_some(()).ok_or(TrunkingError::NotFound)
    }

    // ---- DIDs -------------------------------------------------------------------------------

    pub async fn create_did(
        &self,
        tenant: Uuid,
        e164: String,
        carrier_id: Uuid,
        destination_ref: String,
    ) -> Result<Did, TrunkingError> {
        let d = Did::new(tenant, e164, carrier_id, destination_ref);
        self.store.commit(Tx { dids: vec![d.clone()], ..Default::default() }).await?;
        self.signal.wake();
        Ok(d)
    }
    /// Partial DID update. Outer `None` = leave unchanged; `Some(None)` clears a nullable field.
    pub async fn update_did(
        &self,
        tenant: Uuid,
        id: Uuid,
        destination_ref: Option<String>,
        schedule_id: Option<Option<Uuid>>,
        closed_destination_ref: Option<Option<String>>,
    ) -> Result<Did, TrunkingError> {
        let mut d = self.get_did(tenant, id).await?;
        if let Some(v) = destination_ref {
            d.destination_ref = v;
        }
        if let Some(v) = schedule_id {
            d.schedule_id = v;
        }
        if let Some(v) = closed_destination_ref {
            d.closed_destination_ref = v;
        }
        d.base.touch();
        self.store.commit(Tx { dids: vec![d.clone()], ..Default::default() }).await?;
        self.signal.wake();
        Ok(d)
    }
    pub async fn get_did(&self, tenant: Uuid, id: Uuid) -> Result<Did, TrunkingError> {
        self.store.get_did(tenant, id).await?.ok_or(TrunkingError::NotFound)
    }
    pub async fn list_dids(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Did>, TrunkingError> {
        Ok(self.store.list_dids(tenant, limit, cursor).await?)
    }
    pub async fn delete_did(&self, tenant: Uuid, id: Uuid) -> Result<(), TrunkingError> {
        self.store.delete_did(tenant, id).await?.then_some(()).ok_or(TrunkingError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemStore;

    fn svc() -> TrunkingService {
        TrunkingService::new(Arc::new(MemStore::new()), RelaySignal::new())
    }

    #[tokio::test]
    async fn carrier_gateway_trunk_did_crud() {
        let s = svc();
        let t = Uuid::now_v7();
        let carrier = s.create_carrier(t, "Acme".into(), CarrierKind::SipTrunk, None).await.unwrap();
        let gw = s
            .create_gateway(t, carrier.base.id, GatewayKind::Sip, Some("gw.acme:5060".into()), GatewayHealth::Online)
            .await
            .unwrap();
        s.create_trunk(t, carrier.base.id, Some(30), vec!["PCMU".into()], None).await.unwrap();
        let did = s.create_did(t, "+14155550100".into(), carrier.base.id, "sip:200@host".into()).await.unwrap();

        assert_eq!(s.list_carriers(t, 50, None).await.unwrap().items.len(), 1);
        assert_eq!(s.list_gateways(t, 50, None).await.unwrap().items.len(), 1);
        assert_eq!(s.list_trunks(t, 50, None).await.unwrap().items.len(), 1);
        assert_eq!(s.get_did(t, did.base.id).await.unwrap().destination_ref, "sip:200@host");

        // Health toggle versions the gateway forward.
        let off = s.update_gateway(t, gw.base.id, Some(GatewayHealth::Offline), GatewayDialing::default()).await.unwrap();
        assert_eq!(off.health, GatewayHealth::Offline);
        assert_eq!(off.base.version, 1);

        s.delete_did(t, did.base.id).await.unwrap();
        assert!(matches!(s.get_did(t, did.base.id).await, Err(TrunkingError::NotFound)));
    }

    #[tokio::test]
    async fn reads_are_tenant_scoped() {
        let s = svc();
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let c = s.create_carrier(a, "A".into(), CarrierKind::Pstn, None).await.unwrap();
        assert!(matches!(s.get_carrier(b, c.base.id).await, Err(TrunkingError::NotFound)));
    }

    #[tokio::test]
    async fn did_patch_sets_and_clears_schedule_fields() {
        let s = svc();
        let t = Uuid::now_v7();
        let did = s.create_did(t, "+14155550101".into(), Uuid::now_v7(), "sip:200".into()).await.unwrap();
        assert!(did.schedule_id.is_none());
        let sid = Uuid::now_v7();
        let d = s
            .update_did(t, did.base.id, None, Some(Some(sid)), Some(Some("voicemail".into())))
            .await
            .unwrap();
        assert_eq!(d.destination_ref, "sip:200");
        assert_eq!(d.schedule_id, Some(sid));
        assert_eq!(d.closed_destination_ref.as_deref(), Some("voicemail"));
        assert_eq!(d.base.version, 1);
        // Absent fields are untouched; destination changes.
        let d = s.update_did(t, did.base.id, Some("sip:300".into()), None, None).await.unwrap();
        assert_eq!(d.destination_ref, "sip:300");
        assert_eq!(d.schedule_id, Some(sid));
        // Explicit null clears.
        let d = s.update_did(t, did.base.id, None, Some(None), Some(None)).await.unwrap();
        assert!(d.schedule_id.is_none() && d.closed_destination_ref.is_none());
        assert!(matches!(s.update_did(t, Uuid::now_v7(), None, None, None).await, Err(TrunkingError::NotFound)));
    }
}
