use async_trait::async_trait;
use serde_json::Value;
use tracing::{debug, info};

use crate::processors::field_matcher::{
    ID_FIELDS, MEDIA_ID_LIST_PARENT_FIELDS, SESSION_FIELDS, USER_FIELDS,
};
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

    async fn upstream_media_id(
        &self,
        virtual_id: &str,
        server: &Server,
    ) -> Result<Option<String>, sqlx::Error> {
        if let Some(mapping) = self
            .data_context
            .media_storage
            .get_media_mapping_by_virtual(virtual_id)
            .await?
        {
            return Ok((mapping.server_id == server.id).then_some(mapping.original_media_id));
        }

        Ok(self
            .data_context
            .media_storage
            .get_media_version_members_by_virtual_id(virtual_id)
            .await?
            .into_iter()
            .find(|member| member.mapping.server_id == server.id)
            .map(|member| member.mapping.original_media_id))
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
        // Media ID lists (e.g. `Ids`, `EntryIds`) carry one ID per array
        // item, where the key is the array index. Match on the parent field.
        if json_context.is_array_item
            && MEDIA_ID_LIST_PARENT_FIELDS.contains(last_segment(&json_context.parent_path))
        {
            if let Value::String(ref virtual_id) = value {
                let original_media_id =
                    match self.upstream_media_id(virtual_id, &context.server).await {
                        Ok(id) => id,
                        Err(error) => {
                            return result.add_error(format!(
                                "Media ID lookup failed at {}: {}",
                                json_context.path, error
                            ));
                        }
                    };
                if let Some(original_media_id) = original_media_id {
                    debug!(
                        "Replacing virtual id {} -> {} for list field: {} in payload",
                        virtual_id, original_media_id, &json_context.parent_path
                    );
                    *value = Value::String(original_media_id);
                    result = result.mark_modified();
                }
            }
            return result;
        }
        // Check if this is an ID field (case-insensitive)
        if ID_FIELDS.contains(&json_context.key) {
            if let Value::String(ref virtual_id) = value {
                let original_media_id =
                    match self.upstream_media_id(virtual_id, &context.server).await {
                        Ok(id) => id,
                        Err(error) => {
                            return result.add_error(format!(
                                "Media ID lookup failed at {}: {}",
                                json_context.path, error
                            ));
                        }
                    };
                if let Some(original_media_id) = original_media_id {
                    debug!(
                        "Replacing virtual id  {} -> {} for field: {} in payload",
                        virtual_id, original_media_id, &json_context.key
                    );
                    *value = Value::String(original_media_id);
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

fn last_segment(path: &str) -> &str {
    path.rsplit('.')
        .next()
        .map(|segment| segment.split('[').next().unwrap_or(segment))
        .unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, sync::Arc};

    use serde_json::json;
    use sqlx::SqlitePool;

    use super::*;
    use crate::{
        config::{AppConfig, MediaStreamingMode, MIGRATOR},
        media_identity::{MediaAlias, MediaKind, MediaObservation, MediaProvider},
        media_storage_service::{MediaCatalogSnapshot, MediaStorageService},
        processors::process_json,
        server_id::ServerId,
        server_storage::ServerStorageService,
        server_url::ServerUrl,
        session_storage::SessionStorage,
        user_authorization_service::{Device, UserAuthorizationService},
        virtual_library_service::VirtualLibraryService,
    };

    async fn test_data_context() -> (DataContext, SqlitePool) {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let server_storage = ServerStorageService::new(pool.clone());
        let media_storage = MediaStorageService::new(pool.clone());

        let data_context = DataContext {
            user_authorization: Arc::new(UserAuthorizationService::new(pool.clone())),
            server_storage: Arc::new(server_storage.clone()),
            media_storage: Arc::new(media_storage.clone()),
            virtual_library_service: Arc::new(VirtualLibraryService::new(
                pool.clone(),
                server_storage,
                media_storage,
            )),
            play_sessions: Arc::new(SessionStorage::new()),
            config: Arc::new(tokio::sync::RwLock::new(AppConfig::default())),
        };
        (data_context, pool)
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
        let processor = RequestProcessor::new(test_data_context().await.0);
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

    #[tokio::test]
    async fn missing_media_mapping_is_not_a_database_failure() {
        let (data_context, pool) = test_data_context().await;
        let processor = RequestProcessor::new(data_context);
        let context = RequestProcessingContext {
            user: None,
            server: test_server(),
            sessions: None,
            auth: None,
            session: None,
            new_auth: None,
        };
        let mut payload = json!({ "ItemId": "missing-id" });
        let original = payload.clone();

        assert_eq!(
            processor
                .upstream_media_id("missing-id", &context.server)
                .await
                .unwrap(),
            None
        );
        let response = process_json(&mut payload, &processor, &context)
            .await
            .unwrap();
        assert!(!response.was_modified);
        assert_eq!(payload, original);

        pool.close().await;

        assert!(matches!(
            processor
                .upstream_media_id("missing-id", &context.server)
                .await,
            Err(sqlx::Error::PoolClosed)
        ));
        let error = process_json(&mut payload, &processor, &context)
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("Media ID lookup failed at ItemId"));
        assert!(error
            .to_string()
            .contains(&sqlx::Error::PoolClosed.to_string()));
        assert_eq!(payload, original);
    }

    #[tokio::test]
    async fn aggregate_item_id_is_rewritten_for_the_selected_server() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let server_storage = ServerStorageService::new(pool.clone());
        let server_id = server_storage
            .add_server(
                "Server",
                "http://server.example:8096",
                100,
                MediaStreamingMode::Redirect,
            )
            .await
            .unwrap();
        let server = server_storage
            .get_server_by_id(server_id)
            .await
            .unwrap()
            .unwrap();
        let media_storage = MediaStorageService::new(pool.clone());
        let mapping = media_storage
            .get_or_create_media_mapping("upstream-item", &server)
            .await
            .unwrap();
        let alias = MediaAlias {
            provider: MediaProvider::Tmdb,
            kind: MediaKind::Movie,
            provider_id: "42".to_string(),
        };
        let generation = media_storage.begin_media_reconciliation().await.unwrap();
        let aggregate_id = media_storage
            .reconcile_media_catalog(
                "configured:library:user",
                generation,
                &[MediaCatalogSnapshot {
                    source_key: "server:library".to_string(),
                    server_id: server.id,
                    complete: true,
                    observations: vec![MediaObservation {
                        virtual_media_id: mapping.virtual_media_id.clone(),
                        aliases: BTreeSet::from([alias]),
                    }],
                }],
                true,
            )
            .await
            .unwrap()
            .remove(&mapping.virtual_media_id)
            .unwrap()
            .virtual_media_id;
        let virtual_libraries =
            VirtualLibraryService::new(pool.clone(), server_storage.clone(), media_storage.clone());
        let processor = RequestProcessor::new(DataContext {
            user_authorization: Arc::new(UserAuthorizationService::new(pool)),
            server_storage: Arc::new(server_storage),
            media_storage: Arc::new(media_storage),
            virtual_library_service: Arc::new(virtual_libraries),
            play_sessions: Arc::new(SessionStorage::new()),
            config: Arc::new(tokio::sync::RwLock::new(AppConfig::default())),
        });
        let context = RequestProcessingContext {
            user: None,
            server,
            sessions: None,
            auth: None,
            session: None,
            new_auth: None,
        };
        let mut payload = json!({ "ItemId": aggregate_id });

        let response = process_json(&mut payload, &processor, &context)
            .await
            .unwrap();

        assert!(response.was_modified);
        assert_eq!(payload["ItemId"], "upstream-item");
    }
}
