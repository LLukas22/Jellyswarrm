//! Owned session/control handles; workflows borrow them for one operation.
use crate::{
    application, auth::UserAuthorizationService, config::SettingsView, media::MediaStorageService,
    sessions::ClientSessionService, syncplay::SyncPlayService,
};
use std::sync::Arc;

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
