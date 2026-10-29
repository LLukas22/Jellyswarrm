//! Management handles. Reads use snapshots; persistence uses ConfigurationService.
use super::{paths::AdapterPaths, sessions::SessionState};
use crate::{
    application,
    auth::{
        federation::FederatedUserService, quick_connect::QuickConnectStorage,
        UserAuthorizationService,
    },
    config::{service::ConfigurationService, AdminCredentials, SettingsView},
    libraries::VirtualLibraryService,
    playback::sessions::SessionStorage,
    servers::ServerStorageService,
};
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct SettingsState {
    pub(crate) settings: SettingsView,
    pub(crate) configuration: ConfigurationService,
    pub(crate) paths: AdapterPaths,
}

#[derive(Clone)]
pub(crate) struct ManagementState {
    pub(crate) reqwest_client: reqwest::Client,
    pub(crate) user_authorization: Arc<UserAuthorizationService>,
    pub(crate) server_storage: Arc<ServerStorageService>,
    pub(crate) virtual_library_service: Arc<VirtualLibraryService>,
    pub(crate) play_sessions: Arc<SessionStorage>,
    pub(crate) catalog_snapshots: Arc<application::catalog::snapshots::CatalogSnapshots>,
    pub(crate) federated_users: Arc<FederatedUserService>,
    pub(crate) quick_connect: QuickConnectStorage,
    pub(crate) credentials: AdminCredentials,
    pub(crate) settings: SettingsView,
    pub(crate) configuration: ConfigurationService,
    pub(crate) paths: AdapterPaths,
    pub(crate) sessions: SessionState,
}
impl ManagementState {
    pub(crate) fn server_credentials(&self) -> application::server_admin::ServerCredentials<'_> {
        application::server_admin::ServerCredentials::new(
            &self.server_storage,
            self.reqwest_client.clone(),
            (&self.credentials.password()).into(),
        )
    }
    pub(crate) fn server_administration(
        &self,
    ) -> application::server_admin::ServerAdministration<'_> {
        application::server_admin::ServerAdministration::new(
            &self.server_storage,
            &self.play_sessions,
            &self.catalog_snapshots,
        )
    }
    pub(crate) fn library_administration(
        &self,
    ) -> application::library_admin::LibraryAdministration<'_> {
        application::library_admin::LibraryAdministration::new(
            &self.server_storage,
            &self.user_authorization,
            &self.virtual_library_service,
            self.mapping_authenticator(),
            (&self.credentials.password()).into(),
            self.reqwest_client.clone(),
        )
    }
    pub(crate) fn mapping_authenticator(
        &self,
    ) -> crate::auth::mapping_authentication::MappingAuthenticator<'_> {
        crate::auth::mapping_authentication::MappingAuthenticator::new(
            &self.user_authorization,
            self.reqwest_client.clone(),
            self.credentials.password(),
        )
    }
}
