//! Catalog-only capabilities, owned so bounded source tasks can share them.
use crate::{
    application::{error::ApplicationError, routing::RoutingServices, upstream::execute_payload},
    config::SettingsView,
    libraries::service::VirtualLibraryService,
    media::storage::MediaStorageService,
    playback::sessions::SessionStorage,
    processors::{
        response_processor::{ResponseProcessingProfile, ResponseProcessor},
        url_processor::UrlProcessor,
    },
    servers::storage::{Server, ServerStorageService},
    upstream::jellyfin::json::JsonUpstream,
};
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct CatalogProcessors {
    pub(crate) url_processor: UrlProcessor,
    pub(crate) response_processor: ResponseProcessor,
}

#[derive(Clone)]
pub(crate) struct CatalogService {
    pub(crate) media_storage: Arc<MediaStorageService>,
    pub(crate) server_storage: Arc<ServerStorageService>,
    pub(crate) virtual_library_service: Arc<VirtualLibraryService>,
    pub(crate) play_sessions: Arc<SessionStorage>,
    pub(crate) config: SettingsView,
    pub(crate) selection: crate::application::url_selection::UrlSelection,
    pub(crate) processors: CatalogProcessors,
    pub(crate) upstream: Arc<dyn JsonUpstream>,
    pub(crate) client_sessions: Arc<crate::sessions::ClientSessionService>,
    pub(crate) catalog_snapshots: Arc<super::snapshots::CatalogSnapshots>,
}

impl CatalogService {
    pub(crate) fn routing_services(&self) -> RoutingServices<'_> {
        RoutingServices {
            media_storage: &self.media_storage,
            server_storage: &self.server_storage,
            play_sessions: &self.play_sessions,
            config: &self.config,
            selection: &self.selection,
            urls: &self.processors.url_processor,
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
    pub(crate) async fn deduplicate_media_enabled(&self) -> bool {
        self.config.read().await.deduplicate_media
    }
    pub(crate) async fn merge_libraries_enabled(&self) -> bool {
        self.config.read().await.merge_libraries
    }
    pub(crate) async fn process_response_json_typed(
        &self,
        payload: &mut serde_json::Value,
        server: &Server,
        profile: ResponseProcessingProfile,
        token: Option<&str>,
    ) -> Result<bool, ApplicationError> {
        crate::application::response::project_media_json(
            &self.processors.response_processor,
            &self.config,
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
