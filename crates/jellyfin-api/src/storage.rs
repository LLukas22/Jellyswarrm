//! Shared HTTP transport factory, not an authenticated-identity cache.
//!
//! Every handle has independent token state. Neither eviction nor dropping the
//! factory/handle revokes credentials; callers explicitly own `logout`. The
//! historical name and constructor remain source-compatible, but capacity/TTL
//! no longer imply ownership of a remote session. A future session cache must
//! key immutable sessions by stable mapping identity and credential revision.
use crate::{error::Error, ClientInfo, JellyfinClient};
use std::sync::Arc;

#[derive(Clone)]
pub struct JellyfinClientStorage {
    transport: reqwest::Client,
}

impl JellyfinClientStorage {
    /// Compatibility constructor. Capacity/TTL are ignored: this factory caches
    /// no authenticated clients. Connection pooling belongs to the transport.
    pub fn new(_capacity: u64, _ttl: std::time::Duration) -> Self {
        Self::with_transport(reqwest::Client::new())
    }

    pub fn with_transport(transport: reqwest::Client) -> Self {
        Self { transport }
    }

    /// `id` is retained for compatibility only, never used as an identity key.
    /// Two calls with identical URL, metadata and id still return independent
    /// credentials over the same connection pool.
    pub async fn get(
        &self,
        base_url: &str,
        client_info: ClientInfo,
        _id: Option<&str>,
    ) -> Result<Arc<JellyfinClient>, Error> {
        Ok(Arc::new(JellyfinClient::new_with_client(
            base_url,
            client_info,
            self.transport.clone(),
        )?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    #[tokio::test]
    async fn identical_identity_metadata_does_not_share_mutable_credentials() {
        let backend = MockServer::start().await;
        let factory = JellyfinClientStorage::new(1, std::time::Duration::ZERO);
        let first = factory
            .get(&backend.uri(), ClientInfo::default(), Some("same-user"))
            .await
            .unwrap();
        let second = factory
            .get(&backend.uri(), ClientInfo::default(), Some("same-user"))
            .await
            .unwrap();
        first.with_token("first-token".into()).await;
        assert_eq!(second.get_token().await, None);
        second.with_token("second-token".into()).await;
        assert_eq!(first.get_token().await.as_deref(), Some("first-token"));
        drop(first);
        drop(factory);
        assert_eq!(second.get_token().await.as_deref(), Some("second-token"));
        assert!(backend.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn only_explicit_logout_revokes_the_owned_session() {
        let backend = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/Sessions/Logout"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&backend)
            .await;
        let factory = JellyfinClientStorage::with_transport(reqwest::Client::new());
        let client = factory
            .get(&backend.uri(), ClientInfo::default(), None)
            .await
            .unwrap();
        client.with_token("owned-token".into()).await;
        client.logout().await.unwrap();
        assert_eq!(client.get_token().await, None);
        backend.verify().await;
    }
}
