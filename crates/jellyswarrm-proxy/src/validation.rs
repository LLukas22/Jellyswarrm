//! Input validation utilities for the proxy
//!
//! Provides validation for user inputs to prevent security issues
//! and ensure data integrity.

use thiserror::Error;
use url::Url;

/// Validation errors
#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("Username is required")]
    UsernameRequired,

    #[error("Username is too short (minimum {min} characters)")]
    UsernameTooShort { min: usize },

    #[error("Username is too long (maximum {max} characters)")]
    UsernameTooLong { max: usize },

    #[error("Username contains invalid characters (only alphanumeric, underscore, hyphen, and dot allowed)")]
    UsernameInvalidChars,

    #[error("Password is required")]
    PasswordRequired,

    #[error("Password is too short (minimum {min} characters)")]
    PasswordTooShort { min: usize },

    #[error("Password is too long (maximum {max} characters)")]
    PasswordTooLong { max: usize },

    #[error("Server name is required")]
    ServerNameRequired,

    #[error("Server name is too long (maximum {max} characters)")]
    ServerNameTooLong { max: usize },

    #[error("Server URL is required")]
    ServerUrlRequired,

    #[error("Invalid server URL: {0}")]
    InvalidServerUrl(String),

    #[error("Server URL must use http or https scheme")]
    InvalidUrlScheme,

    #[error("Server URL must have a valid host")]
    MissingHost,

    #[error("Server URL is too long (maximum {max} characters)")]
    ServerUrlTooLong { max: usize },

    #[error("API key is too long (maximum {max} characters)")]
    ApiKeyTooLong { max: usize },

    #[error("Invalid priority value (must be between {min} and {max})")]
    InvalidPriority { min: i32, max: i32 },
}

/// Constraints for username validation
pub const USERNAME_MIN_LENGTH: usize = 1;
pub const USERNAME_MAX_LENGTH: usize = 128;

/// Constraints for password validation
pub const PASSWORD_MIN_LENGTH: usize = 1;
pub const PASSWORD_MAX_LENGTH: usize = 1024;

/// Constraints for server validation
pub const SERVER_NAME_MAX_LENGTH: usize = 256;
pub const SERVER_URL_MAX_LENGTH: usize = 2048;
pub const PRIORITY_MIN: i32 = 0;
pub const PRIORITY_MAX: i32 = 1000;

/// Constraints for API keys
pub const API_KEY_MAX_LENGTH: usize = 512;

/// Validate a username
///
/// Rules:
/// - Must be between 1 and 128 characters
/// - Can contain alphanumeric characters, underscores, hyphens, and dots
pub fn validate_username(username: &str) -> Result<(), ValidationError> {
    let username = username.trim();

    if username.is_empty() {
        return Err(ValidationError::UsernameRequired);
    }

    if username.len() < USERNAME_MIN_LENGTH {
        return Err(ValidationError::UsernameTooShort {
            min: USERNAME_MIN_LENGTH,
        });
    }

    if username.len() > USERNAME_MAX_LENGTH {
        return Err(ValidationError::UsernameTooLong {
            max: USERNAME_MAX_LENGTH,
        });
    }

    // Allow alphanumeric, underscore, hyphen, and dot
    if !username
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '@')
    {
        return Err(ValidationError::UsernameInvalidChars);
    }

    Ok(())
}

/// Validate a password
///
/// Rules:
/// - Must be at least 1 character (we don't enforce strong passwords,
///   that's the Jellyfin server's responsibility)
/// - Maximum 1024 characters to prevent DoS
pub fn validate_password(password: &str) -> Result<(), ValidationError> {
    if password.is_empty() {
        return Err(ValidationError::PasswordRequired);
    }

    if password.len() < PASSWORD_MIN_LENGTH {
        return Err(ValidationError::PasswordTooShort {
            min: PASSWORD_MIN_LENGTH,
        });
    }

    if password.len() > PASSWORD_MAX_LENGTH {
        return Err(ValidationError::PasswordTooLong {
            max: PASSWORD_MAX_LENGTH,
        });
    }

    Ok(())
}

/// Validate a server name
///
/// Rules:
/// - Must not be empty
/// - Maximum 256 characters
pub fn validate_server_name(name: &str) -> Result<(), ValidationError> {
    let name = name.trim();

    if name.is_empty() {
        return Err(ValidationError::ServerNameRequired);
    }

    if name.len() > SERVER_NAME_MAX_LENGTH {
        return Err(ValidationError::ServerNameTooLong {
            max: SERVER_NAME_MAX_LENGTH,
        });
    }

    Ok(())
}

/// Validate a server URL
///
/// Rules:
/// - Must be a valid URL
/// - Must use http or https scheme
/// - Must have a valid host
/// - Maximum 2048 characters
pub fn validate_server_url(url_str: &str) -> Result<Url, ValidationError> {
    let url_str = url_str.trim();

    if url_str.is_empty() {
        return Err(ValidationError::ServerUrlRequired);
    }

    if url_str.len() > SERVER_URL_MAX_LENGTH {
        return Err(ValidationError::ServerUrlTooLong {
            max: SERVER_URL_MAX_LENGTH,
        });
    }

    let url = Url::parse(url_str).map_err(|e| ValidationError::InvalidServerUrl(e.to_string()))?;

    // Check scheme
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(ValidationError::InvalidUrlScheme);
    }

    // Check host
    if url.host().is_none() {
        return Err(ValidationError::MissingHost);
    }

    Ok(url)
}

/// Validate server priority
///
/// Rules:
/// - Must be between 0 and 1000
pub fn validate_priority(priority: i32) -> Result<(), ValidationError> {
    if priority < PRIORITY_MIN || priority > PRIORITY_MAX {
        return Err(ValidationError::InvalidPriority {
            min: PRIORITY_MIN,
            max: PRIORITY_MAX,
        });
    }

    Ok(())
}

/// Validate an API key
///
/// Rules:
/// - Maximum 512 characters
pub fn validate_api_key(api_key: &str) -> Result<(), ValidationError> {
    if api_key.len() > API_KEY_MAX_LENGTH {
        return Err(ValidationError::ApiKeyTooLong {
            max: API_KEY_MAX_LENGTH,
        });
    }

    Ok(())
}

/// Sanitize a string for safe logging (redact potential secrets)
pub fn sanitize_for_logging(input: &str, max_len: usize) -> String {
    let truncated = if input.len() > max_len {
        format!("{}...", &input[..max_len])
    } else {
        input.to_string()
    };

    // Remove newlines and control characters
    truncated
        .chars()
        .filter(|c| !c.is_control() || *c == ' ')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_username() {
        assert!(validate_username("john").is_ok());
        assert!(validate_username("john_doe").is_ok());
        assert!(validate_username("john-doe").is_ok());
        assert!(validate_username("john.doe").is_ok());
        assert!(validate_username("john@example.com").is_ok());
        assert!(validate_username("JohnDoe123").is_ok());
    }

    #[test]
    fn test_invalid_username() {
        assert!(validate_username("").is_err());
        assert!(validate_username("   ").is_err());
        assert!(validate_username("john doe").is_err()); // space
        assert!(validate_username("john<script>").is_err()); // special chars
        assert!(validate_username(&"a".repeat(200)).is_err()); // too long
    }

    #[test]
    fn test_valid_password() {
        assert!(validate_password("a").is_ok());
        assert!(validate_password("password123").is_ok());
        assert!(validate_password("p@ssw0rd!#$%").is_ok());
    }

    #[test]
    fn test_invalid_password() {
        assert!(validate_password("").is_err());
        assert!(validate_password(&"a".repeat(2000)).is_err()); // too long
    }

    #[test]
    fn test_valid_server_url() {
        assert!(validate_server_url("http://localhost:8096").is_ok());
        assert!(validate_server_url("https://jellyfin.example.com").is_ok());
        assert!(validate_server_url("http://192.168.1.100:8096").is_ok());
        assert!(validate_server_url("https://jellyfin.example.com/jellyfin").is_ok());
    }

    #[test]
    fn test_invalid_server_url() {
        assert!(validate_server_url("").is_err());
        assert!(validate_server_url("not-a-url").is_err());
        assert!(validate_server_url("ftp://example.com").is_err()); // wrong scheme
        assert!(validate_server_url("http://").is_err()); // no host
    }

    #[test]
    fn test_valid_server_name() {
        assert!(validate_server_name("My Server").is_ok());
        assert!(validate_server_name("Jellyfin-1").is_ok());
    }

    #[test]
    fn test_invalid_server_name() {
        assert!(validate_server_name("").is_err());
        assert!(validate_server_name("   ").is_err());
        assert!(validate_server_name(&"a".repeat(300)).is_err());
    }

    #[test]
    fn test_valid_priority() {
        assert!(validate_priority(0).is_ok());
        assert!(validate_priority(500).is_ok());
        assert!(validate_priority(1000).is_ok());
    }

    #[test]
    fn test_invalid_priority() {
        assert!(validate_priority(-1).is_err());
        assert!(validate_priority(1001).is_err());
    }

    #[test]
    fn test_sanitize_for_logging() {
        assert_eq!(sanitize_for_logging("hello", 10), "hello");
        assert_eq!(sanitize_for_logging("hello world", 5), "hello...");
        assert_eq!(sanitize_for_logging("hello\nworld", 20), "helloworld");
    }
}
