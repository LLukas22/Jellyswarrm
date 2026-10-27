//! Owned account capabilities allow login workers to outlive an individual poll,
//! without granting access to writable configuration or unrelated workflows.
use std::sync::Arc;

use crate::{
    auth::{mapping_authentication::MappingAuthenticator, UserAuthorizationService},
    config::SettingsView,
    media::MediaStorageService,
    playback::sessions::SessionStorage,
    processors::url_processor::UrlProcessor,
    servers::ServerStorageService,
    sessions::ClientSessionService,
};

#[derive(Clone)]
pub(crate) struct AuthenticationService {
    pub user_authorization: Arc<UserAuthorizationService>,
    pub server_storage: Arc<ServerStorageService>,
    pub settings: SettingsView,
    pub reqwest_client: reqwest::Client,
    pub client_sessions: Arc<ClientSessionService>,
    pub admin_password: crate::auth::encryption::Password,
}

impl AuthenticationService {
    pub async fn mapping_authenticator(&self) -> MappingAuthenticator<'_> {
        MappingAuthenticator::new(
            &self.user_authorization,
            self.reqwest_client.clone(),
            self.admin_password.clone(),
        )
    }
}

#[derive(Clone)]
pub(crate) struct AccountService {
    pub authentication: AuthenticationService,
    pub media_storage: Arc<MediaStorageService>,
    pub play_sessions: Arc<SessionStorage>,
    pub urls: UrlProcessor,
    pub selection: crate::application::url_selection::UrlSelection,
}

impl AccountService {
    pub fn routing_services(&self) -> crate::application::routing::RoutingServices<'_> {
        crate::application::routing::RoutingServices {
            media_storage: &self.media_storage,
            server_storage: &self.authentication.server_storage,
            play_sessions: &self.play_sessions,
            urls: &self.urls,
            config: &self.authentication.settings,
            selection: &self.selection,
        }
    }
}
