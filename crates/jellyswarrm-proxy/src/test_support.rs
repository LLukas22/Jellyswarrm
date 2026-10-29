//! Test-only, in-memory media capabilities. Workflow tests need neither a router
//! nor AppState; HTTP fixtures explicitly opt into adapter construction.
use std::sync::Arc;
pub(crate) mod adapters;
use crate::{
    application,
    auth::UserAuthorizationService,
    config::{AppConfig, SettingsView},
    libraries::VirtualLibraryService,
    media::MediaStorageService,
    playback::sessions::SessionStorage,
    processors::{
        request_analyzer::RequestAnalyzer, request_processor::RequestProcessor,
        response_processor::ResponseProcessor, url_processor::UrlProcessor, ProxyProcessors,
    },
    servers::ServerStorageService,
    sessions::{transport::ConnectionHub, ClientSessionService},
    syncplay::SyncPlayService,
};

#[derive(Clone)]
pub(crate) struct MediaFixture {
    reqwest_client: reqwest::Client,
    streaming_reqwest_client: reqwest::Client,
    pub(crate) user_authorization: Arc<UserAuthorizationService>,
    pub(crate) server_storage: Arc<ServerStorageService>,
    pub(crate) media_storage: Arc<MediaStorageService>,
    pub(crate) virtual_library_service: Arc<VirtualLibraryService>,
    pub(crate) play_sessions: Arc<SessionStorage>,
    pub(crate) config: Arc<tokio::sync::RwLock<AppConfig>>,
    pub(crate) settings: SettingsView,
    pub(crate) url_selection: application::url_selection::UrlSelection,
    pub(crate) processors: Arc<ProxyProcessors>,
    pub(crate) client_sessions: Arc<ClientSessionService>,
    pub(crate) catalog_snapshots: Arc<application::catalog::snapshots::CatalogSnapshots>,
}
impl MediaFixture {
    pub(crate) fn new(pool: sqlx::SqlitePool, config: AppConfig) -> Self {
        Self::with_authorization(pool.clone(), config, UserAuthorizationService::new(pool))
    }
    pub(crate) fn with_authorization(
        pool: sqlx::SqlitePool,
        config: AppConfig,
        authorization: UserAuthorizationService,
    ) -> Self {
        let server_storage = Arc::new(ServerStorageService::new(pool.clone()));
        let media_storage = Arc::new(MediaStorageService::new(pool.clone()));
        let virtual_library_service = Arc::new(VirtualLibraryService::new(
            pool,
            (*server_storage).clone(),
            (*media_storage).clone(),
        ));
        let user_authorization = Arc::new(authorization);
        let play_sessions = Arc::new(SessionStorage::new());
        let config = Arc::new(tokio::sync::RwLock::new(config));
        let settings = SettingsView::new(config.clone());
        let processors = Arc::new(ProxyProcessors::new(
            RequestProcessor::new(media_storage.clone(), user_authorization.clone()),
            RequestAnalyzer::new(
                media_storage.clone(),
                server_storage.clone(),
                play_sessions.clone(),
                user_authorization.clone(),
            ),
            ResponseProcessor::new(media_storage.clone(), user_authorization.clone()),
            UrlProcessor::new(
                media_storage.clone(),
                virtual_library_service.clone(),
                play_sessions.clone(),
                user_authorization.clone(),
                settings.clone(),
            ),
        ));
        let url_selection = application::url_selection::UrlSelection::new(
            media_storage.clone(),
            server_storage.clone(),
            virtual_library_service.clone(),
        );
        Self {
            reqwest_client: reqwest::Client::new(),
            streaming_reqwest_client: reqwest::Client::new(),
            user_authorization,
            server_storage,
            media_storage,
            virtual_library_service,
            play_sessions,
            config,
            settings,
            url_selection,
            processors,
            client_sessions: Arc::new(ClientSessionService::new(ConnectionHub::default())),
            catalog_snapshots: Arc::new(
                application::catalog::snapshots::CatalogSnapshots::default(),
            ),
        }
    }
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
            upstream: Arc::new(crate::upstream::jellyfin::json::JellyfinJsonUpstream {
                client: self.reqwest_client.clone(),
            }),
            client_sessions: self.client_sessions.clone(),
            catalog_snapshots: self.catalog_snapshots.clone(),
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
            upstream: Box::new(crate::upstream::jellyfin::json::JellyfinJsonUpstream {
                client: self.reqwest_client.clone(),
            }),
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
    pub(crate) async fn authentication_service(
        &self,
    ) -> application::accounts::services::AuthenticationService {
        application::accounts::services::AuthenticationService {
            user_authorization: self.user_authorization.clone(),
            server_storage: self.server_storage.clone(),
            settings: self.settings.clone(),
            reqwest_client: self.reqwest_client.clone(),
            client_sessions: self.client_sessions.clone(),
            admin_password: self.config.read().await.password.clone(),
        }
    }
}
