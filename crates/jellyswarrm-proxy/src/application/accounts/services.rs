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
    pub(crate) user_authorization: Arc<UserAuthorizationService>,
    pub(crate) server_storage: Arc<ServerStorageService>,
    pub(crate) settings: SettingsView,
    pub(crate) reqwest_client: reqwest::Client,
    pub(crate) client_sessions: Arc<ClientSessionService>,
    pub(crate) admin_password: crate::auth::encryption::Password,
}

impl AuthenticationService {
    pub(crate) async fn mapping_authenticator(&self) -> MappingAuthenticator<'_> {
        MappingAuthenticator::new(
            &self.user_authorization,
            self.reqwest_client.clone(),
            self.admin_password.clone(),
        )
    }
}

#[derive(Clone)]
pub(crate) struct AccountService {
    pub(crate) authentication: AuthenticationService,
    pub(crate) media_storage: Arc<MediaStorageService>,
    pub(crate) play_sessions: Arc<SessionStorage>,
    pub(crate) urls: UrlProcessor,
    pub(crate) selection: crate::application::url_selection::UrlSelection,
}

impl AccountService {
    pub(crate) fn routing_services(&self) -> crate::application::routing::RoutingServices<'_> {
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
