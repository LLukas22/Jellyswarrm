//! Shared application state and construction of workflow dependency views.
use crate::syncplay::SyncPlayService;
use crate::{application, sessions, upstream};
use crate::{
    application::playback::sessions::SessionStorage,
    auth::quick_connect::QuickConnectStorage,
    auth::{encryption::Password, federation::FederatedUserService, UserAuthorizationService},
    config::{AppConfig, MediaStreamingMode},
    libraries::VirtualLibraryService,
    media::MediaStorageService,
    processors::{
        response_processor::{ResponseProcessingContext, ResponseProcessingProfile},
        ProxyProcessors,
    },
    servers::{Server, ServerStorageService},
};
use std::sync::Arc;
#[derive(Clone)]
pub struct AppState {
    pub reqwest_client: reqwest::Client,
    pub streaming_reqwest_client: reqwest::Client,
    pub user_authorization: Arc<UserAuthorizationService>,
    pub server_storage: Arc<ServerStorageService>,
    pub media_storage: Arc<MediaStorageService>,
    pub virtual_library_service: Arc<VirtualLibraryService>,
    pub play_sessions: Arc<SessionStorage>,
    pub config: Arc<tokio::sync::RwLock<AppConfig>>,
    configuration: crate::config::service::ConfigurationService,
    pub(crate) settings: crate::config::SettingsView,
    pub(crate) url_selection: application::url_selection::UrlSelection,
    pub processors: Arc<ProxyProcessors>,
    pub quick_connect: QuickConnectStorage,
    pub federated_users: Arc<FederatedUserService>,
    pub syncplay: Arc<SyncPlayService>,
    pub client_sessions: Arc<sessions::ClientSessionService>,
    pub(crate) catalog_snapshots: Arc<application::catalog::snapshots::CatalogSnapshots>,
}

impl AppState {
    pub(crate) fn media_access(&self) -> crate::sessions::access::MediaAccess<'_> {
        crate::sessions::access::MediaAccess::new(
            &self.user_authorization,
            &self.media_storage,
            &self.url_selection,
        )
    }
    pub(crate) fn syncplay_operations(&self) -> application::syncplay::SyncPlayOperations<'_> {
        application::syncplay::SyncPlayOperations::new(&self.syncplay, self.media_access())
    }
    pub(crate) async fn server_credentials(
        &self,
    ) -> crate::application::server_admin::ServerCredentials<'_> {
        crate::application::server_admin::ServerCredentials::new(
            &self.server_storage,
            self.reqwest_client.clone(),
            (&self.get_admin_password().await).into(),
        )
    }
    pub(crate) fn server_administration(
        &self,
    ) -> crate::application::server_admin::ServerAdministration<'_> {
        crate::application::server_admin::ServerAdministration::new(
            &self.server_storage,
            &self.play_sessions,
            &self.catalog_snapshots,
        )
    }
    pub(crate) async fn library_administration(
        &self,
    ) -> crate::application::library_admin::LibraryAdministration<'_> {
        let password = self.get_admin_password().await;
        crate::application::library_admin::LibraryAdministration::new(
            &self.server_storage,
            &self.user_authorization,
            &self.virtual_library_service,
            crate::auth::mapping_authentication::MappingAuthenticator::new(
                &self.user_authorization,
                self.reqwest_client.clone(),
                password.clone(),
            ),
            (&password).into(),
            self.reqwest_client.clone(),
        )
    }
    pub(crate) async fn mapping_authenticator(
        &self,
    ) -> crate::auth::mapping_authentication::MappingAuthenticator<'_> {
        crate::auth::mapping_authentication::MappingAuthenticator::new(
            &self.user_authorization,
            self.reqwest_client.clone(),
            self.get_admin_password().await,
        )
    }
    pub(crate) fn configuration_service(&self) -> crate::config::service::ConfigurationService {
        self.configuration.clone()
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
            upstream: std::sync::Arc::new(self.json_upstream()),
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
    pub(crate) fn json_upstream(&self) -> upstream::jellyfin::json::JellyfinJsonUpstream {
        upstream::jellyfin::json::JellyfinJsonUpstream {
            client: self.reqwest_client.clone(),
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
    pub fn new(
        reqwest_client: reqwest::Client,
        streaming_reqwest_client: reqwest::Client,
        data_context: DataContext,
        proxy_processors: ProxyProcessors,
        quick_connect: QuickConnectStorage,
    ) -> Self {
        // Create temporary state to initialize FederatedUserService
        // This is a bit circular but FederatedUserService needs parts of AppState
        // We can construct it manually here since we have all components
        let federated_users = Arc::new(FederatedUserService::new_from_components(
            data_context.server_storage.clone(),
            data_context.user_authorization.clone(),
            data_context.config.clone(),
        ));

        let transport = sessions::transport::ConnectionHub::default();
        Self {
            reqwest_client,
            url_selection: application::url_selection::UrlSelection::new(
                data_context.media_storage.clone(),
                data_context.server_storage.clone(),
                data_context.virtual_library_service.clone(),
            ),
            streaming_reqwest_client,
            user_authorization: data_context.user_authorization,
            server_storage: data_context.server_storage,
            media_storage: data_context.media_storage,
            virtual_library_service: data_context.virtual_library_service,
            play_sessions: data_context.play_sessions,
            settings: crate::config::SettingsView::new(data_context.config.clone()),
            configuration: crate::config::service::ConfigurationService::new(
                data_context.config.clone(),
            ),
            config: data_context.config,
            processors: Arc::new(proxy_processors),
            quick_connect,
            federated_users,
            syncplay: Arc::new(SyncPlayService::with_transport(transport.clone())),
            client_sessions: Arc::new(sessions::ClientSessionService::new(transport)),
            catalog_snapshots: Arc::new(
                application::catalog::snapshots::CatalogSnapshots::default(),
            ),
        }
    }

    pub async fn get_ui_route(&self) -> String {
        let config = self.config.read().await;
        if let Some(prefix) = &config.url_prefix {
            format!("{}/{}", prefix, config.ui_route)
        } else {
            config.ui_route.to_string()
        }
    }

    pub async fn get_url_prefix(&self) -> Option<String> {
        let config = self.config.read().await;
        config.url_prefix.as_ref().map(|prefix| prefix.to_string())
    }

    pub async fn get_admin_password(&self) -> Password {
        let config = self.config.read().await;
        config.password.clone()
    }

    pub async fn remove_prefix_from_path<'a>(&self, path: &'a str) -> &'a str {
        let config = self.config.read().await;
        if let Some(prefix) = &config.url_prefix {
            path.trim_start_matches(&format!("/{}", prefix))
        } else {
            path
        }
    }

    pub async fn get_media_streaming_mode(&self) -> MediaStreamingMode {
        let config = self.config.read().await;
        config.media_streaming_mode
    }

    pub async fn auto_create_users_on_login(&self) -> bool {
        let config = self.config.read().await;
        config.auto_create_users_on_login
    }

    pub async fn merge_libraries_enabled(&self) -> bool {
        self.config.read().await.merge_libraries
    }

    pub async fn deduplicate_media_enabled(&self) -> bool {
        self.config.read().await.deduplicate_media
    }

    /// Translate backend IDs, URLs and permissions without presentation or
    /// caching side effects. Handlers cache only their finalized response.
    pub(crate) async fn process_response_json_typed(
        &self,
        payload: &mut serde_json::Value,
        server: &Server,
        profile: ResponseProcessingProfile,
        proxy_api_key: Option<&str>,
    ) -> Result<bool, application::error::ApplicationError> {
        let config = self.config.read().await;
        let context = ResponseProcessingContext {
            server: server.clone(),
            proxy_server_id: config.server_id.clone(),
            proxy_api_key: proxy_api_key.map(str::to_string),
            profile,
        };
        drop(config);

        let modified = self
            .processors
            .process_response_json_typed(payload, &context)
            .await?;
        Ok(modified)
    }
}

impl From<crate::DataContext> for crate::processors::url_processor::UrlProcessor {
    fn from(context: crate::DataContext) -> Self {
        Self::new(
            context.media_storage,
            context.virtual_library_service,
            context.play_sessions,
            context.user_authorization,
            crate::config::SettingsView::new(context.config),
        )
    }
}

#[derive(Clone)]
/// Struct holding shared services and configuration
pub struct DataContext {
    pub user_authorization: Arc<UserAuthorizationService>,
    pub server_storage: Arc<ServerStorageService>,
    pub media_storage: Arc<MediaStorageService>,
    pub virtual_library_service: Arc<VirtualLibraryService>,
    pub play_sessions: Arc<SessionStorage>,
    pub config: Arc<tokio::sync::RwLock<AppConfig>>,
}
