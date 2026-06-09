use async_trait::async_trait;
use serde_json::Value;
use tracing::{debug, info};

use crate::processors::field_matcher::{ID_FIELDS, SESSION_FIELDS, USER_FIELDS};
use crate::processors::json_processor::{
    JsonProcessingContext, JsonProcessingResult, JsonProcessor,
};
use crate::request_preprocessing::{JellyfinAuthorization, PreprocessedRequest};
use crate::server_storage::Server;
use crate::user_authorization_service::{AuthorizationSession, User};
use crate::DataContext;

pub struct RequestProcessor {
    pub data_context: DataContext,
}

impl RequestProcessor {
    pub fn new(data_context: DataContext) -> Self {
        Self { data_context }
    }
}

#[allow(dead_code)]
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
impl JsonProcessor<RequestProcessingContext> for RequestProcessor {
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
                    .data_context
                    .media_storage
                    .get_media_mapping_by_virtual(virtual_id)
                    .await
                    .unwrap_or_default()
                {
                    debug!(
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
                    result = result.mark_modified();
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use sqlx::SqlitePool;

    use super::*;
    use crate::{
        config::{AppConfig, MediaStreamingMode, MIGRATOR},
        media_storage_service::MediaStorageService,
        merged_library_service::MergedLibraryService,
        library_group_service::LibraryGroupService,
        processors::process_json,
        server_id::ServerId,
        server_storage::ServerStorageService,
        server_url::ServerUrl,
        session_storage::SessionStorage,
        user_authorization_service::{Device, UserAuthorizationService},
    };

    async fn test_data_context() -> DataContext {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();

        DataContext {
            user_authorization: Arc::new(UserAuthorizationService::new(pool.clone())),
            server_storage: Arc::new(ServerStorageService::new(pool.clone())),
            media_storage: Arc::new(MediaStorageService::new(pool.clone())),
            merged_library_service: Arc::new(MergedLibraryService::new(pool.clone())),
            library_group_service: Arc::new(LibraryGroupService::new(pool)),
            play_sessions: Arc::new(SessionStorage::new()),
            config: Arc::new(tokio::sync::RwLock::new(AppConfig::default())),
        }
    }

    fn test_server() -> Server {
        let now = chrono::Utc::now();
        Server {
            id: ServerId::new(1),
            name: "Test Server".to_string(),
            url: ServerUrl::parse("http://server.example:8096").unwrap(),
            priority: 0,
            media_streaming_mode: MediaStreamingMode::Redirect,
            created_at: now,
            updated_at: now,
        }
    }

    fn test_session() -> AuthorizationSession {
        let now = chrono::Utc::now();
        AuthorizationSession {
            id: 1,
            user_id: "proxy-user".to_string(),
            mapping_id: 1,
            server_url: "http://server.example:8096".to_string(),
            device: Device {
                client: "Test".to_string(),
                device: "Test Device".to_string(),
                device_id: "device-id".to_string(),
                version: "1".to_string(),
            },
            jellyfin_token: "server-token".to_string(),
            original_user_id: "upstream-user".to_string(),
            expires_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn user_id_rewrite_marks_request_body_modified() {
        let processor = RequestProcessor::new(test_data_context().await);
        let context = RequestProcessingContext {
            user: None,
            server: test_server(),
            sessions: None,
            auth: None,
            session: Some(test_session()),
            new_auth: None,
        };
        let mut payload = json!({ "UserId": "proxy-user" });

        let response = process_json(&mut payload, &processor, &context)
            .await
            .unwrap();

        assert!(response.was_modified);
        assert_eq!(payload["UserId"], "upstream-user");
    }
}
