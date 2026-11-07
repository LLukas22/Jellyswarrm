//! Analyze request body IDs and playback hints without mutating the payload.
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use crate::{
    auth::service::LocalUser,
    auth::UserAuthorizationService,
    domain::ids::{PlaybackSessionId, VirtualItemId},
    media::MediaStorageService,
    playback::{PlaybackRegistry, RevisionedPlaybackSession},
    protocol::routes::PlaybackSessionAction,
    servers::compare_routes,
    servers::storage::Server,
    servers::ServerStorageService,
    translation::{
        field_matcher::{ID_FIELDS, MEDIA_ID_LIST_PARENT_FIELDS, SESSION_FIELDS, USER_FIELDS},
        json_processor::{JsonAnalyzer, JsonProcessingContext},
    },
};

pub(crate) struct RequestAnalyzer {
    media_storage: Arc<MediaStorageService>,
    server_storage: Arc<ServerStorageService>,
    playback_registry: Arc<PlaybackRegistry>,
    user_authorization: Arc<UserAuthorizationService>,
}

impl RequestAnalyzer {
    pub(crate) fn new(
        media_storage: Arc<MediaStorageService>,
        server_storage: Arc<ServerStorageService>,
        playback_registry: Arc<PlaybackRegistry>,
        user_authorization: Arc<UserAuthorizationService>,
    ) -> Self {
        Self {
            media_storage,
            server_storage,
            playback_registry,
            user_authorization,
        }
    }

    async fn media_version_server(&self, aggregate_id: &VirtualItemId) -> Result<Option<Server>> {
        let members = self
            .media_storage
            .get_media_version_members_by_virtual_id(aggregate_id)
            .await?;
        let mut healthy_members = Vec::new();
        for member in members {
            if self
                .server_storage
                .server_status(member.server.id)
                .await
                .is_healthy()
            {
                healthy_members.push(member);
            }
        }
        Ok(healthy_members
            .into_iter()
            .max_by(|left, right| {
                compare_routes(
                    &left.server,
                    left.mapping.original_media_id.as_str(),
                    &right.server,
                    right.mapping.original_media_id.as_str(),
                )
            })
            .map(|member| member.server))
    }
}

#[derive(Debug, Default)]
pub(crate) struct RequestBodyAnalysisResult {
    pub(crate) found_ids: Vec<String>,
    pub(crate) found_session_ids: Vec<String>,
    pub(crate) found_user_ids: Vec<String>,
    pub(crate) servers: Vec<Server>,
    pub(crate) users: Vec<LocalUser>,
    pub(crate) authoritative_play_session: Option<RevisionedPlaybackSession>,
    pub(crate) requested_play_session_id: Option<PlaybackSessionId>,
    pub(crate) requested_play_item_id: Option<VirtualItemId>,
    pub(crate) requested_play_source_id: Option<VirtualItemId>,
}

impl RequestBodyAnalysisResult {
    /// Returns the server with the highest occurance in the servers vector, or None if the vector is empty.
    pub(crate) fn get_server(&self) -> Option<Server> {
        if self.servers.is_empty() {
            return None;
        }
        let mut server_count = std::collections::HashMap::new();
        for server in &self.servers {
            *server_count.entry(server).or_insert(0) += 1;
        }
        let (most_common_server, _) = server_count.into_iter().max_by_key(|&(_, count)| count)?;
        Some(most_common_server.clone())
    }

    /// Returns the user with the highest occurance in the users vector, or None if the vector is empty.
    pub(crate) fn get_user(&self) -> Option<LocalUser> {
        if self.users.is_empty() {
            return None;
        }
        let mut user_count = std::collections::HashMap::new();
        for user in &self.users {
            *user_count.entry(user).or_insert(0) += 1;
        }
        let (most_common_user, _) = user_count.into_iter().max_by_key(|&(_, count)| count)?;
        Some(most_common_user.clone())
    }
}

pub(crate) struct RequestAnalysisContext {
    pub(crate) authenticated_user_id: Option<crate::domain::user_id::UserId>,
    pub(crate) playback_session_action: Option<PlaybackSessionAction>,
}

#[async_trait]
impl JsonAnalyzer<RequestAnalysisContext, RequestBodyAnalysisResult> for RequestAnalyzer {
    async fn analyze(
        &self,
        json_context: &JsonProcessingContext,
        value: &Value,
        context: &RequestAnalysisContext,
        accumulator: &mut RequestBodyAnalysisResult,
    ) -> Result<()> {
        if context.playback_session_action.is_some()
            && json_context.depth == 0
            && json_context.parent_path.is_empty()
        {
            let is_item = json_context.key.eq_ignore_ascii_case("ItemId");
            let is_source = json_context.key.eq_ignore_ascii_case("MediaSourceId");
            let is_session = json_context.key.eq_ignore_ascii_case("PlaySessionId");
            if (is_item || is_source || is_session) && !value.is_null() {
                let id = value
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid playback authority field"))?;
                if id.trim().is_empty() {
                    if is_item {
                        anyhow::bail!("playback report requires a nonempty item ID");
                    }
                    // Clients may send blank optional IDs instead of omitting them.
                    return Ok(());
                }
                if is_item {
                    record_authority(&mut accumulator.requested_play_item_id, id)?;
                } else if is_source {
                    record_authority(&mut accumulator.requested_play_source_id, id)?;
                } else {
                    record_authority(&mut accumulator.requested_play_session_id, id)?;
                }
            }
        }
        // Check if this is an ID field (case-insensitive). Media ID lists
        // (e.g. `Ids`, `EntryIds`) carry one ID per array item, where the
        // key is the array index, so match on the parent field instead.
        if ID_FIELDS.contains(&json_context.key)
            || (json_context.is_array_item
                && MEDIA_ID_LIST_PARENT_FIELDS.contains(last_segment(&json_context.parent_path)))
        {
            if let serde_json::Value::String(ref wire_id) = value {
                let virtual_id = VirtualItemId::from(wire_id.as_str());
                let server = self
                    .media_storage
                    .get_media_mapping_with_server(&virtual_id)
                    .await?
                    .map(|(_, server)| server);
                if let Some(server) = match server {
                    Some(server) => Some(server),
                    None => self.media_version_server(&virtual_id).await?,
                } {
                    accumulator.servers.push(server);
                }
                accumulator.found_ids.push(wire_id.clone());
            }
        }

        // Check if this is a SessionId field (case-insensitive)
        if SESSION_FIELDS.contains(&json_context.key) {
            if let serde_json::Value::String(ref session_id) = value {
                let is_authoritative_field = context.playback_session_action.is_some()
                    && json_context.depth == 0
                    && json_context.parent_path.is_empty()
                    && json_context.key.eq_ignore_ascii_case("PlaySessionId");
                if is_authoritative_field {
                    if let Some(user_id) = context.authenticated_user_id.as_deref() {
                        let binding = self
                            .playback_registry
                            .resolve_report_binding(&session_id.as_str().into(), &user_id.into())
                            .await?;
                        accumulator.authoritative_play_session = binding;
                    }
                } else if context.playback_session_action.is_none() {
                    if let Some(play_session) = match context.authenticated_user_id.as_deref() {
                        Some(user_id) => {
                            self.playback_registry
                                .get_session_for_user(&session_id.as_str().into(), &user_id.into())
                                .await
                        }
                        None => None,
                    } {
                        if let Some(server) = self
                            .server_storage
                            .get_server_by_id(play_session.server_id)
                            .await?
                        {
                            accumulator.servers.push(server);
                        } else {
                            self.playback_registry
                                .remove_sessions_for_server(play_session.server_id)
                                .await;
                        }
                    }
                }
                accumulator.found_session_ids.push(session_id.clone());
            }
        }

        // Check if this is a UserId field (case-insensitive)
        if USER_FIELDS.contains(&json_context.key) {
            if let serde_json::Value::String(ref user_id) = value {
                if let Some(user) = self.user_authorization.get_user_by_id(user_id).await? {
                    accumulator.users.push(user);
                }
                accumulator.found_user_ids.push(user_id.clone());
            }
        }
        Ok(())
    }
}

/// A report may repeat an authority field only with the same identity.
fn record_authority<T>(field: &mut Option<T>, id: &str) -> Result<()>
where
    T: PartialEq + for<'a> From<&'a str>,
{
    let id = T::from(id);
    if field.as_ref().is_some_and(|previous| *previous != id) {
        anyhow::bail!("conflicting playback authority fields");
    }
    *field = Some(id);
    Ok(())
}

fn last_segment(path: &str) -> &str {
    path.rsplit('.')
        .next()
        .map(|segment| segment.split('[').next().unwrap_or(segment))
        .unwrap_or(path)
}
