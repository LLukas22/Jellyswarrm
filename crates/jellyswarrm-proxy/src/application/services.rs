//! Media capabilities shared by catalog, playback, routing, and fallback forwarding.
//! Every handle is reference counted: workflows borrow `&MediaServices`, and
//! bounded source tasks clone it. Accounts, administration, SyncPlay, and Quick
//! Connect keep their own state and are not reachable from here.
use crate::{
    application::{
        catalog::snapshots::CatalogSnapshots,
        error::ApplicationError,
        request::{response, UpstreamRequestBuilder},
    },
    auth::UserAuthorizationService,
    config::SettingsView,
    libraries::VirtualLibraryService,
    media::MediaStorageService,
    playback::PlaybackRegistry,
    processors::{
        response_processor::{ResponseProcessingContext, ResponseProcessingProfile},
        ProxyProcessors,
    },
    servers::{Server, ServerStorageService},
    sessions::ClientSessionService,
};
use std::sync::Arc;

use super::request::selection::UrlSelection;

#[derive(Clone)]
pub(crate) struct MediaServices {
    pub(crate) reqwest_client: reqwest::Client,
    pub(crate) streaming_reqwest_client: reqwest::Client,
    pub(crate) user_authorization: Arc<UserAuthorizationService>,
    pub(crate) server_storage: Arc<ServerStorageService>,
    pub(crate) media_storage: Arc<MediaStorageService>,
    pub(crate) virtual_library_service: Arc<VirtualLibraryService>,
    pub(crate) playback_registry: Arc<PlaybackRegistry>,
    pub(crate) config: SettingsView,
    pub(crate) url_selection: UrlSelection,
    pub(crate) processors: Arc<ProxyProcessors>,
    pub(crate) client_sessions: Arc<ClientSessionService>,
    pub(crate) catalog_snapshots: Arc<CatalogSnapshots>,
    /// Fixed at startup, like the router mount it mirrors.
    pub(crate) url_prefix: Option<String>,
}

impl MediaServices {
    pub(crate) fn url_prefix(&self) -> Option<&str> {
        self.url_prefix.as_deref()
    }

    pub(crate) fn remove_prefix<'a>(&self, path: &'a str) -> &'a str {
        match &self.url_prefix {
            Some(prefix) => path.trim_start_matches(&format!("/{prefix}")),
            None => path,
        }
    }

    pub(crate) fn request_builder(&self) -> UpstreamRequestBuilder<'_> {
        UpstreamRequestBuilder::new(&self.processors.url_processor, self.url_prefix())
    }

    pub(crate) async fn deduplicate_media_enabled(&self) -> bool {
        self.config.read().await.deduplicate_media
    }

    pub(crate) async fn merge_libraries_enabled(&self) -> bool {
        self.config.read().await.merge_libraries
    }

    pub(crate) async fn process_response_json(
        &self,
        payload: &mut serde_json::Value,
        server: &Server,
        profile: ResponseProcessingProfile,
        token: Option<&str>,
    ) -> Result<bool, ApplicationError> {
        let context = ResponseProcessingContext {
            server: server.clone(),
            proxy_server_id: self.config.read().await.server_id.clone(),
            proxy_api_key: token.map(str::to_owned),
            profile,
        };
        self.processors
            .process_response_json(payload, &context)
            .await
            .map_err(Into::into)
    }

    pub(crate) async fn execute_processed_json(
        &self,
        request: reqwest::Request,
        server: &Server,
        profile: ResponseProcessingProfile,
        token: Option<&str>,
    ) -> Result<serde_json::Value, ApplicationError> {
        let mut payload = response::execute_json_request(&self.reqwest_client, request).await?;
        self.process_response_json(&mut payload, server, profile, token)
            .await?;
        Ok(payload)
    }
}
