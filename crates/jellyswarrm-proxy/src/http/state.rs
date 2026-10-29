//! Adapter-owned handles, wired by startup without I/O or configuration reads.
mod management;
mod media;
mod paths;
mod sessions;

pub(crate) use management::{ManagementState, SettingsState};
pub(crate) use media::MediaState;
pub(crate) use paths::AdapterPaths;
pub(crate) use sessions::SessionState;

use crate::application::accounts::services::{AccountService, AuthenticationService};
use axum::extract::FromRef;

#[derive(Clone, FromRef)]
pub(crate) struct AppState {
    media: MediaState,
    sessions: SessionState,
    accounts: AccountService,
    management: ManagementState,
    settings: SettingsState,
    quick_connect: crate::auth::quick_connect::QuickConnectStorage,
    paths: AdapterPaths,
}

impl AppState {
    pub(crate) fn new(
        media: MediaState,
        sessions: SessionState,
        accounts: AccountService,
        management: ManagementState,
        settings: SettingsState,
        quick_connect: crate::auth::quick_connect::QuickConnectStorage,
    ) -> Self {
        let paths = management.paths.clone();
        Self {
            media,
            sessions,
            accounts,
            management,
            settings,
            quick_connect,
            paths,
        }
    }
}

impl FromRef<AppState> for AuthenticationService {
    fn from_ref(state: &AppState) -> Self {
        state.accounts.authentication.clone()
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
