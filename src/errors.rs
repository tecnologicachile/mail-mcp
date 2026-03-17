//! Application error model with MCP error mapping
//!
//! Defines a typed error hierarchy using `thiserror` for internal error handling,
//! and maps each variant to the appropriate MCP `ErrorData` type for protocol
//! compliance.

use rmcp::model::ErrorData;
use serde_json::json;
use thiserror::Error;

/// Application error type
///
/// Covers all error cases the IMAP MCP server may encounter. Each variant maps
/// to an appropriate MCP error code in [`ErrorData`].
#[derive(Debug, Error)]
pub enum AppError {
    /// Invalid user input (validation failed, malformed request)
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// Resource not found (account, mailbox, message)
    #[error("not found: {0}")]
    NotFound(String),
    /// Authentication failure (bad credentials, account disabled)
    #[error("authentication failed: {0}")]
    AuthFailed(String),
    /// Operation timeout (TCP connect, TLS handshake, IMAP response)
    #[error("operation timed out: {0}")]
    Timeout(String),
    /// Conflict (mailbox UIDVALIDITY changed, state inconsistent)
    #[error("conflict: {0}")]
    Conflict(String),
    /// OAuth2 token refresh failure
    #[error("token refresh failed: {0}")]
    TokenRefresh(String),
    /// Internal error (unexpected failure, external crate error)
    #[error("internal error: {0}")]
    Internal(String),
}

impl AppError {
    /// Convenience constructor for `InvalidInput`
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::InvalidInput(msg.into())
    }

    /// Convert to MCP `ErrorData`
    ///
    /// Maps each `AppError` variant to the appropriate MCP error type and
    /// includes a structured `code` field for client error handling.
    ///
    /// # Mappings
    ///
    /// - `InvalidInput` → `invalid_params`
    /// - `NotFound` → `resource_not_found`
    /// - `AuthFailed` → `invalid_request`
    /// - `Timeout` → `internal_error`
    /// - `Conflict` → `invalid_request`
    /// - `TokenRefresh` → `invalid_request`
    /// - `Internal` → `internal_error`
    pub fn to_error_data(&self) -> ErrorData {
        match self {
            Self::InvalidInput(msg) => {
                ErrorData::invalid_params(msg.clone(), Some(json!({ "code": "invalid_input" })))
            }
            Self::NotFound(msg) => {
                ErrorData::resource_not_found(msg.clone(), Some(json!({ "code": "not_found" })))
            }
            Self::AuthFailed(msg) => {
                ErrorData::invalid_request(msg.clone(), Some(json!({ "code": "auth_failed" })))
            }
            Self::Timeout(msg) => {
                ErrorData::internal_error(msg.clone(), Some(json!({ "code": "timeout" })))
            }
            Self::Conflict(msg) => {
                ErrorData::invalid_request(msg.clone(), Some(json!({ "code": "conflict" })))
            }
            Self::TokenRefresh(msg) => ErrorData::invalid_request(
                msg.clone(),
                Some(json!({ "code": "token_refresh_failed" })),
            ),
            Self::Internal(msg) => {
                ErrorData::internal_error(msg.clone(), Some(json!({ "code": "internal" })))
            }
        }
    }
}

/// Type alias for fallible return values
///
/// Use this for all internal functions that can fail. Provides a consistent
/// error type throughout the codebase.
pub type AppResult<T> = Result<T, AppError>;
