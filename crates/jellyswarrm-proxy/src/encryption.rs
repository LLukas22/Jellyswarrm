//! Encryption utilities for securing server mapping passwords
//!
//! This module provides functions to encrypt and decrypt server mapping passwords
//! using the user's master password with AES-GCM encryption.
//!
//! Password hashing uses Argon2id for secure password storage.

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use base64::{engine::general_purpose, Engine as _};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::string::ToString;

/// A wrapper type for plaintext passwords.
#[derive(Clone, PartialEq, Eq, Hash, sqlx::Type, Serialize, Deserialize)]
#[sqlx(transparent)]
pub struct Password(String);

impl std::fmt::Debug for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Password").field(&"***").finish()
    }
}

impl std::fmt::Display for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "***")
    }
}

impl Password {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl From<String> for Password {
    fn from(password: String) -> Self {
        Self(password)
    }
}

impl From<&str> for Password {
    fn from(password: &str) -> Self {
        Self(password.to_string())
    }
}

/// Hash a password using SHA-256 for encryption key derivation.
///
/// IMPORTANT: This function is used for deriving encryption keys for server
/// password mappings. It MUST use SHA-256 (not Argon2) to maintain backward
/// compatibility with existing encrypted data.
///
/// Returns a deterministic 64-character hex string.
pub fn hash_password(password: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(password.as_bytes());
    hex::encode(hasher.finalize())
}

/// Hash a password using Argon2id for secure storage.
///
/// Returns an Argon2 PHC string format hash (includes algorithm, params, salt, and hash).
/// Use this for storing passwords that need to be verified later.
pub fn hash_password_argon2(password: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();

    argon2
        .hash_password(password.as_bytes(), &salt)
        .expect("Failed to hash password")
        .to_string()
}

/// Verify password against stored hash.
///
/// Supports both Argon2 and SHA-256 hashes for backwards compatibility.
pub fn verify_password(password: &str, stored_hash: &str) -> bool {
    // Try Argon2 verification first (new format)
    if let Ok(parsed_hash) = PasswordHash::new(stored_hash) {
        return Argon2::default()
            .verify_password(password.as_bytes(), &parsed_hash)
            .is_ok();
    }

    // Fallback to SHA-256 for legacy/encryption-key hashes (64 hex chars)
    if stored_hash.len() == 64 && stored_hash.chars().all(|c| c.is_ascii_hexdigit()) {
        let mut hasher = Sha256::new();
        hasher.update(password.as_bytes());
        let sha256_hash = hex::encode(hasher.finalize());
        return sha256_hash == stored_hash;
    }

    false
}

/// Check if a hash is in the legacy SHA-256 format and needs migration
pub fn needs_hash_migration(stored_hash: &str) -> bool {
    // SHA-256 hashes are exactly 64 hex characters
    stored_hash.len() == 64 && stored_hash.chars().all(|c| c.is_ascii_hexdigit())
}

/// A wrapper type for hashed passwords used for encryption key derivation.
///
/// IMPORTANT: This type uses SHA-256 hashing because it's used to derive
/// encryption keys for server password mappings. Changing the hash algorithm
/// would break decryption of existing data.
///
/// For secure password storage (e.g., user authentication), consider using
/// `hash_password_argon2()` instead.
#[derive(Clone, PartialEq, Eq, Debug, Hash, sqlx::Type, Serialize, Deserialize)]
#[sqlx(transparent)]
pub struct HashedPassword(String);

impl HashedPassword {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }

    /// Create a new hashed password from plaintext.
    /// Uses SHA-256 for encryption key derivation compatibility.
    pub fn from_password(password: &str) -> Self {
        Self(hash_password(password))
    }

    /// Verify a plaintext password against this hash.
    /// Supports both SHA-256 and Argon2 formats.
    pub fn verify(&self, password: &str) -> bool {
        verify_password(password, &self.0)
    }

    pub fn from_hashed(hashed: String) -> Self {
        Self(hashed)
    }
}

impl From<Password> for HashedPassword {
    fn from(password: Password) -> Self {
        Self::from_password(password.as_str())
    }
}

impl From<&Password> for HashedPassword {
    fn from(password: &Password) -> Self {
        Self::from_password(password.as_str())
    }
}

/// A wrapper type for encrypted passwords.
#[derive(Clone, PartialEq, Eq, Debug, Hash, sqlx::Type, Serialize, Deserialize)]
#[sqlx(transparent)]
pub struct EncryptedPassword(String);

impl EncryptedPassword {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }

    pub fn from_raw(raw: String) -> Self {
        Self(raw)
    }
}

/// Custom error type for encryption/decryption operations
#[derive(Debug, thiserror::Error)]
pub enum EncryptionError {
    #[error("Encryption failed: {0}")]
    EncryptionFailed(String),
    #[error("Decryption failed: {0}")]
    DecryptionFailed(String),
    #[error("Base64 decoding failed: {0}")]
    Base64DecodeFailed(#[from] base64::DecodeError),
    #[error("Invalid nonce size")]
    InvalidNonceSize,
    #[error("Password decryption failed - possibly incorrect password")]
    PasswordDecryptionFailed,
}

/// Encrypts a password using the provided master password
///
/// # Arguments
/// * `plaintext` - The password to encrypt
/// * `master_password` - The hashed master password used as encryption key
///
/// # Returns
/// Base64-encoded string containing the nonce and encrypted data
pub fn encrypt_password(
    plaintext: &Password,
    master_password: &HashedPassword,
) -> Result<EncryptedPassword, EncryptionError> {
    tracing::debug!("Encrypting password with master password");

    // Derive a 32-byte key from the master password using SHA-256
    let key = derive_key(master_password.as_str());
    let cipher = Aes256Gcm::new(&key.into());

    // Generate a random 12-byte nonce
    let mut nonce_bytes = [0u8; 12];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    // Encrypt the plaintext
    let plaintext_bytes = plaintext.as_str().as_bytes();
    let ciphertext = cipher.encrypt(nonce, plaintext_bytes).map_err(|e| {
        tracing::error!("Encryption failed: {}", e);
        EncryptionError::EncryptionFailed(e.to_string())
    })?;

    // Combine nonce and ciphertext
    let mut combined = Vec::with_capacity(nonce_bytes.len() + ciphertext.len());
    combined.extend_from_slice(&nonce_bytes);
    combined.extend_from_slice(&ciphertext);

    // Encode as base64 for storage
    let encoded = EncryptedPassword(general_purpose::STANDARD.encode(&combined));
    tracing::debug!("Password encrypted successfully");
    Ok(encoded)
}

/// Decrypts a password using the provided master password
///
/// # Arguments
/// * `encrypted_data` - Base64-encoded string containing nonce and encrypted data
/// * `master_password` - The hashed master password used as decryption key
///
/// # Returns
/// The decrypted password as a String
pub fn decrypt_password(
    encrypted_data: &EncryptedPassword,
    master_password: &HashedPassword,
) -> Result<Password, EncryptionError> {
    tracing::debug!("Decrypting password with master password");

    // Decode the base64 data
    let combined = general_purpose::STANDARD
        .decode(encrypted_data.as_str())
        .map_err(|e| {
            tracing::error!("Base64 decoding failed: {}", e);
            EncryptionError::Base64DecodeFailed(e)
        })?;

    // Extract nonce (first 12 bytes) and ciphertext (remaining bytes)
    if combined.len() < 12 {
        tracing::error!(
            "Invalid nonce size: expected at least 12 bytes, got {}",
            combined.len()
        );
        return Err(EncryptionError::InvalidNonceSize);
    }

    let (nonce_bytes, ciphertext) = combined.split_at(12);
    let nonce = Nonce::from_slice(nonce_bytes);

    // Derive the same key from the master password
    let key = derive_key(master_password.as_str());
    let cipher = Aes256Gcm::new(&key.into());

    // Decrypt the ciphertext
    let plaintext_bytes = cipher.decrypt(nonce, ciphertext).map_err(|e| {
        tracing::error!("Decryption failed: {}", e);
        EncryptionError::PasswordDecryptionFailed
    })?;

    // Convert to string
    let result = Password(String::from_utf8(plaintext_bytes).map_err(|e| {
        tracing::error!("Invalid UTF-8 in decrypted data: {}", e);
        EncryptionError::DecryptionFailed(format!("Invalid UTF-8: {}", e))
    })?);

    tracing::debug!("Password decrypted successfully");
    Ok(result)
}

/// Derives a 32-byte key from a password using SHA-256
fn derive_key(password: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(password.as_bytes());
    let result = hasher.finalize();
    let mut key = [0u8; 32];
    key.copy_from_slice(&result[..32]);
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt() {
        let password = Password("my_secret_password".into());
        let master_password = HashedPassword::from_password("master_key_123");

        let encrypted = encrypt_password(&password, &master_password).unwrap();
        let decrypted = decrypt_password(&encrypted, &master_password).unwrap();

        assert_eq!(password, decrypted);
    }

    #[test]
    fn test_decrypt_with_wrong_key() {
        let password = Password("my_secret_password".into());
        let master_password = HashedPassword::from_password("master_key_123");
        let wrong_password = HashedPassword::from_password("wrong_key_456");

        let encrypted = encrypt_password(&password, &master_password).unwrap();
        let result = decrypt_password(&encrypted, &wrong_password);

        assert!(result.is_err());
        matches!(
            result.unwrap_err(),
            EncryptionError::PasswordDecryptionFailed
        );
    }

    #[test]
    fn test_empty_password() {
        let password = Password("".into());
        let master_password = HashedPassword::from_password("master_key_123");

        let encrypted = encrypt_password(&password, &master_password).unwrap();
        let decrypted = decrypt_password(&encrypted, &master_password).unwrap();

        assert_eq!(password, decrypted);
    }

    #[test]
    fn test_special_characters() {
        let password = Password("p@ssw0rd!#$%^&*()_+-=[]{}|;':\",./<>?".into());
        let master_password = HashedPassword::from_password("m@st3r_k3y!@#$%^&*()");

        let encrypted = encrypt_password(&password, &master_password).unwrap();
        let decrypted = decrypt_password(&encrypted, &master_password).unwrap();

        assert_eq!(password, decrypted);
    }

    #[test]
    fn test_sha256_password_hashing() {
        let password = "test_password_123";
        let hashed = HashedPassword::from_password(password);

        // Verify SHA-256 format (64 hex chars)
        assert_eq!(hashed.as_str().len(), 64);
        assert!(hashed.as_str().chars().all(|c| c.is_ascii_hexdigit()));

        // Verify correct password works
        assert!(hashed.verify(password));

        // Verify wrong password fails
        assert!(!hashed.verify("wrong_password"));
    }

    #[test]
    fn test_argon2_password_hashing() {
        let password = "test_password_123";
        let hashed = hash_password_argon2(password);

        // Verify Argon2 format (starts with $argon2)
        assert!(hashed.starts_with("$argon2"));

        // Verify correct password works
        assert!(verify_password(password, &hashed));

        // Verify wrong password fails
        assert!(!verify_password("wrong_password", &hashed));
    }

    #[test]
    fn test_verify_password_function() {
        let password = "test123";

        // Test SHA-256 hash verification
        let sha256_hash = hash_password(password);
        assert!(verify_password(password, &sha256_hash));
        assert!(!verify_password("wrong", &sha256_hash));

        // Test Argon2 hash verification
        let argon2_hash = hash_password_argon2(password);
        assert!(verify_password(password, &argon2_hash));
        assert!(!verify_password("wrong", &argon2_hash));
    }

    #[test]
    fn test_sha256_hash_needs_migration() {
        let password = "test123";
        let sha256_hash = hash_password(password);

        // SHA-256 hashes should be identified as needing migration
        assert!(needs_hash_migration(&sha256_hash));
    }

    #[test]
    fn test_argon2_hash_does_not_need_migration() {
        let password = "test123";
        let argon2_hash = hash_password_argon2(password);

        // Argon2 hashes should NOT need migration
        assert!(!needs_hash_migration(&argon2_hash));
    }

    #[test]
    fn test_sha256_deterministic() {
        // SHA-256 should produce the same hash for the same password
        let password = "same_password";
        let hash1 = hash_password(password);
        let hash2 = hash_password(password);

        // Hashes should be identical (deterministic)
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_argon2_unique_hashes() {
        // Argon2 with random salt should produce different hashes
        let password = "same_password";
        let hash1 = hash_password_argon2(password);
        let hash2 = hash_password_argon2(password);

        // Hashes should be different (different salts)
        assert_ne!(hash1, hash2);

        // But both should verify correctly
        assert!(verify_password(password, &hash1));
        assert!(verify_password(password, &hash2));
    }
}
