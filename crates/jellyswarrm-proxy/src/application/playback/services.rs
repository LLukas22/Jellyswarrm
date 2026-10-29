//! Explicit playback capabilities. No authentication/account services, catalog
//! caches, SyncPlay, Quick Connect, or catch-all application state are available.
use crate::{
    application::{error::ApplicationError, routing::RoutingServices, upstream::execute_payload},
    config::SettingsView,
    media::storage::MediaStorageService,
    playback::sessions::SessionStorage,
    processors::{
        response_processor::{ResponseProcessingProfile, ResponseProcessor},
        url_processor::UrlProcessor,
    },
    servers::storage::{Server, ServerStorageService},
    upstream::jellyfin::json::JsonUpstream,
};

pub(crate) struct PlaybackProcessors<'a> {
    pub(crate) url_processor: &'a UrlProcessor,
    pub(crate) response_processor: &'a ResponseProcessor,
}

pub(crate) struct PlaybackService<'a> {
    pub(crate) media_storage: &'a MediaStorageService,
    pub(crate) server_storage: &'a ServerStorageService,
    pub(crate) play_sessions: &'a SessionStorage,
    pub(crate) config: &'a SettingsView,
    pub(crate) selection: &'a crate::application::url_selection::UrlSelection,
    pub(crate) processors: PlaybackProcessors<'a>,
    pub(crate) upstream: Box<dyn JsonUpstream + 'a>,
    pub(crate) streaming_reqwest_client: &'a reqwest::Client,
    pub(crate) client_sessions: &'a crate::sessions::ClientSessionService,
}

impl PlaybackService<'_> {
    pub(crate) fn routing_services(&self) -> RoutingServices<'_> {
        RoutingServices {
            media_storage: self.media_storage,
            server_storage: self.server_storage,
            play_sessions: self.play_sessions,
            config: self.config,
            selection: self.selection,
            urls: self.processors.url_processor,
        }
    }
    pub(crate) async fn get_url_prefix(&self) -> Option<String> {
        self.config
            .read()
            .await
            .url_prefix
            .as_ref()
            .map(ToString::to_string)
    }

    pub(crate) async fn process_response_json_typed(
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

    pub(crate) async fn execute_processed_json(
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
