//! Authorize client-session discovery and remote-control operations for the current user.
use crate::libraries::service::VirtualLibraryAccessScope;

/// Read-only media authorization capability shared by client control and
/// SyncPlay application operations, without incoming HTTP status policy.
pub(crate) struct MediaAccess<'a> {
    authorization: &'a crate::auth::UserAuthorizationService,
    media: &'a crate::media::MediaStorageService,
    selection: &'a crate::application::url_selection::UrlSelection,
}
impl<'a> MediaAccess<'a> {
    pub(crate) fn new(
        authorization: &'a crate::auth::UserAuthorizationService,
        media: &'a crate::media::MediaStorageService,
        selection: &'a crate::application::url_selection::UrlSelection,
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
