//! Media entry-point handles. Borrowed workflow views live only for a request.
use super::paths::AdapterPaths;
use crate::{
    application, auth::UserAuthorizationService, config::SettingsView,
    libraries::VirtualLibraryService, media::MediaStorageService,
    playback::sessions::SessionStorage, processors::ProxyProcessors, servers::ServerStorageService,
    sessions::ClientSessionService, upstream::jellyfin::json::JellyfinJsonUpstream,
};
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct MediaState {
    pub(crate) reqwest_client: reqwest::Client,
    pub(crate) streaming_reqwest_client: reqwest::Client,
    pub(crate) user_authorization: Arc<UserAuthorizationService>,
    pub(crate) server_storage: Arc<ServerStorageService>,
    pub(crate) media_storage: Arc<MediaStorageService>,
    pub(crate) virtual_library_service: Arc<VirtualLibraryService>,
    pub(crate) play_sessions: Arc<SessionStorage>,
    pub(crate) settings: SettingsView,
    pub(crate) paths: AdapterPaths,
    pub(crate) url_selection: application::url_selection::UrlSelection,
    pub(crate) processors: Arc<ProxyProcessors>,
    pub(crate) client_sessions: Arc<ClientSessionService>,
    pub(crate) catalog_snapshots: Arc<application::catalog::snapshots::CatalogSnapshots>,
}
impl axum::extract::FromRef<MediaState> for application::catalog::services::CatalogService {
    fn from_ref(state: &MediaState) -> Self {
        state.catalog_service()
    }
}

impl MediaState {
    pub(crate) fn catalog_service(&self) -> application::catalog::services::CatalogService {
        application::catalog::services::CatalogService {
            media_storage: self.media_storage.clone(),
            server_storage: self.server_storage.clone(),
            virtual_library_service: self.virtual_library_service.clone(),
            play_sessions: self.play_sessions.clone(),
            config: self.settings.clone(),
            selection: self.url_selection.clone(),
            processors: application::catalog::services::CatalogProcessors {
                url_processor: self.processors.url_processor.clone(),
                response_processor: self.processors.response_processor.clone(),
            },
            upstream: Arc::new(self.json_upstream()),
            client_sessions: self.client_sessions.clone(),
            catalog_snapshots: self.catalog_snapshots.clone(),
        }
    }
    pub(crate) fn playback_service(&self) -> application::playback::services::PlaybackService<'_> {
        application::playback::services::PlaybackService {
            media_storage: &self.media_storage,
            server_storage: &self.server_storage,
            play_sessions: &self.play_sessions,
            config: &self.settings,
            selection: &self.url_selection,
            processors: application::playback::services::PlaybackProcessors {
                url_processor: &self.processors.url_processor,
                response_processor: &self.processors.response_processor,
            },
            upstream: Box::new(self.json_upstream()),
            streaming_reqwest_client: &self.streaming_reqwest_client,
            client_sessions: &self.client_sessions,
        }
    }
    pub(crate) fn proxy_service(&self) -> application::proxy::services::ProxyService<'_> {
        application::proxy::services::ProxyService {
            routing: self.routing_services(),
            media_storage: &self.media_storage,
            play_sessions: &self.play_sessions,
            settings: &self.settings,
            processors: &self.processors,
            reqwest_client: &self.reqwest_client,
            client_sessions: &self.client_sessions,
            catalog_snapshots: &self.catalog_snapshots,
        }
    }
    pub(crate) fn routing_services(&self) -> application::routing::RoutingServices<'_> {
        application::routing::RoutingServices {
            media_storage: &self.media_storage,
            server_storage: &self.server_storage,
            play_sessions: &self.play_sessions,
            urls: &self.processors.url_processor,
            config: &self.settings,
            selection: &self.url_selection,
        }
    }
    fn json_upstream(&self) -> JellyfinJsonUpstream {
        JellyfinJsonUpstream {
            client: self.reqwest_client.clone(),
        }
    }
}
