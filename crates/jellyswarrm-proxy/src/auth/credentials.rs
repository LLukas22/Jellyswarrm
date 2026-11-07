//! Local credential hashing and verification with bounded off-runtime password work.
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::domain::security::{HashedPassword, Password};

// Bound both CPU and memory use (Argon2id uses ~19 MiB per operation).
// The permit stays in the blocking task even if its request is cancelled.
static PASSWORD_WORK: std::sync::LazyLock<std::sync::Arc<tokio::sync::Semaphore>> =
    std::sync::LazyLock::new(|| std::sync::Arc::new(tokio::sync::Semaphore::new(2)));

pub(super) async fn password_work<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, sqlx::Error> {
    let permit = PASSWORD_WORK
        .clone()
        .acquire_owned()
        .await
        .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
    .map_err(|e| sqlx::Error::Protocol(e.to_string()))
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
pub(crate) enum LocalCredential {
    Passwordless,
    Password(HashedPassword),
    Argon2(HashedPassword),
}

impl LocalCredential {
    const PASSWORD_KIND: &'static str = "password";
    const ARGON2_KIND: &'static str = "argon2id";
    const PASSWORDLESS_KIND: &'static str = "passwordless";
    const EMPTY_PASSWORD_HASH: &'static str =
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    pub(crate) async fn from_password(password: &Password) -> Result<Self, sqlx::Error> {
        let password = password.clone();
        password_work(move || Self::hash_password(&password)).await
    }

    fn hash_password(password: &Password) -> Self {
        if password.as_str().is_empty() {
            Self::Passwordless
        } else {
            let mut salt = [0u8; 16];
            rand::rng().fill_bytes(&mut salt);
            let salt = SaltString::encode_b64(&salt).expect("valid random salt");
            let encoded = Argon2::default()
                .hash_password(password.as_str().as_bytes(), &salt)
                .expect("Argon2id password hash")
                .to_string();
            Self::Argon2(HashedPassword::from_hashed(encoded))
        }
    }

    pub(crate) async fn verify_async(&self, password: &Password) -> Result<bool, sqlx::Error> {
        let credential = self.clone();
        let password = password.clone();
        password_work(move || credential.verify(&password)).await
    }

    pub(super) fn verify(&self, password: &Password) -> bool {
        match self {
            Self::Passwordless => password.as_str().is_empty(),
            Self::Password(password_hash) => password_hash.verify(password.as_str()),
            Self::Argon2(hash) => PasswordHash::new(hash.as_str()).ok().is_some_and(|parsed| {
                Argon2::default()
                    .verify_password(password.as_str().as_bytes(), &parsed)
                    .is_ok()
            }),
        }
    }

    pub(crate) fn mapping_key(&self) -> HashedPassword {
        match self {
            Self::Passwordless => HashedPassword::from_password(""),
            Self::Password(password_hash) | Self::Argon2(password_hash) => password_hash.clone(),
        }
    }

    pub(crate) fn session_auth_hash(&self) -> &[u8] {
        match self {
            Self::Passwordless => Self::EMPTY_PASSWORD_HASH.as_bytes(),
            Self::Password(password_hash) | Self::Argon2(password_hash) => {
                password_hash.as_str().as_bytes()
            }
        }
    }

    pub(super) fn kind(&self) -> &'static str {
        match self {
            Self::Passwordless => Self::PASSWORDLESS_KIND,
            Self::Password(_) => Self::PASSWORD_KIND,
            Self::Argon2(_) => Self::ARGON2_KIND,
        }
    }

    pub(super) fn stored_hash(&self) -> HashedPassword {
        self.mapping_key()
    }

    pub(super) fn from_storage(
        kind: &str,
        password_hash: HashedPassword,
    ) -> Result<Self, sqlx::Error> {
        match kind {
            Self::PASSWORD_KIND => Ok(Self::Password(password_hash)),
            Self::ARGON2_KIND => Ok(Self::Argon2(password_hash)),
            Self::PASSWORDLESS_KIND => Ok(Self::Passwordless),
            _ => Err(sqlx::Error::Decode(
                format!("invalid local credential kind: {kind}").into(),
            )),
        }
    }
}
