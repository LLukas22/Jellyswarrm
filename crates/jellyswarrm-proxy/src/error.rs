//! Unified error types for the Jellyswarrm proxy
//!
//! This module provides a consistent error handling approach across
//! all handlers and services.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use tracing::error;

/// Application-wide error type
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// Database operation failed
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    /// Authentication failed
    #[error("Authentication failed")]
    Unauthorized,

    /// Access denied
    #[error("Access denied")]
    Forbidden,

    /// Resource not found
    #[error("Not found: {0}")]
    NotFound(String),

    /// Invalid request
    #[error("Bad request: {0}")]
    BadRequest(String),

    /// Validation error
    #[error("Validation failed: {0}")]
    Validation(String),

    /// External service error (e.g., upstream Jellyfin server)
    #[error("Upstream service error: {0}")]
    Upstream(String),

    /// Internal server error
    #[error("Internal error: {0}")]
    Internal(String),

    /// Request preprocessing failed
    #[error("Preprocessing failed: {0}")]
    Preprocessing(String),

    /// Encryption/decryption error
    #[error("Encryption error: {0}")]
    Encryption(String),
}

/// Error response body for JSON responses
#[derive(Serialize)]
struct ErrorResponse {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<String>,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error_message, details) = match &self {
            AppError::Database(e) => {
                error!("Database error: {}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Database operation failed",
                    None,
                )
            }
            AppError::Unauthorized => (StatusCode::UNAUTHORIZED, "Authentication required", None),
            AppError::Forbidden => (StatusCode::FORBIDDEN, "Access denied", None),
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, "Resource not found", Some(msg.clone())),
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, "Invalid request", Some(msg.clone())),
            AppError::Validation(msg) => {
                (StatusCode::UNPROCESSABLE_ENTITY, "Validation failed", Some(msg.clone()))
            }
            AppError::Upstream(msg) => {
                error!("Upstream error: {}", msg);
                (StatusCode::BAD_GATEWAY, "Upstream service error", None)
            }
            AppError::Internal(msg) => {
                error!("Internal error: {}", msg);
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error", None)
            }
            AppError::Preprocessing(msg) => {
                (StatusCode::BAD_REQUEST, "Request preprocessing failed", Some(msg.clone()))
            }
            AppError::Encryption(msg) => {
                error!("Encryption error: {}", msg);
                (StatusCode::INTERNAL_SERVER_ERROR, "Encryption operation failed", None)
            }
        };

        let body = Json(ErrorResponse {
            error: error_message.to_string(),
            details,
        });

        (status, body).into_response()
    }
}

// Convenience conversion from crate::encryption::EncryptionError
impl From<crate::encryption::EncryptionError> for AppError {
    fn from(e: crate::encryption::EncryptionError) -> Self {
        AppError::Encryption(e.to_string())
    }
}

/// Result type alias for handlers
pub type AppResult<T> = Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_status_codes() {
        assert_eq!(
            AppError::Unauthorized.into_response().status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            AppError::Forbidden.into_response().status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            AppError::NotFound("test".to_string()).into_response().status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            AppError::BadRequest("test".to_string()).into_response().status(),
            StatusCode::BAD_REQUEST
        );
    }
}
