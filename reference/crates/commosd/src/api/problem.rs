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
    /// The HTTP status' canonical reason — always a static string.
    pub title: &'static str,
    pub status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Machine-readable CommOS error code — a fixed catalogue of static slugs. Keeping it
    /// (and `title`) `&'static str` rather than `String` also holds `Problem` under clippy's
    /// `result_large_err` threshold, so the API's `Result<_, Problem>` stays cheap to return.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
}

impl Problem {
    pub fn new(status: StatusCode, code: &'static str, detail: impl Into<String>) -> Self {
        Problem {
            type_uri: format!("https://commos.dev/problems/{code}"),
            title: status.canonical_reason().unwrap_or("Error"),
            status: status.as_u16(),
            detail: Some(detail.into()),
            code: Some(code),
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
