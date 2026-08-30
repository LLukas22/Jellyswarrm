use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::{
    processors::{
        field_matcher::{ID_FIELDS, SESSION_FIELDS, USER_FIELDS},
        json_processor::{JsonAnalyzer, JsonProcessingContext},
    },
    server_storage::Server,
    session_storage::PlaybackSession,
    user_authorization_service::User,
    virtual_library_service::compare_virtual_library_routes,
    DataContext,
};

pub struct RequestAnalyzer {
    pub data_context: DataContext,
}

impl RequestAnalyzer {
    pub fn new(data_context: DataContext) -> Self {
        Self { data_context }
    }

    async fn movie_version_server(&self, aggregate_id: &str) -> Result<Option<Server>> {
        let members = self
            .data_context
            .media_storage
            .get_movie_version_members_by_virtual_id(aggregate_id)
            .await?;
        let mut healthy_members = Vec::new();
        for member in members {
            if self
                .data_context
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
                compare_virtual_library_routes(
                    &left.server,
                    &left.mapping.original_media_id,
                    &right.server,
                    &right.mapping.original_media_id,
                )
            })
            .map(|member| member.server))
    }
}

#[derive(Debug, Default)]
pub struct RequestBodyAnalysisResult {
    pub found_ids: Vec<String>,
    pub found_session_ids: Vec<String>,
    pub found_user_ids: Vec<String>,
    pub servers: Vec<Server>,
    pub users: Vec<User>,
    pub authoritative_play_session: Option<PlaybackSession>,
    pub requested_play_session_id: Option<String>,
    pub requested_play_item_id: Option<String>,
}

impl RequestBodyAnalysisResult {
    /// Returns the server with the highest occurance in the servers vector, or None if the vector is empty.
    pub fn get_server(&self) -> Option<Server> {
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
    pub fn get_user(&self) -> Option<User> {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackSessionAction {
    Start,
    Refresh,
    Remove,
}

pub struct RequestAnalysisContext {
    pub authenticated_user_id: Option<String>,
    pub playback_session_action: Option<PlaybackSessionAction>,
}

#[async_trait]
impl JsonAnalyzer<RequestAnalysisContext, RequestBodyAnalysisResult> for RequestAnalyzer {
    async fn analyze(
        &self,
        json_context: &JsonProcessingContext,
        value: &Value,
        context: &RequestAnalysisContext,
        accumulator: &mut RequestBodyAnalysisResult,
    ) -> Result<Option<Vec<String>>> {
        // Check if this is an ID field (case-insensitive)
        if ID_FIELDS.contains(&json_context.key) {
            if let serde_json::Value::String(ref virtual_id) = value {
                let server = self
                    .data_context
                    .media_storage
                    .get_media_mapping_with_server(virtual_id)
                    .await?
                    .map(|(_, server)| server);
                if let Some(server) = match server {
                    Some(server) => Some(server),
                    None => self.movie_version_server(virtual_id).await?,
                } {
                    accumulator.servers.push(server);
                }
                accumulator.found_ids.push(virtual_id.clone());
                if context.playback_session_action.is_some()
                    && json_context.depth == 0
                    && json_context.key.eq_ignore_ascii_case("ItemId")
                {
                    accumulator.requested_play_item_id = Some(virtual_id.clone());
                }
            }
        }

        // Check if this is a SessionId field (case-insensitive)
        if SESSION_FIELDS.contains(&json_context.key) {
            if let serde_json::Value::String(ref session_id) = value {
                let is_authoritative_field = context.playback_session_action.is_some()
                    && json_context.depth == 0
                    && json_context.key.eq_ignore_ascii_case("PlaySessionId");
                if is_authoritative_field {
                    accumulator.requested_play_session_id = Some(session_id.clone());
                    if let Some(user_id) = context.authenticated_user_id.as_deref() {
                        accumulator.authoritative_play_session = self
                            .data_context
                            .play_sessions
                            .get_session_for_user(session_id, user_id)
                            .await;
                    }
                } else if context.playback_session_action.is_none() {
                    if let Some(play_session) = match context.authenticated_user_id.as_deref() {
                        Some(user_id) => {
                            self.data_context
                                .play_sessions
                                .get_session_for_user(session_id, user_id)
                                .await
                        }
                        None => None,
                    } {
                        if let Some(server) = self
                            .data_context
                            .server_storage
                            .get_server_by_id(play_session.server_id)
                            .await?
                        {
                            accumulator.servers.push(server);
                        } else {
                            self.data_context
                                .play_sessions
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
                if let Some(user) = self
                    .data_context
                    .user_authorization
                    .get_user_by_id(user_id)
                    .await?
                {
                    accumulator.users.push(user);
                }
                accumulator.found_user_ids.push(user_id.clone());
            }
        }
        Ok(None)
    }
}
