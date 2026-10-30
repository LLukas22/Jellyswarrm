//! Adapter-owned handles, wired by startup without I/O or configuration reads.
use crate::{
    application::{self, accounts::AuthenticationService, services::MediaServices},
    auth::{
        federation::FederatedUserService, quick_connect::QuickConnectStorage,
        UserAuthorizationService,
    },
    config::{service::ConfigurationService, AdminCredentials, SettingsView},
    libraries::VirtualLibraryService,
    media::MediaStorageService,
    playback::SessionStorage,
    servers::ServerStorageService,
    sessions::ClientSessionService,
    syncplay::SyncPlayService,
};
use axum::extract::FromRef;
use std::sync::Arc;

#[derive(Clone, FromRef)]
pub(crate) struct AppState {
    media: MediaServices,
    sessions: SessionState,
    authentication: AuthenticationService,
    management: ManagementState,
    settings: SettingsState,
    quick_connect: crate::auth::quick_connect::QuickConnectStorage,
    paths: AdapterPaths,
}

impl AppState {
    pub(crate) fn new(
        media: MediaServices,
        sessions: SessionState,
        authentication: AuthenticationService,
        management: ManagementState,
        settings: SettingsState,
        quick_connect: crate::auth::quick_connect::QuickConnectStorage,
    ) -> Self {
        let paths = management.paths.clone();
        Self {
            media,
            sessions,
            authentication,
            management,
            settings,
            quick_connect,
            paths,
        }
    }
}

impl FromRef<AppState> for reqwest::Client {
    fn from_ref(state: &AppState) -> Self {
        state.media.reqwest_client.clone()
    }
}
impl FromRef<AppState> for std::sync::Arc<crate::auth::UserAuthorizationService> {
    fn from_ref(state: &AppState) -> Self {
        state.media.user_authorization.clone()
    }
}
impl FromRef<AppState> for std::sync::Arc<crate::servers::ServerStorageService> {
    fn from_ref(state: &AppState) -> Self {
        state.media.server_storage.clone()
    }
}

/// Immutable adapter paths resolved at startup, not writable configuration.
#[derive(Clone)]
pub(crate) struct AdapterPaths {
    ui_route: String,
    url_prefix: Option<String>,
}
impl AdapterPaths {
    pub(crate) fn new(startup: &crate::config::StartupConfig) -> Self {
        Self {
            ui_route: startup.ui_route.to_string(),
            url_prefix: startup.url_prefix.as_ref().map(ToString::to_string),
        }
    }
    pub(crate) fn ui_route(&self) -> String {
        match &self.url_prefix {
            Some(prefix) => format!("{prefix}/{}", self.ui_route),
            None => self.ui_route.clone(),
        }
    }
    pub(crate) fn url_prefix(&self) -> Option<String> {
        self.url_prefix.clone()
    }
}

/// Owned session/control handles; workflows borrow them for one operation.
#[derive(Clone)]
pub(crate) struct SessionState {
    pub(crate) user_authorization: Arc<UserAuthorizationService>,
    pub(crate) media_storage: Arc<MediaStorageService>,
    pub(crate) url_selection: application::url_selection::UrlSelection,
    pub(crate) settings: SettingsView,
    pub(crate) client_sessions: Arc<ClientSessionService>,
    pub(crate) syncplay: Arc<SyncPlayService>,
}
impl SessionState {
    fn media_access(&self) -> application::sessions::access::MediaAccess<'_> {
        application::sessions::access::MediaAccess::new(
            &self.user_authorization,
            &self.media_storage,
            &self.url_selection,
        )
    }
    pub(crate) fn session_operations(&self) -> application::sessions::SessionOperations<'_> {
        application::sessions::SessionOperations::new(
            &self.user_authorization,
            &self.client_sessions,
            &self.syncplay,
            &self.settings,
            self.media_access(),
        )
    }
    pub(crate) fn syncplay_operations(&self) -> application::syncplay::SyncPlayOperations<'_> {
        application::syncplay::SyncPlayOperations::new(&self.syncplay, self.media_access())
    }
}

/// Management handles. Reads use snapshots; persistence uses ConfigurationService.
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn adapter_paths_are_startup_snapshots_and_preserve_prefix_semantics() {
        let mut config = crate::config::AppConfig {
            url_prefix: Some("proxy".into()),
            ui_route: "manage".into(),
            ..Default::default()
        };
        let paths = AdapterPaths::new(&config.startup_config());
        config.url_prefix = None;
        config.ui_route = "changed".into();
        assert_eq!(paths.ui_route(), "proxy/manage");
        assert_eq!(paths.url_prefix().as_deref(), Some("proxy"));
        let unprefixed = AdapterPaths::new(&config.startup_config());
        assert_eq!(unprefixed.ui_route(), "changed");
    }
}
