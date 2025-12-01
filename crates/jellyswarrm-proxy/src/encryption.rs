//! Encryption utilities for securing server mapping passwords
//!
//! This module provides functions to encrypt and decrypt server mapping passwords
//! using the user's master password with AES-GCM encryption.

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use base64::{engine::general_purpose, Engine as _};
use rand::RngCore;
use std::string::ToString;

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
/// * `master_password` - The master password used as encryption key
///
/// # Returns
/// Base64-encoded string containing the nonce and encrypted data
pub fn encrypt_password(plaintext: &str, master_password: &str) -> Result<String, EncryptionError> {
    tracing::debug!("Encrypting password with master password");

    // Derive a 32-byte key from the master password using SHA-256
    let key = derive_key(master_password);
    let cipher = Aes256Gcm::new(&key.into());

    // Generate a random 12-byte nonce
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    // Encrypt the plaintext
    let plaintext_bytes = plaintext.as_bytes();
    let ciphertext = cipher.encrypt(nonce, plaintext_bytes).map_err(|e| {
        tracing::error!("Encryption failed: {}", e);
        EncryptionError::EncryptionFailed(e.to_string())
    })?;

    // Combine nonce and ciphertext
    let mut combined = Vec::with_capacity(nonce_bytes.len() + ciphertext.len());
    combined.extend_from_slice(&nonce_bytes);
    combined.extend_from_slice(&ciphertext);

    // Encode as base64 for storage
    let encoded = general_purpose::STANDARD.encode(&combined);
    tracing::debug!("Password encrypted successfully");
    Ok(encoded)
}

/// Decrypts a password using the provided master password
///
/// # Arguments
/// * `encrypted_data` - Base64-encoded string containing nonce and encrypted data
/// * `master_password` - The master password used as decryption key
///
/// # Returns
/// The decrypted password as a String
pub fn decrypt_password(
    encrypted_data: &str,
    master_password: &str,
) -> Result<String, EncryptionError> {
    tracing::debug!("Decrypting password with master password");

    // Decode the base64 data
    let combined = general_purpose::STANDARD
        .decode(encrypted_data)
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
    let key = derive_key(master_password);
    let cipher = Aes256Gcm::new(&key.into());

    // Decrypt the ciphertext
    let plaintext_bytes = cipher.decrypt(nonce, ciphertext).map_err(|e| {
        tracing::error!("Decryption failed: {}", e);
        EncryptionError::PasswordDecryptionFailed
    })?;

    // Convert to string
    let result = String::from_utf8(plaintext_bytes).map_err(|e| {
        tracing::error!("Invalid UTF-8 in decrypted data: {}", e);
        EncryptionError::DecryptionFailed(format!("Invalid UTF-8: {}", e))
    })?;

    tracing::debug!("Password decrypted successfully");
    Ok(result)
}

/// Derives a 32-byte key from a password using SHA-256
fn derive_key(password: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
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
        let password = "my_secret_password";
        let master_password = "master_key_123";

        let encrypted = encrypt_password(password, master_password).unwrap();
        let decrypted = decrypt_password(&encrypted, master_password).unwrap();

        assert_eq!(password, decrypted);
    }

    #[test]
    fn test_decrypt_with_wrong_key() {
        let password = "my_secret_password";
        let master_password = "master_key_123";
        let wrong_password = "wrong_key_456";

        let encrypted = encrypt_password(password, master_password).unwrap();
        let result = decrypt_password(&encrypted, wrong_password);

        assert!(result.is_err());
        matches!(
            result.unwrap_err(),
            EncryptionError::PasswordDecryptionFailed
        );
    }

    #[test]
    fn test_empty_password() {
        let password = "";
        let master_password = "master_key_123";

        let encrypted = encrypt_password(password, master_password).unwrap();
        let decrypted = decrypt_password(&encrypted, master_password).unwrap();

        assert_eq!(password, decrypted);
    }

    #[test]
    fn test_special_characters() {
        let password = "p@ssw0rd!#$%^&*()_+-=[]{}|;':\",./<>?";
        let master_password = "m@st3r_k3y!@#$%^&*()";

        let encrypted = encrypt_password(password, master_password).unwrap();
        let decrypted = decrypt_password(&encrypted, master_password).unwrap();

        assert_eq!(password, decrypted);
    }
}
