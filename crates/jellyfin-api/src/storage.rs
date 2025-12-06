use std::sync::Arc;

use crate::{error::Error, ClientInfo, JellyfinClient};
use moka::future::Cache;
use url::Url;

#[derive(Clone)]
pub struct JellyfinClientStorage {
    cache: Cache<(String, ClientInfo, String), Arc<JellyfinClient>>,
}

impl JellyfinClientStorage {
    pub fn new(capacity: u64, ttl: std::time::Duration) -> Self {
        let cache = Cache::builder()
            .max_capacity(capacity)
            .time_to_idle(ttl)
            .eviction_listener(|_key, value: Arc<JellyfinClient>, _cause| {
                if value.get_token().is_some() {
                    tokio::spawn(async move {
                        if let Err(e) = value.logout().await {
                            tracing::error!("Failed to logout evicted client: {:?}", e);
                        }
                    });
                }
            })
            .build();

        Self { cache }
    }

    pub async fn get(
        &self,
        base_url: &str,
        client_info: ClientInfo,
        id: Option<&str>,
    ) -> Result<Arc<JellyfinClient>, Error> {
        let mut url = Url::parse(base_url)?;
        if url.path().ends_with('/') {
            url.path_segments_mut()
                .map_err(|_| Error::UrlParse(url::ParseError::EmptyHost))?
                .pop_if_empty();
        }
        let normalized_url = url.to_string();
        let id = id.unwrap_or_default().to_string();
        let key = (normalized_url.clone(), client_info.clone(), id);

        if let Some(client) = self.cache.get(&key).await {
            return Ok(client);
        }

        let client = Arc::new(JellyfinClient::new(&normalized_url, client_info)?);
        self.cache.insert(key, client.clone()).await;

        Ok(client)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_client_eviction_logout() {
        let mock_server = MockServer::start().await;

        // Mock the logout endpoint
        Mock::given(method("POST"))
            .and(path("/Sessions/Logout"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1) // Expect exactly one call
            .mount(&mock_server)
            .await;

        // Create storage with capacity 1
        let storage = JellyfinClientStorage::new(1, Duration::from_secs(60));
        let client_info = ClientInfo::default();

        // 1. Get first client
        let client1 = storage
            .get(&mock_server.uri(), client_info.clone(), None)
            .await
            .unwrap();

        // Simulate authentication (this updates the shared Arc<RwLock>)
        let _ = client1.with_token("test_token".to_string());

        // 2. Manually invalidate the client to force eviction
        // We need to reconstruct the key used in storage
        let mut url = Url::parse(&mock_server.uri()).unwrap();
        if url.path().ends_with('/') {
            url.path_segments_mut().unwrap().pop_if_empty();
        }
        let normalized_url = url.to_string();
        let key = (normalized_url, client_info, "".to_string());

        storage.cache.invalidate(&key).await;

        // Force maintenance to ensure eviction happens (invalidate might be lazy or listener might be async)
        storage.cache.run_pending_tasks().await;

        // We need to wait for the background task to complete.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // The expectation on the Mock will verify that the request was received.
    }

    #[tokio::test]
    async fn test_client_ttl_eviction() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/Sessions/Logout"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&mock_server)
            .await;

        // Create storage with short TTL
        let storage = JellyfinClientStorage::new(10, Duration::from_millis(100));
        let client_info = ClientInfo::default();

        let client = storage
            .get(&mock_server.uri(), client_info, None)
            .await
            .unwrap();

        let _ = client.with_token("test_token".to_string());

        // Wait for TTL to expire + some buffer
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Trigger maintenance/eviction check
        storage.cache.run_pending_tasks().await;

        // Wait for the eviction listener to run
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
