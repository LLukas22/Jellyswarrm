//! Soft, client-scoped server preference for successive episodes of one series.
//! Explicit sources and playback bindings remain authoritative.
use crate::{
    application::{
        context::{AuthorizedTarget, RequestContext},
        error::ApplicationError,
    },
    media::storage::MediaStorageService,
    servers::id::ServerId,
    sessions::ClientSessionService,
};

pub(crate) struct EpisodeContinuity {
    client_session: String,
    series_id: String,
}

impl EpisodeContinuity {
    pub(crate) async fn prepare(
        media: &MediaStorageService,
        clients: &ClientSessionService,
        context: &RequestContext,
        item: &str,
    ) -> Result<Option<Self>, ApplicationError> {
        let (Some(user), Some(token), Some(device)) = (
            context.user(),
            context.authorization().and_then(|auth| auth.token_ref()),
            context
                .device()
                .filter(|device| !device.device_id.is_empty()),
        ) else {
            return Ok(None);
        };
        let Some(series_id) = media
            .get_episode_series_id(item, user.id.as_str())
            .await
            .map_err(ApplicationError::Storage)?
        else {
            return Ok(None);
        };
        Ok(Some(Self {
            client_session: clients.ensure(user, token, Some(device)).await,
            series_id,
        }))
    }

    pub(super) async fn preferred_target(
        &self,
        state: &super::services::PlaybackService<'_>,
        context: &RequestContext,
        item: &str,
    ) -> Result<Option<AuthorizedTarget>, ApplicationError> {
        let Some(server_id) = state
            .client_sessions
            .preferred_episode_server(&self.client_session, &self.series_id)
            .await
        else {
            return Ok(None);
        };
        // Only a merged item may choose a different owner. Explicit member IDs
        // stay pinned, and an unavailable preference falls back to normal routing.
        let members = state
            .media_storage
            .get_media_version_members_by_virtual_id(item)
            .await
            .map_err(ApplicationError::Storage)?;
        if !members.iter().any(|member| member.server.id == server_id)
            || !state
                .server_storage
                .server_status(server_id)
                .await
                .is_healthy()
        {
            return Ok(None);
        }
        Ok(context.target(server_id).ok())
    }

    pub(crate) async fn remember(&self, clients: &ClientSessionService, server_id: ServerId) {
        clients
            .remember_episode_server(&self.client_session, &self.series_id, server_id)
            .await;
    }
}
