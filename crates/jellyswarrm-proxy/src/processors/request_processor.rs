use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::LazyLock;
use tracing::info;

use crate::processors::json_processor::{
    AsyncJsonProcessor, JsonProcessingContext, JsonProcessingResult,
};
use crate::request_preprocessing::{JellyfinAuthorization, PreprocessedRequest};
use crate::server_storage::Server;
use crate::user_authorization_service::{AuthorizationSession, User};
use crate::AppState;

/// A struct for case-insensitive field name matching
pub struct FieldMatcher {
    fields: HashSet<String>,
}

impl FieldMatcher {
    /// Create a new FieldMatcher with the given field names
    pub fn new(fields: &[&str]) -> Self {
        Self {
            fields: fields.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Check if a field name matches any of the stored fields (case-insensitive)
    pub fn contains(&self, field_name: &str) -> bool {
        self.fields
            .iter()
            .any(|field| field.eq_ignore_ascii_case(field_name))
    }

    /// Add a new field to the matcher
    pub fn add_field(&mut self, field_name: &str) {
        self.fields.insert(field_name.to_string());
    }
}

// Static field matchers for different field types
static ID_FIELDS: LazyLock<FieldMatcher> = LazyLock::new(|| {
    FieldMatcher::new(&[
        "Id",
        "ItemId",
        "ParentId",
        "SeriesId",
        "SeasonId",
        "MediaSourceId",
        "PlaylistItemId",
    ])
});

static SESSION_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["SessionId", "PlaySessionId"]));

static USER_FIELDS: LazyLock<FieldMatcher> = LazyLock::new(|| FieldMatcher::new(&["UserId"]));

pub struct RequestProcessor {
    pub state: AppState,
}

impl RequestProcessor {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

pub struct RequestProcessingContext {
    pub user: Option<User>,
    pub server: Server,
    pub sessions: Option<Vec<(AuthorizationSession, Server)>>,
    pub auth: Option<JellyfinAuthorization>,
    pub session: Option<AuthorizationSession>,
    pub new_auth: Option<JellyfinAuthorization>,
}

impl RequestProcessingContext {
    pub fn new(preprocessed_request: &PreprocessedRequest) -> Self {
        Self {
            user: preprocessed_request.user.clone(),
            server: preprocessed_request.server.clone(),
            sessions: preprocessed_request.sessions.clone(),
            auth: preprocessed_request.auth.clone(),
            session: preprocessed_request.session.clone(),
            new_auth: preprocessed_request.new_auth.clone(),
        }
    }
}

#[async_trait]
impl AsyncJsonProcessor<RequestProcessingContext> for RequestProcessor {
    async fn process(
        &self,
        json_context: &JsonProcessingContext,
        value: &mut Value,
        context: &RequestProcessingContext,
    ) -> JsonProcessingResult {
        let mut result = JsonProcessingResult::new();
        // Check if this is an ID field (case-insensitive)
        if ID_FIELDS.contains(&json_context.key) {
            if let Value::String(ref virtual_id) = value {
                if let Some(media_mapping) = self
                    .state
                    .media_storage
                    .get_media_mapping_by_virtual(virtual_id)
                    .await
                    .unwrap_or_default()
                {
                    info!(
                        "Replacing virtual id  {} -> {} for field: {} in payload",
                        virtual_id, &media_mapping.original_media_id, &json_context.key
                    );
                    *value = Value::String(media_mapping.original_media_id);
                    result = result.mark_modified();
                }
                // For r equests, we need to convert virtual IDs back to real IDs
            }
        }
        // Handle session IDs that might need transformation
        else if SESSION_FIELDS.contains(&json_context.key) {
            // For requests, session IDs typically stay as-is
        }
        // Handle user IDs
        else if USER_FIELDS.contains(&json_context.key) {
            if let Value::String(ref virtual_id) = value {
                // For requests, we need to convert virtual IDs back to real IDs
                if let Some(session) = &context.session {
                    info!(
                        "Replacing User ID {} -> {} for field: {} in payload",
                        virtual_id, &session.original_user_id, &json_context.key
                    );
                    *value = Value::String(session.original_user_id.clone());
                }
            }
        }
        // Handle any other request-specific transformations
        else {
            // Handle any other request-specific transformations
        }

        result
    }
}
