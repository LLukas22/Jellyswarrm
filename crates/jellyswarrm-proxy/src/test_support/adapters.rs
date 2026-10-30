//! Opt-in HTTP fixture wiring; core media tests do not construct these handles.
use super::*;
use crate::{
    auth::{federation::FederatedUserService, quick_connect::QuickConnectStorage},
    http::state::{AdapterPaths, AppState, ManagementState, SessionState, SettingsState},
};

#[derive(Clone)]
pub(crate) struct AdapterFixture {
    media: MediaFixture,
    pub(crate) sessions: SessionState,
    pub(crate) management: ManagementState,
    pub(crate) quick_connect: QuickConnectStorage,
    pub(crate) syncplay: Arc<SyncPlayService>,
    pub(crate) reqwest_client: reqwest::Client,
}
impl std::ops::Deref for AdapterFixture {
    type Target = MediaFixture;
    fn deref(&self) -> &MediaFixture {
        &self.media
    }
}
impl AdapterFixture {
    pub(crate) async fn new(pool: sqlx::SqlitePool, config: AppConfig) -> Self {
        Self::from_media(MediaFixture::new(pool, config)).await
    }
    pub(crate) async fn from_media(media: MediaFixture) -> Self {
        let config = media.config.read().await;
        let paths = AdapterPaths::new(&config.startup_config());
        let credentials = config.admin_credentials();
        drop(config);
        let client = reqwest::Client::new();
        let syncplay = Arc::new(SyncPlayService::with_transport(
            media.client_sessions.transport.clone(),
        ));
        let sessions = SessionState {
            user_authorization: media.user_authorization.clone(),
            media_storage: media.media_storage.clone(),
            url_selection: media.url_selection.clone(),
            settings: media.services.config.clone(),
            client_sessions: media.client_sessions.clone(),
            syncplay: syncplay.clone(),
        };
        let quick_connect = QuickConnectStorage::new();
        let configuration = crate::config::service::ConfigurationService::new(
            media.config.clone(),
            crate::config::Paths::temporary(),
        );
        let management = ManagementState {
            reqwest_client: client.clone(),
            user_authorization: media.user_authorization.clone(),
            server_storage: media.server_storage.clone(),
            virtual_library_service: media.virtual_library_service.clone(),
            play_sessions: media.play_sessions.clone(),
            catalog_snapshots: media.catalog_snapshots.clone(),
            federated_users: Arc::new(FederatedUserService::new(
                media.server_storage.clone(),
                media.user_authorization.clone(),
                credentials.clone(),
            )),
            quick_connect: quick_connect.clone(),
            credentials,
            settings: media.services.config.clone(),
            configuration,
            paths,
            sessions: sessions.clone(),
        };
        Self {
            media,
            sessions,
            management,
            quick_connect,
            syncplay,
            reqwest_client: client,
        }
    }
    pub(crate) async fn app_state(&self) -> AppState {
        let settings = SettingsState {
            settings: self.services.config.clone(),
            configuration: self.management.configuration.clone(),
            paths: self.management.paths.clone(),
        };
        AppState::new(
            self.services.clone(),
            self.sessions.clone(),
            self.authentication_service().await,
            self.management.clone(),
            settings,
            self.quick_connect.clone(),
        )
    }
    pub(crate) fn session_operations(&self) -> application::sessions::SessionOperations<'_> {
        self.sessions.session_operations()
    }
}
