//! Shared application state and construction of workflow dependency views.
use crate::syncplay::SyncPlayService;
use crate::{application, sessions, upstream};
use crate::{
    application::playback::sessions::SessionStorage,
    auth::{encryption::Password, federation::FederatedUserService, UserAuthorizationService},
    config::{AppConfig, MediaStreamingMode},
    http::handlers::quick_connect::QuickConnectStorage,
    libraries::VirtualLibraryService,
    media::MediaStorageService,
    processors::{
        response_processor::{ResponseProcessingContext, ResponseProcessingProfile},
        ProxyProcessors,
    },
    servers::{Server, ServerStorageService},
};
use axum::http::StatusCode;
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
    pub processors: Arc<ProxyProcessors>,
    pub quick_connect: QuickConnectStorage,
    pub federated_users: Arc<FederatedUserService>,
    pub syncplay: Arc<SyncPlayService>,
    pub client_sessions: Arc<sessions::ClientSessionService>,
    pub(crate) catalog_snapshots: Arc<application::catalog::snapshots::CatalogSnapshots>,
}

impl AppState {
    pub(crate) fn catalog_service(&self) -> application::catalog::services::CatalogService {
        application::catalog::services::CatalogService {
            media_storage: self.media_storage.clone(),
            server_storage: self.server_storage.clone(),
            virtual_library_service: self.virtual_library_service.clone(),
            play_sessions: self.play_sessions.clone(),
            config: self.config.clone(),
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
            config: &self.config,
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
            config: &self.config,
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
            streaming_reqwest_client,
            user_authorization: data_context.user_authorization,
            server_storage: data_context.server_storage,
            media_storage: data_context.media_storage,
            virtual_library_service: data_context.virtual_library_service,
            play_sessions: data_context.play_sessions,
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
    pub async fn process_response_json(
        &self,
        payload: &mut serde_json::Value,
        server: &Server,
        profile: ResponseProcessingProfile,
        proxy_api_key: Option<&str>,
    ) -> Result<bool, StatusCode> {
        self.process_response_json_typed(payload, server, profile, proxy_api_key)
            .await
            .map_err(|error| error.detail_status())
    }

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
