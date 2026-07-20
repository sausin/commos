//! RFC 9457 Problem Details (Volume 4 §5; OpenAPI `components.schemas.Problem`).
//!
//! Every non-2xx API response is a Problem, with `content-type: application/problem+json`.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Problem {
    /// A URI reference identifying the problem type.
    #[serde(rename = "type")]
    pub type_uri: String,
    pub title: String,
    pub status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Machine-readable CommOS error code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
}

impl Problem {
    pub fn new(status: StatusCode, code: &str, detail: impl Into<String>) -> Self {
        Problem {
            type_uri: format!("https://commos.dev/problems/{code}"),
            title: status.canonical_reason().unwrap_or("Error").to_string(),
            status: status.as_u16(),
            detail: Some(detail.into()),
            code: Some(code.to_string()),
            correlation_id: None,
        }
    }

    pub fn unauthorized(detail: impl Into<String>) -> Self {
        Problem::new(StatusCode::UNAUTHORIZED, "unauthenticated", detail)
    }
    pub fn not_found(detail: impl Into<String>) -> Self {
        Problem::new(StatusCode::NOT_FOUND, "not_found", detail)
    }
    pub fn bad_request(detail: impl Into<String>) -> Self {
        Problem::new(StatusCode::BAD_REQUEST, "bad_request", detail)
    }
    pub fn internal(detail: impl Into<String>) -> Self {
        Problem::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", detail)
    }
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut resp = (status, Json(&self)).into_response();
        resp.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/problem+json"),
        );
        resp
    }
}
