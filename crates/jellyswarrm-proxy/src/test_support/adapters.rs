//! Opt-in HTTP fixture wiring; core media tests do not construct these handles.
use super::*;
use crate::{
    application::admin::{
        libraries::LibraryAdministration,
        servers::{ServerAdministration, ServerCredentials},
    },
    auth::{
        federation::FederatedUserService, mapping_authentication::MappingAuthenticator,
        quick_connect::QuickConnectStorage,
    },
    http::state::{AdapterPaths, AppState, ManagementState, SettingsState},
};

#[derive(Clone)]
pub(crate) struct AdapterFixture {
    media: MediaFixture,
    pub(crate) sessions: application::sessions::SessionOperations,
    syncplay_operations: application::syncplay::SyncPlayOperations,
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
        let syncplay = Arc::new(SyncPlayService::with_connections(
            media.client_sessions.connections.clone(),
        ));
        let access = application::sessions::access::MediaAccess::new(
            media.user_authorization.clone(),
            media.media_storage.clone(),
            media.url_selection.clone(),
        );
        let sessions = application::sessions::SessionOperations::new(
            media.user_authorization.clone(),
            media.client_sessions.clone(),
            syncplay.clone(),
            media.services.config.clone(),
            access.clone(),
        );
        let syncplay_operations =
            application::syncplay::SyncPlayOperations::new(syncplay.clone(), access);
        let mapping_authenticator = MappingAuthenticator::new(
            media.user_authorization.clone(),
            client.clone(),
            credentials.password(),
        );
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
            federated_users: Arc::new(FederatedUserService::new(
                media.server_storage.clone(),
                media.user_authorization.clone(),
                credentials.clone(),
            )),
            quick_connect: quick_connect.clone(),
            settings: media.services.config.clone(),
            configuration,
            paths,
            sessions: sessions.clone(),
            server_credentials: ServerCredentials::new(
                media.server_storage.clone(),
                client.clone(),
                (&credentials.password()).into(),
            ),
            server_administration: ServerAdministration::new(
                media.server_storage.clone(),
                media.playback_registry.clone(),
                media.catalog_snapshots.clone(),
            ),
            library_administration: LibraryAdministration::new(
                media.server_storage.clone(),
                media.user_authorization.clone(),
                media.virtual_library_service.clone(),
                mapping_authenticator.clone(),
                (&credentials.password()).into(),
                client.clone(),
            ),
            mapping_authenticator,
            credentials,
        };
        Self {
            media,
            sessions,
            syncplay_operations,
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
            self.syncplay_operations.clone(),
            self.authentication_service().await,
            self.management.clone(),
            settings,
            self.quick_connect.clone(),
        )
    }
}
