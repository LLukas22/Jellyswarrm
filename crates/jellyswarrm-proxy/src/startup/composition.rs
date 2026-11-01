//! Construct services and the handler state from loaded configuration.
use crate::{
    application::{
        self,
        admin::{
            libraries::LibraryAdministration,
            servers::{ServerAdministration, ServerCredentials},
        },
        client_sessions::{access::MediaAccess, SessionOperations},
        syncplay::SyncPlayOperations,
    },
    auth::{
        federation::FederatedUserService, mapping_authentication::MappingAuthenticator,
        quick_connect::QuickConnectStorage, UserAuthorizationService,
    },
    client_sessions,
    config::{AppConfig, Paths, SettingsView},
    http::state::{AdapterPaths, AppState, ManagementState, SettingsState},
    libraries::VirtualLibraryService,
    media::MediaStorageService,
    playback::PlaybackRegistry,
    servers::ServerStorageService,
    syncplay::SyncPlayService,
    translation::{
        request_analyzer::RequestAnalyzer, request_processor::RequestProcessor,
        response_processor::ResponseProcessor, url_processor::UrlProcessor, ProxyProcessors,
    },
};
use std::sync::Arc;

pub(super) struct HttpClients {
    pub(super) api: reqwest::Client,
    pub(super) streaming: reqwest::Client,
}

pub(super) fn adapter_state(
    clients: HttpClients,
    config: AppConfig,
    paths: Paths,
    user_authorization: Arc<UserAuthorizationService>,
    server_storage: Arc<ServerStorageService>,
    media_storage: Arc<MediaStorageService>,
    virtual_library_service: Arc<VirtualLibraryService>,
) -> AppState {
    let credentials = config.admin_credentials();
    let adapter_paths = AdapterPaths::new(&config.startup_config());
    let url_prefix = adapter_paths.url_prefix();
    let config = Arc::new(tokio::sync::RwLock::new(config));
    let settings = SettingsView::new(config.clone());
    let configuration = crate::config::service::ConfigurationService::new(config, paths);
    let playback_registry = Arc::new(PlaybackRegistry::new());
    let urls = UrlProcessor::new(
        media_storage.clone(),
        virtual_library_service.clone(),
        playback_registry.clone(),
        user_authorization.clone(),
        settings.clone(),
    );
    let translation = Arc::new(ProxyProcessors::new(
        RequestProcessor::new(media_storage.clone(), user_authorization.clone()),
        RequestAnalyzer::new(
            media_storage.clone(),
            server_storage.clone(),
            playback_registry.clone(),
            user_authorization.clone(),
        ),
        ResponseProcessor::new(media_storage.clone(), user_authorization.clone()),
        urls,
    ));
    let selection = application::request::selection::UrlSelection::new(
        media_storage.clone(),
        server_storage.clone(),
        virtual_library_service.clone(),
    );
    let connections = client_sessions::connections::ConnectionHub::default();
    let client_sessions = Arc::new(client_sessions::ClientSessionService::new(
        connections.clone(),
    ));
    let syncplay = Arc::new(SyncPlayService::with_connections(connections));
    let quick_connect = QuickConnectStorage::new();
    let catalog_snapshots = Arc::new(application::catalog::snapshots::CatalogSnapshots::default());
    let media_access = MediaAccess::new(
        user_authorization.clone(),
        media_storage.clone(),
        selection.clone(),
    );
    let session_operations = SessionOperations::new(
        user_authorization.clone(),
        client_sessions.clone(),
        syncplay.clone(),
        settings.clone(),
        media_access.clone(),
    );
    let syncplay_operations = SyncPlayOperations::new(syncplay, media_access);
    let mapping_authenticator = MappingAuthenticator::new(
        user_authorization.clone(),
        clients.api.clone(),
        credentials.password(),
    );
    let authentication = application::accounts::AuthenticationService {
        user_authorization: user_authorization.clone(),
        server_storage: server_storage.clone(),
        settings: settings.clone(),
        client_sessions: client_sessions.clone(),
        mapping_authenticator: mapping_authenticator.clone(),
    };
    let management = ManagementState {
        reqwest_client: clients.api.clone(),
        user_authorization: user_authorization.clone(),
        server_storage: server_storage.clone(),
        virtual_library_service: virtual_library_service.clone(),
        federated_users: Arc::new(FederatedUserService::new(
            server_storage.clone(),
            user_authorization.clone(),
            credentials.clone(),
        )),
        quick_connect: quick_connect.clone(),
        settings: settings.clone(),
        configuration: configuration.clone(),
        paths: adapter_paths.clone(),
        sessions: session_operations.clone(),
        server_credentials: ServerCredentials::new(
            server_storage.clone(),
            clients.api.clone(),
            (&credentials.password()).into(),
        ),
        server_administration: ServerAdministration::new(
            server_storage.clone(),
            playback_registry.clone(),
            catalog_snapshots.clone(),
        ),
        library_administration: LibraryAdministration::new(
            server_storage.clone(),
            user_authorization.clone(),
            virtual_library_service.clone(),
            mapping_authenticator.clone(),
            (&credentials.password()).into(),
            clients.api.clone(),
        ),
        mapping_authenticator,
        credentials,
    };
    let settings_state = SettingsState {
        settings: settings.clone(),
        configuration,
        paths: adapter_paths,
    };
    let media = application::media_services::MediaServices {
        reqwest_client: clients.api,
        streaming_reqwest_client: clients.streaming,
        user_authorization,
        server_storage,
        media_storage,
        virtual_library_service,
        playback_registry,
        config: settings,
        url_selection: selection,
        translation,
        client_sessions,
        catalog_snapshots,
        url_prefix,
    };
    AppState::new(
        media,
        session_operations,
        syncplay_operations,
        authentication,
        management,
        settings_state,
        quick_connect,
    )
}
