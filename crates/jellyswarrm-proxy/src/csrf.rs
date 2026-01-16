//! CSRF (Cross-Site Request Forgery) protection
//!
//! Implements double-submit cookie pattern for CSRF protection on admin forms.

use axum::{
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tower_sessions::Session;

const CSRF_TOKEN_KEY: &str = "csrf_token";
const CSRF_TOKEN_LENGTH: usize = 32;

/// CSRF token stored in session
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CsrfToken(pub String);

impl CsrfToken {
    /// Generate a new random CSRF token
    pub fn generate() -> Self {
        let mut bytes = [0u8; CSRF_TOKEN_LENGTH];
        rand::rng().fill_bytes(&mut bytes);
        CsrfToken(hex::encode(bytes))
    }

    /// Get the token value
    pub fn value(&self) -> &str {
        &self.0
    }

    /// Verify a submitted token matches this token
    pub fn verify(&self, submitted: &str) -> bool {
        // Use constant-time comparison to prevent timing attacks
        constant_time_eq(self.0.as_bytes(), submitted.as_bytes())
    }
}

/// Constant-time string comparison to prevent timing attacks
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut result = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        result |= x ^ y;
    }
    result == 0
}

/// Extractor that gets or creates a CSRF token from the session
pub struct Csrf(pub CsrfToken);

impl<S> FromRequestParts<S> for Csrf
where
    S: Send + Sync,
{
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let session = parts
            .extensions
            .get::<Session>()
            .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

        // Try to get existing token or create a new one
        let token: CsrfToken = match session.get(CSRF_TOKEN_KEY).await {
            Ok(Some(token)) => token,
            _ => {
                let token = CsrfToken::generate();
                if let Err(e) = session.insert(CSRF_TOKEN_KEY, token.clone()).await {
                    tracing::error!("Failed to store CSRF token: {}", e);
                    return Err(StatusCode::INTERNAL_SERVER_ERROR);
                }
                token
            }
        };

        Ok(Csrf(token))
    }
}

/// Validate a submitted CSRF token against the session token
pub async fn validate_csrf_token(session: &Session, submitted_token: &str) -> Result<(), StatusCode> {
    let stored_token: CsrfToken = session
        .get(CSRF_TOKEN_KEY)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::FORBIDDEN)?;

    if !stored_token.verify(submitted_token) {
        tracing::warn!("CSRF token mismatch");
        return Err(StatusCode::FORBIDDEN);
    }

    Ok(())
}

/// Form field name for CSRF token
pub const CSRF_FIELD_NAME: &str = "_csrf";

/// Generate HTML hidden input for CSRF token
pub fn csrf_hidden_input(token: &CsrfToken) -> String {
    format!(
        r#"<input type="hidden" name="{}" value="{}">"#,
        CSRF_FIELD_NAME,
        token.value()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_csrf_token_generation() {
        let token1 = CsrfToken::generate();
        let token2 = CsrfToken::generate();

        // Tokens should be different
        assert_ne!(token1.value(), token2.value());

        // Tokens should be 64 chars (32 bytes hex encoded)
        assert_eq!(token1.value().len(), 64);
    }

    #[test]
    fn test_csrf_token_verification() {
        let token = CsrfToken::generate();

        // Should verify itself
        assert!(token.verify(token.value()));

        // Should not verify different token
        assert!(!token.verify("different_token"));

        // Should not verify empty string
        assert!(!token.verify(""));
    }

    #[test]
    fn test_constant_time_eq() {
        assert!(constant_time_eq(b"test", b"test"));
        assert!(!constant_time_eq(b"test", b"Test"));
        assert!(!constant_time_eq(b"test", b"test1"));
        assert!(!constant_time_eq(b"test1", b"test"));
    }
}
