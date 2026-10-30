//! Owned account capabilities allow login workers to outlive an individual poll,
//! without granting access to writable configuration or unrelated workflows.
use std::sync::Arc;

use crate::{
    auth::{mapping_authentication::MappingAuthenticator, UserAuthorizationService},
    config::SettingsView,
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
    pub(crate) admin_password: crate::domain::security::Password,
}

impl AuthenticationService {
    pub(crate) fn mapping_authenticator(&self) -> MappingAuthenticator<'_> {
        MappingAuthenticator::new(
            &self.user_authorization,
            self.reqwest_client.clone(),
            self.admin_password.clone(),
        )
    }
}
