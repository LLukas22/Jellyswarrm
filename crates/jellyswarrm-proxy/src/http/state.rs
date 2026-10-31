//! Service handles for the HTTP and UI handlers, built at startup.
use crate::{
    application::{
        accounts::AuthenticationService,
        admin::{
            libraries::LibraryAdministration,
            servers::{ServerAdministration, ServerCredentials},
        },
        services::MediaServices,
        sessions::SessionOperations,
        syncplay::SyncPlayOperations,
    },
    auth::{
        federation::FederatedUserService, mapping_authentication::MappingAuthenticator,
        quick_connect::QuickConnectStorage, UserAuthorizationService,
    },
    config::{service::ConfigurationService, AdminCredentials, SettingsView},
    libraries::VirtualLibraryService,
    servers::ServerStorageService,
};
use axum::extract::FromRef;
use std::sync::Arc;

#[derive(Clone, FromRef)]
pub(crate) struct AppState {
    media: MediaServices,
    sessions: SessionOperations,
    syncplay: SyncPlayOperations,
    authentication: AuthenticationService,
    management: ManagementState,
    settings: SettingsState,
    quick_connect: crate::auth::quick_connect::QuickConnectStorage,
    paths: AdapterPaths,
}

impl AppState {
    pub(crate) fn new(
        media: MediaServices,
        sessions: SessionOperations,
        syncplay: SyncPlayOperations,
        authentication: AuthenticationService,
        management: ManagementState,
        settings: SettingsState,
        quick_connect: crate::auth::quick_connect::QuickConnectStorage,
    ) -> Self {
        let paths = management.paths.clone();
        Self {
            media,
            sessions,
            syncplay,
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

/// Management handles. Reads use snapshots; persistence uses ConfigurationService.
#[derive(Clone)]
pub(crate) struct SettingsState {
    pub(crate) settings: SettingsView,
    pub(crate) configuration: ConfigurationService,
    pub(crate) paths: AdapterPaths,
}

/// Administration services are built once at startup from the same shared handles.
#[derive(Clone)]
pub(crate) struct ManagementState {
    pub(crate) reqwest_client: reqwest::Client,
    pub(crate) user_authorization: Arc<UserAuthorizationService>,
    pub(crate) server_storage: Arc<ServerStorageService>,
    pub(crate) virtual_library_service: Arc<VirtualLibraryService>,
    pub(crate) federated_users: Arc<FederatedUserService>,
    pub(crate) quick_connect: QuickConnectStorage,
    pub(crate) credentials: AdminCredentials,
    pub(crate) settings: SettingsView,
    pub(crate) configuration: ConfigurationService,
    pub(crate) paths: AdapterPaths,
    pub(crate) sessions: SessionOperations,
    pub(crate) mapping_authenticator: MappingAuthenticator,
    pub(crate) server_credentials: ServerCredentials,
    pub(crate) server_administration: ServerAdministration,
    pub(crate) library_administration: LibraryAdministration,
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
