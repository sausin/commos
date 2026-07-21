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
    /// Update a gateway's health (the one mutable field an operator sets directly). Versions the
    /// entity forward.
    pub async fn set_gateway_health(&self, tenant: Uuid, id: Uuid, health: GatewayHealth) -> Result<Gateway, TrunkingError> {
        let mut g = self.get_gateway(tenant, id).await?;
        g.health = health;
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
        let off = s.set_gateway_health(t, gw.base.id, GatewayHealth::Offline).await.unwrap();
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
}
