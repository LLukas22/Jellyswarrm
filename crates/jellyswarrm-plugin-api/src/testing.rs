//! An in-memory [`Host`] for plugin tests (feature `testing`).
//!
//! Add the API crate again as a dev-dependency with the feature:
//!
//! ```toml
//! [dev-dependencies]
//! jellyswarrm-plugin-api = { workspace = true, features = ["testing"] }
//! ```
//!
//! ```
//! use std::sync::Arc;
//! use jellyswarrm_plugin_api::{testing::TestHost, Caller, CredentialKind, Host};
//!
//! let alice = Caller {
//!     user_id: "alice-id".into(), user_name: "alice".into(), credential: CredentialKind::Login,
//! };
//! let host = Arc::new(
//!     TestHost::new()
//!         .with_settings(serde_json::json!({ "service_url": "http://service" }))
//!         .with_user("alice-token", alice.clone()),
//! );
//! host.set_secret(&alice, "session", Some("seeded"));
//! let as_host: Arc<dyn Host> = host.clone();
//! // `PLUGIN.router(as_host)` … then inspect `host.secret(&alice, "session")`.
//! # let _ = as_host;
//! ```
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
};

use axum::http::request::Parts;
use serde_json::Value;

use crate::{Caller, Host, HostError, SecretRevision, UserSecret};

/// Behaves like the proxy's host: secrets are per caller and key, and every
/// write gets a fresh [`SecretRevision`].
pub struct TestHost {
    server_name: String,
    url_prefix: Option<String>,
    settings: Option<Value>,
    users: HashMap<String, Caller>,
    secrets: Mutex<HashMap<(String, String), UserSecret>>,
    revisions: AtomicU64,
}

impl Default for TestHost {
    fn default() -> Self {
        Self::new()
    }
}

impl TestHost {
    /// No settings, no users, server name `Jellyswarrm`.
    pub fn new() -> Self {
        Self {
            server_name: "Jellyswarrm".into(),
            url_prefix: None,
            settings: None,
            users: HashMap::new(),
            secrets: Mutex::default(),
            revisions: AtomicU64::new(0),
        }
    }

    pub fn with_server_name(mut self, name: impl Into<String>) -> Self {
        self.server_name = name.into();
        self
    }

    pub fn with_url_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.url_prefix = Some(prefix.into());
        self
    }

    /// The plugin's `[plugins.<id>]` table.
    pub fn with_settings(mut self, settings: Value) -> Self {
        self.settings = Some(settings);
        self
    }

    /// Requests carrying `token`, as `Authorization: MediaBrowser Token="…"`
    /// or `X-Emby-Token`, are signed in as `caller`.
    pub fn with_user(mut self, token: impl Into<String>, caller: Caller) -> Self {
        self.users.insert(token.into(), caller);
        self
    }

    /// The stored value for `caller` and `key`, to inspect in assertions.
    pub fn secret(&self, caller: &Caller, key: &str) -> Option<String> {
        self.secrets
            .lock()
            .unwrap()
            .get(&slot(caller, key))
            .map(|secret| secret.value.clone())
    }

    /// Store or remove a value directly, e.g. to seed a test or to simulate a
    /// concurrent request. Gets a fresh revision like any write.
    pub fn set_secret(&self, caller: &Caller, key: &str, value: Option<&str>) {
        let mut secrets = self.secrets.lock().unwrap();
        match value {
            Some(value) => {
                secrets.insert(slot(caller, key), self.versioned(value));
            }
            None => {
                secrets.remove(&slot(caller, key));
            }
        }
    }

    fn versioned(&self, value: &str) -> UserSecret {
        let revision = self.revisions.fetch_add(1, Ordering::Relaxed);
        UserSecret {
            value: value.to_owned(),
            revision: SecretRevision::new(revision.to_string()),
        }
    }

    fn token(parts: &Parts) -> Option<&str> {
        let header = |name: &str| parts.headers.get(name)?.to_str().ok();
        header("authorization")
            .and_then(quoted_token)
            .or_else(|| header("x-emby-authorization").and_then(quoted_token))
            .or_else(|| header("x-emby-token"))
    }
}

/// The `Token="…"` value of a `MediaBrowser` authorization header.
fn quoted_token(header: &str) -> Option<&str> {
    let start = header.find("Token=\"")? + "Token=\"".len();
    header[start..].split('"').next()
}

fn slot(caller: &Caller, key: &str) -> (String, String) {
    (caller.user_id.clone(), key.to_owned())
}

#[async_trait::async_trait]
impl Host for TestHost {
    async fn server_name(&self) -> String {
        self.server_name.clone()
    }

    fn url_prefix(&self) -> Option<&str> {
        self.url_prefix.as_deref()
    }

    fn settings(&self) -> Option<&Value> {
        self.settings.as_ref()
    }

    async fn caller(&self, parts: &Parts) -> Result<Option<Caller>, HostError> {
        Ok(Self::token(parts).and_then(|token| self.users.get(token).cloned()))
    }

    async fn user_secret(
        &self,
        caller: &Caller,
        key: &str,
    ) -> Result<Option<UserSecret>, HostError> {
        Ok(self
            .secrets
            .lock()
            .unwrap()
            .get(&slot(caller, key))
            .cloned())
    }

    async fn set_user_secret(
        &self,
        caller: &Caller,
        key: &str,
        value: &str,
    ) -> Result<(), HostError> {
        self.set_secret(caller, key, Some(value));
        Ok(())
    }

    async fn replace_user_secret(
        &self,
        caller: &Caller,
        key: &str,
        expected: &SecretRevision,
        value: Option<&str>,
    ) -> Result<bool, HostError> {
        let mut secrets = self.secrets.lock().unwrap();
        let slot = slot(caller, key);
        if secrets.get(&slot).map(|secret| &secret.revision) != Some(expected) {
            return Ok(false);
        }
        match value {
            Some(value) => secrets.insert(slot, self.versioned(value)),
            None => secrets.remove(&slot),
        };
        Ok(true)
    }

    async fn take_user_secret(
        &self,
        caller: &Caller,
        key: &str,
    ) -> Result<Option<UserSecret>, HostError> {
        Ok(self.secrets.lock().unwrap().remove(&slot(caller, key)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;

    fn alice() -> Caller {
        Caller {
            user_id: "alice-id".into(),
            user_name: "alice".into(),
            credential: crate::CredentialKind::Login,
        }
    }

    #[tokio::test]
    async fn signs_in_by_token_and_guards_revisions_like_the_proxy() {
        let host = TestHost::new().with_user("t", alice());
        for (name, value, expected) in [
            ("authorization", "MediaBrowser Token=\"t\"", Some(alice())),
            ("x-emby-token", "t", Some(alice())),
            ("x-emby-token", "other", None),
        ] {
            let (parts, ()) = Request::get("/")
                .header(name, value)
                .body(())
                .unwrap()
                .into_parts();
            assert_eq!(
                host.caller(&parts).await.unwrap(),
                expected,
                "{name}: {value}"
            );
        }

        host.set_user_secret(&alice(), "k", "one").await.unwrap();
        let first = host.user_secret(&alice(), "k").await.unwrap().unwrap();
        assert_eq!(format!("{first:?}"), "UserSecret([redacted])");
        // Rewriting the same value still invalidates the old revision.
        host.set_user_secret(&alice(), "k", "one").await.unwrap();
        assert!(!host
            .replace_user_secret(&alice(), "k", &first.revision, None)
            .await
            .unwrap());
        let current = host.user_secret(&alice(), "k").await.unwrap().unwrap();
        assert!(host
            .replace_user_secret(&alice(), "k", &current.revision, Some("two"))
            .await
            .unwrap());
        assert_eq!(host.secret(&alice(), "k").as_deref(), Some("two"));
    }
}
