//! Fallback forwarding capabilities; no mutable configuration or administration.
use crate::{
    application::{error::ApplicationError, routing::RoutingServices},
    config::SettingsView,
    media::MediaStorageService,
    playback::sessions::SessionStorage,
    processors::{response_processor::ResponseProcessingProfile, ProxyProcessors},
    servers::Server,
    sessions::ClientSessionService,
};

pub(crate) struct ProxyService<'a> {
    pub routing: RoutingServices<'a>,
    pub media_storage: &'a MediaStorageService,
    pub play_sessions: &'a SessionStorage,
    pub settings: &'a SettingsView,
    pub processors: &'a ProxyProcessors,
    pub reqwest_client: &'a reqwest::Client,
    pub client_sessions: &'a ClientSessionService,
    pub catalog_snapshots: &'a crate::application::catalog::snapshots::CatalogSnapshots,
}

impl ProxyService<'_> {
    pub async fn get_url_prefix(&self) -> Option<String> {
        self.settings
            .read()
            .await
            .url_prefix
            .as_ref()
            .map(ToString::to_string)
    }

    pub async fn remove_prefix_from_path<'a>(&self, path: &'a str) -> &'a str {
        match self.get_url_prefix().await {
            Some(prefix) => path.trim_start_matches(&format!("/{prefix}")),
            None => path,
        }
    }

    pub async fn process_response_json_typed(
        &self,
        payload: &mut serde_json::Value,
        server: &Server,
        profile: ResponseProcessingProfile,
        token: Option<&str>,
    ) -> Result<bool, ApplicationError> {
        let context = crate::processors::response_processor::ResponseProcessingContext {
            server: server.clone(),
            proxy_server_id: self.settings.read().await.server_id.clone(),
            proxy_api_key: token.map(str::to_owned),
            profile,
        };
        self.processors
            .process_response_json_typed(payload, &context)
            .await
    }
}
