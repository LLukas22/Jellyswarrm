//! Cross-service media authorization for client control and SyncPlay workflows.
use std::sync::Arc;

use crate::{
    application::request::selection::UrlSelection, auth::UserAuthorizationService,
    libraries::service::VirtualLibraryAccessScope, media::MediaStorageService,
};

/// Read-only media authorization capability shared by client control and
/// SyncPlay application operations, without incoming HTTP status policy.
#[derive(Clone)]
pub(crate) struct MediaAccess {
    authorization: Arc<UserAuthorizationService>,
    media: Arc<MediaStorageService>,
    selection: UrlSelection,
}
impl MediaAccess {
    pub(crate) fn new(
        authorization: Arc<UserAuthorizationService>,
        media: Arc<MediaStorageService>,
        selection: UrlSelection,
    ) -> Self {
        Self {
            authorization,
            media,
            selection,
        }
    }

    pub(crate) async fn allows(
        &self,
        user_id: &str,
        items: &[String],
        source_id: Option<&str>,
    ) -> Result<bool, sqlx::Error> {
        if items.is_empty() {
            return Ok(true);
        }
        let Some((_, sessions)) = self
            .authorization
            .get_user_sessions_by_user_id(user_id)
            .await?
        else {
            return Ok(false);
        };
        let scope =
            VirtualLibraryAccessScope::new(user_id.to_string(), sessions.iter().map(|(_, s)| s.id));
        for item in items {
            if self
                .selection
                .client_media_mapping(item, Some(&scope), None)
                .await?
                .is_none()
            {
                return Ok(false);
            }
        }
        if let Some(source) = source_id {
            // A source selection applies to the first item; the queue retains virtual IDs.
            if let Some(group) = self.media.get_media_version_group(&items[0]).await? {
                let Some(route) = self
                    .media
                    .get_media_version_source_route(group.id, source)
                    .await?
                else {
                    return Ok(false);
                };
                if !sessions
                    .iter()
                    .any(|(_, server)| server.id == route.member_mapping.server_id)
                {
                    return Ok(false);
                }
            } else {
                let Some(item) = self
                    .selection
                    .client_media_mapping(&items[0], Some(&scope), None)
                    .await?
                else {
                    return Ok(false);
                };
                if self
                    .selection
                    .client_media_mapping(source, Some(&scope), Some(item.server_id))
                    .await?
                    .is_none()
                {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }
}
