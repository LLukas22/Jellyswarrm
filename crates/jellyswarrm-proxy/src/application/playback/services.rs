//! Explicit playback capabilities. No authentication/account services, catalog
//! caches, SyncPlay, Quick Connect, or catch-all application state are available.
use crate::{
    application::playback::sessions::SessionStorage,
    application::{
        error::ApplicationError,
        routing::RoutingServices,
        upstream::{execute_payload, JsonUpstream},
    },
    config::AppConfig,
    media::storage::MediaStorageService,
    processors::{
        response_processor::{ResponseProcessingProfile, ResponseProcessor},
        url_processor::UrlProcessor,
    },
    servers::storage::{Server, ServerStorageService},
};

pub(crate) struct PlaybackProcessors<'a> {
    pub url_processor: &'a UrlProcessor,
    pub response_processor: &'a ResponseProcessor,
}

pub(crate) struct PlaybackService<'a> {
    pub media_storage: &'a MediaStorageService,
    pub server_storage: &'a ServerStorageService,
    pub play_sessions: &'a SessionStorage,
    pub config: &'a tokio::sync::RwLock<AppConfig>,
    pub processors: PlaybackProcessors<'a>,
    pub upstream: Box<dyn JsonUpstream + 'a>,
    pub streaming_reqwest_client: &'a reqwest::Client,
    pub client_sessions: &'a crate::sessions::ClientSessionService,
}

impl PlaybackService<'_> {
    pub fn routing_services(&self) -> RoutingServices<'_> {
        RoutingServices {
            media_storage: self.media_storage,
            server_storage: self.server_storage,
            play_sessions: self.play_sessions,
            config: self.config,
            urls: self.processors.url_processor,
        }
    }
    pub async fn get_url_prefix(&self) -> Option<String> {
        self.config
            .read()
            .await
            .url_prefix
            .as_ref()
            .map(ToString::to_string)
    }

    pub async fn process_response_json_typed(
        &self,
        payload: &mut serde_json::Value,
        server: &Server,
        profile: ResponseProcessingProfile,
        token: Option<&str>,
    ) -> Result<bool, ApplicationError> {
        crate::application::response::project_media_json(
            self.processors.response_processor,
            self.config,
            payload,
            server,
            profile,
            token,
        )
        .await
    }

    pub async fn execute_processed_json(
        &self,
        request: reqwest::Request,
        server: &Server,
        profile: ResponseProcessingProfile,
        token: Option<&str>,
    ) -> Result<serde_json::Value, ApplicationError> {
        let mut payload = execute_payload(self.upstream.as_ref(), request).await?;
        self.process_response_json_typed(&mut payload, server, profile, token)
            .await?;
        Ok(payload)
    }
}
