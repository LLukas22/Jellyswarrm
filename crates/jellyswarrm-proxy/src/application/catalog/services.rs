//! Catalog-only capabilities, owned so bounded source tasks can share them.
use crate::{
    application::{
        error::ApplicationError,
        routing::RoutingServices,
        upstream::{execute_payload, JsonUpstream},
    },
    config::AppConfig,
    media_storage_service::MediaStorageService,
    processors::{
        response_processor::{ResponseProcessingProfile, ResponseProcessor},
        url_processor::UrlProcessor,
    },
    server_storage::{Server, ServerStorageService},
    session_storage::SessionStorage,
    virtual_library_service::VirtualLibraryService,
};
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct CatalogProcessors {
    pub url_processor: UrlProcessor,
    pub response_processor: ResponseProcessor,
}

#[derive(Clone)]
pub(crate) struct CatalogService {
    pub media_storage: Arc<MediaStorageService>,
    pub server_storage: Arc<ServerStorageService>,
    pub virtual_library_service: Arc<VirtualLibraryService>,
    pub play_sessions: Arc<SessionStorage>,
    pub config: Arc<tokio::sync::RwLock<AppConfig>>,
    pub processors: CatalogProcessors,
    pub upstream: Arc<dyn JsonUpstream>,
    pub client_sessions: Arc<crate::sessions::ClientSessionService>,
    pub catalog_snapshots: Arc<super::snapshots::CatalogSnapshots>,
}

impl CatalogService {
    pub fn routing_services(&self) -> RoutingServices<'_> {
        RoutingServices {
            media_storage: &self.media_storage,
            server_storage: &self.server_storage,
            play_sessions: &self.play_sessions,
            config: &self.config,
            urls: &self.processors.url_processor,
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
    pub async fn deduplicate_media_enabled(&self) -> bool {
        self.config.read().await.deduplicate_media
    }
    pub async fn merge_libraries_enabled(&self) -> bool {
        self.config.read().await.merge_libraries
    }
    pub async fn process_response_json_typed(
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
