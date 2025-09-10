use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::{
    processors::{
        field_matcher::{ID_FIELDS, SESSION_FIELDS, USER_FIELDS},
        json_processor::{JsonAnalyzer, JsonProcessingContext},
    },
    server_storage::Server,
    user_authorization_service::User,
    DataContext,
};

pub struct RequestAnalyzer {
    pub data_context: DataContext,
}

impl RequestAnalyzer {
    pub fn new(data_context: DataContext) -> Self {
        Self { data_context }
    }
}

#[derive(Debug, Default)]
pub struct RequestBodyAnalysisResult {
    pub found_ids: Vec<String>,
    pub found_session_ids: Vec<String>,
    pub found_user_ids: Vec<String>,
    pub servers: Vec<Server>,
    pub users: Vec<User>,
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
        let (most_common_server, _) = server_count
            .into_iter()
            .max_by_key(|&(_, count)| count)
            .unwrap();
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
        let (most_common_user, _) = user_count
            .into_iter()
            .max_by_key(|&(_, count)| count)
            .unwrap();
        Some(most_common_user.clone())
    }
}

pub struct RequestAnalysisContext;

#[async_trait]
impl JsonAnalyzer<RequestAnalysisContext, RequestBodyAnalysisResult> for RequestAnalyzer {
    async fn analyze(
        &self,
        json_context: &JsonProcessingContext,
        value: &Value,
        _context: &RequestAnalysisContext,
        accumulator: &mut RequestBodyAnalysisResult,
    ) -> Result<Option<Vec<String>>> {
        // Check if this is an ID field (case-insensitive)
        if ID_FIELDS.contains(&json_context.key) {
            if let serde_json::Value::String(ref virtual_id) = value {
                if let Some((_, server)) = self
                    .data_context
                    .media_storage
                    .get_media_mapping_with_server(virtual_id)
                    .await?
                {
                    accumulator.servers.push(server);
                }
                accumulator.found_ids.push(virtual_id.clone());
            }
        }

        // Check if this is a SessionId field (case-insensitive)
        if SESSION_FIELDS.contains(&json_context.key) {
            if let serde_json::Value::String(ref session_id) = value {
                if let Some(play_session) = self
                    .data_context
                    .play_sessions
                    .get_session(session_id)
                    .await
                {
                    accumulator.servers.push(play_session.server);
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
