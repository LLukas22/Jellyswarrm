//! What a plugin's own handlers may ask of the proxy.
use std::sync::Arc;

use axum::{
    extract::{FromRef, FromRequestParts},
    http::{request::Parts, StatusCode},
};

/// A decrypted value and its revision, scoped to one plugin, caller and key.
/// Read the value in memory and pass the revision back for a conditional write.
#[derive(Clone, PartialEq, Eq)]
pub struct UserSecret {
    pub value: String,
    pub revision: SecretRevision,
}

impl std::fmt::Debug for UserSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UserSecret([redacted])")
    }
}

/// Opaque write version of a [`UserSecret`]. Every write gets a fresh one,
/// even when the value is unchanged. Plugins only pass it back to
/// [`Host::replace_user_secret`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRevision(String);

impl SecretRevision {
    /// For [`Host`] implementations only.
    #[doc(hidden)]
    pub fn new(token: String) -> Self {
        Self(token)
    }

    /// For [`Host`] implementations only.
    #[doc(hidden)]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The proxy, as seen by one plugin. Each plugin gets its own instance.
#[async_trait::async_trait]
pub trait Host: Send + Sync + 'static {
    /// The proxy's configured server name, read when called.
    async fn server_name(&self) -> String;

    /// The URL prefix the proxy is served under, without slashes
    /// (`Some("jellyfin")` for `https://host/jellyfin/…`).
    fn url_prefix(&self) -> Option<&str>;

    /// This plugin's `[plugins.<id>]` table from the proxy configuration, as
    /// read at startup. Plugins usually read it typed, with
    /// [`settings_as`](trait.Host.html#method.settings_as).
    fn settings(&self) -> Option<&serde_json::Value>;

    /// The signed-in proxy user making the request, using the proxy's token
    /// rules. `None` when the request carries no valid token.
    async fn caller(&self, parts: &Parts) -> Result<Option<Caller>, HostError>;

    /// A value this plugin stored for `caller` under `key`, encrypted at rest.
    /// Values are deleted together with the user.
    async fn user_secret(
        &self,
        caller: &Caller,
        key: &str,
    ) -> Result<Option<UserSecret>, HostError>;

    /// Create or overwrite the value for this caller and key.
    async fn set_user_secret(
        &self,
        caller: &Caller,
        key: &str,
        value: &str,
    ) -> Result<(), HostError>;

    /// Atomically remove and return the current value. Use this before revoking
    /// an external session so a later login keeps its own secret. The revision
    /// belongs to the removed value.
    async fn take_user_secret(
        &self,
        caller: &Caller,
        key: &str,
    ) -> Result<Option<UserSecret>, HostError>;

    /// Replace the value, or delete it with `None`, only if the revision from
    /// [`Host::user_secret`] still matches. Returns false on a stale or missing row.
    async fn replace_user_secret(
        &self,
        caller: &Caller,
        key: &str,
        expected: &SecretRevision,
        value: Option<&str>,
    ) -> Result<bool, HostError>;
}

/// A signed-in proxy user. Extract it in a handler to require sign-in: the
/// request is rejected with 401 otherwise. Read-only API keys are already
/// limited to `GET` and `HEAD` by the proxy before a plugin sees the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    /// The proxy (virtual) user ID, not a backend user ID.
    pub user_id: String,
    pub user_name: String,
    pub credential: CredentialKind,
}

/// The credential that authenticated this request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CredentialKind {
    Login,
    ReadOnlyApiKey,
}

impl<S> FromRequestParts<S> for Caller
where
    Arc<dyn Host>: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Arc::<dyn Host>::from_ref(state).caller(parts).await {
            Ok(Some(caller)) => Ok(caller),
            Ok(None) => Err(StatusCode::UNAUTHORIZED),
            Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
        }
    }
}

impl dyn Host {
    /// This plugin's settings as `T`. `Ok(None)` when the configuration has no
    /// `[plugins.<id>]` table; an error when the table doesn't match `T`.
    ///
    /// ```
    /// # use jellyswarrm_plugin_api::{testing::TestHost, Host};
    /// #[derive(serde::Deserialize)]
    /// #[serde(deny_unknown_fields)]
    /// struct Settings {
    ///     service_url: String,
    /// }
    ///
    /// let host: &dyn Host = &TestHost::new().with_settings(serde_json::json!({
    ///     "service_url": "http://service:8080",
    /// }));
    /// let settings: Settings = host.settings_as().unwrap().unwrap();
    /// assert_eq!(settings.service_url, "http://service:8080");
    /// ```
    pub fn settings_as<T: serde::de::DeserializeOwned>(
        &self,
    ) -> Result<Option<T>, serde_json::Error> {
        self.settings().map(T::deserialize).transpose()
    }
}

/// The proxy failed to answer, e.g. its database was unavailable. The proxy
/// logs the cause; plugins usually answer with a 500.
#[derive(Debug)]
pub struct HostError(String);

impl HostError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HostError {}
