//! Authentication → tenant context (Volume 9; CMOS-04-API bearer auth).
//!
//! The frozen API secures every `/v1` route with `bearerAuth` (JWT). Full JWT
//! verification against the Identity subsystem's keys is Volume 9 work; this first slice
//! accepts a **development** bearer of the form `tenant:<uuidv7>` so tenant isolation
//! (CMOS-03-ARCH-050) is real and testable now. The extractor is the single choke point,
//! so swapping in real JWT validation later touches only this file.

use axum::async_trait;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use commos_core::common::Uuid;

use super::problem::Problem;

/// The authenticated caller's tenant. Every data access is scoped by this
/// (defence in depth, CMOS-03-ARCH-050).
#[derive(Clone, Copy, Debug)]
pub struct TenantContext {
    pub tenant_id: Uuid,
}

#[async_trait]
impl<S: Send + Sync> FromRequestParts<S> for TenantContext {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| Problem::unauthorized("missing Authorization header"))?;

        let token = header
            .strip_prefix("Bearer ")
            .ok_or_else(|| Problem::unauthorized("expected a Bearer token"))?
            .trim();

        // Development token shape: `tenant:<uuidv7>`.
        let raw = token
            .strip_prefix("tenant:")
            .ok_or_else(|| Problem::unauthorized("unrecognised token"))?;
        let tenant_id = Uuid::parse(raw)
            .map_err(|_| Problem::unauthorized("token tenant is not a valid UUIDv7"))?;

        Ok(TenantContext { tenant_id })
    }
}
