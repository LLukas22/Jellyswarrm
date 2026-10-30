//! Test-only, in-memory media capabilities. Workflow tests need neither a router
//! nor AppState; HTTP fixtures explicitly opt into adapter construction.
use std::sync::Arc;
pub(crate) mod adapters;
use crate::{
    application::{self, services::MediaServices},
    auth::UserAuthorizationService,
    config::{AppConfig, SettingsView},
    domain::server_id::ServerId,
    libraries::VirtualLibraryService,
    media::MediaStorageService,
    playback::sessions::{
        LiveStreamBinding, PreparedLiveStream, RevisionedPlaybackSession, SessionStorage,
    },
    processors::{
        request_analyzer::RequestAnalyzer, request_processor::RequestProcessor,
        response_processor::ResponseProcessor, url_processor::UrlProcessor, ProxyProcessors,
    },
    servers::ServerStorageService,
    sessions::{transport::ConnectionHub, ClientSessionService},
    syncplay::SyncPlayService,
};

/// Media services plus the writable configuration behind `services.config`.
#[derive(Clone)]
pub(crate) struct MediaFixture {
    pub(crate) services: MediaServices,
    pub(crate) config: Arc<tokio::sync::RwLock<AppConfig>>,
}
impl std::ops::Deref for MediaFixture {
    type Target = MediaServices;
    fn deref(&self) -> &MediaServices {
        &self.services
    }
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
        let url_prefix = config.url_prefix.as_ref().map(ToString::to_string);
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
        let services = MediaServices {
            reqwest_client: reqwest::Client::new(),
            streaming_reqwest_client: reqwest::Client::new(),
            user_authorization,
            server_storage,
            media_storage,
            virtual_library_service,
            play_sessions,
            config: settings,
            url_selection,
            processors,
            client_sessions: Arc::new(ClientSessionService::new(ConnectionHub::default())),
            catalog_snapshots: Arc::new(
                application::catalog::snapshots::CatalogSnapshots::default(),
            ),
            url_prefix,
        };
        Self { services, config }
    }
    pub(crate) async fn authentication_service(
        &self,
    ) -> application::accounts::services::AuthenticationService {
        application::accounts::services::AuthenticationService {
            user_authorization: self.user_authorization.clone(),
            server_storage: self.server_storage.clone(),
            settings: self.services.config.clone(),
            reqwest_client: self.reqwest_client.clone(),
            client_sessions: self.client_sessions.clone(),
            admin_password: self.config.read().await.password.clone(),
        }
    }
}

/// Wire-string conveniences for historical fixtures. Production APIs require
/// distinct ID namespaces, especially for session and viewer arguments.
impl SessionStorage {
    pub(crate) async fn register_live_stream(
        &self,
        original: &str,
        viewer: &str,
        server: ServerId,
    ) -> String {
        self.register_live_binding(&original.into(), &viewer.into(), server)
            .await
            .as_str()
            .to_owned()
    }
    pub(crate) async fn prepare_live_stream(
        &self,
        original: &str,
        viewer: &str,
        server: ServerId,
    ) -> Arc<PreparedLiveStream> {
        self.prepare_live_binding(&original.into(), &viewer.into(), server)
            .await
    }
    pub(crate) async fn live_stream(&self, id: &str, viewer: &str) -> Option<LiveStreamBinding> {
        self.live_binding(&id.into(), &viewer.into()).await
    }
    pub(crate) async fn remove_live_stream(&self, id: &str, viewer: &str) {
        self.close_live_binding(&id.into(), &viewer.into()).await;
    }
    pub(crate) async fn resolve_report_session(
        &self,
        id: &str,
        viewer: &str,
    ) -> anyhow::Result<Option<RevisionedPlaybackSession>> {
        self.resolve_report_binding(&id.into(), &viewer.into())
            .await
    }
}
