//! Explicit startup wiring. Adapter handles own clones, never writable config.
use crate::{
    application,
    auth::{
        federation::FederatedUserService, quick_connect::QuickConnectStorage,
        UserAuthorizationService,
    },
    config::{AppConfig, Paths, SettingsView},
    http::state::{AdapterPaths, AppState, ManagementState, SessionState, SettingsState},
    libraries::VirtualLibraryService,
    media::MediaStorageService,
    playback::sessions::SessionStorage,
    processors::{
        request_analyzer::RequestAnalyzer, request_processor::RequestProcessor,
        response_processor::ResponseProcessor, url_processor::UrlProcessor, ProxyProcessors,
    },
    servers::ServerStorageService,
    sessions,
    syncplay::SyncPlayService,
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
    let play_sessions = Arc::new(SessionStorage::new());
    let urls = UrlProcessor::new(
        media_storage.clone(),
        virtual_library_service.clone(),
        play_sessions.clone(),
        user_authorization.clone(),
        settings.clone(),
    );
    let processors = Arc::new(ProxyProcessors::new(
        RequestProcessor::new(media_storage.clone(), user_authorization.clone()),
        RequestAnalyzer::new(
            media_storage.clone(),
            server_storage.clone(),
            play_sessions.clone(),
            user_authorization.clone(),
        ),
        ResponseProcessor::new(media_storage.clone(), user_authorization.clone()),
        urls,
    ));
    let selection = application::url_selection::UrlSelection::new(
        media_storage.clone(),
        server_storage.clone(),
        virtual_library_service.clone(),
    );
    let transport = sessions::transport::ConnectionHub::default();
    let client_sessions = Arc::new(sessions::ClientSessionService::new(transport.clone()));
    let syncplay = Arc::new(SyncPlayService::with_transport(transport));
    let quick_connect = QuickConnectStorage::new();
    let catalog_snapshots = Arc::new(application::catalog::snapshots::CatalogSnapshots::default());
    let session_state = SessionState {
        user_authorization: user_authorization.clone(),
        media_storage: media_storage.clone(),
        url_selection: selection.clone(),
        settings: settings.clone(),
        client_sessions: client_sessions.clone(),
        syncplay,
    };
    let authentication = application::accounts::services::AuthenticationService {
        user_authorization: user_authorization.clone(),
        server_storage: server_storage.clone(),
        settings: settings.clone(),
        reqwest_client: clients.api.clone(),
        client_sessions: client_sessions.clone(),
        admin_password: credentials.password(),
    };
    let management = ManagementState {
        reqwest_client: clients.api.clone(),
        user_authorization: user_authorization.clone(),
        server_storage: server_storage.clone(),
        virtual_library_service: virtual_library_service.clone(),
        play_sessions: play_sessions.clone(),
        catalog_snapshots: catalog_snapshots.clone(),
        federated_users: Arc::new(FederatedUserService::new(
            server_storage.clone(),
            user_authorization.clone(),
            credentials.clone(),
        )),
        quick_connect: quick_connect.clone(),
        credentials,
        settings: settings.clone(),
        configuration: configuration.clone(),
        paths: adapter_paths.clone(),
        sessions: session_state.clone(),
    };
    let settings_state = SettingsState {
        settings: settings.clone(),
        configuration,
        paths: adapter_paths,
    };
    let media = application::services::MediaServices {
        reqwest_client: clients.api,
        streaming_reqwest_client: clients.streaming,
        user_authorization,
        server_storage,
        media_storage,
        virtual_library_service,
        play_sessions,
        config: settings,
        url_selection: selection,
        processors,
        client_sessions,
        catalog_snapshots,
        url_prefix,
    };
    AppState::new(
        media,
        session_state,
        authentication,
        management,
        settings_state,
        quick_connect,
    )
}
