//! Core error type.

use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CoreError {
    /// A value violated its contract constraint (schema pattern / enum / required field).
    #[error("invalid {kind}: {detail}")]
    Invalid { kind: String, detail: String },
}

impl CoreError {
    pub(crate) fn invalid(kind: &str, detail: impl Into<String>) -> Self {
        CoreError::Invalid {
            kind: kind.to_string(),
            detail: detail.into(),
        }
    }
}
