//! Translate virtual IDs and sharing permissions in outgoing JSON request bodies.
use async_trait::async_trait;
use serde_json::Value;
use tracing::debug;

use super::user_ids::BackendUserIds;
use crate::auth::service::{AuthorizationSession, LocalUser};
use crate::processors::field_matcher::{
    ID_FIELDS, MEDIA_ID_LIST_PARENT_FIELDS, SESSION_FIELDS, USER_FIELDS,
};
use crate::processors::json_processor::{
    JsonProcessingContext, JsonProcessingResult, JsonProcessor,
};
use crate::protocol::identity::JellyfinAuthorization;
use crate::servers::storage::Server;
use crate::{auth::UserAuthorizationService, media::MediaStorageService};
use std::sync::Arc;

pub(crate) struct RequestProcessor {
    media_storage: Arc<MediaStorageService>,
    user_ids: BackendUserIds,
}

impl RequestProcessor {
    pub(crate) fn new(
        media_storage: Arc<MediaStorageService>,
        authorization: Arc<UserAuthorizationService>,
    ) -> Self {
        Self {
            media_storage,
            user_ids: BackendUserIds::new(authorization),
        }
    }

    async fn upstream_media_id(
        &self,
        virtual_id: &str,
        server: &Server,
    ) -> Result<Option<String>, sqlx::Error> {
        if let Some(mapping) = self
            .media_storage
            .get_media_mapping_by_virtual(virtual_id)
            .await?
        {
            return Ok((mapping.server_id == server.id).then_some(mapping.original_media_id));
        }

        Ok(self
            .media_storage
            .get_media_version_members_by_virtual_id(virtual_id)
            .await?
            .into_iter()
            .find(|member| member.mapping.server_id == server.id)
            .map(|member| member.mapping.original_media_id))
    }
}

#[allow(dead_code)]
pub(crate) struct RequestProcessingContext {
    pub(crate) user: Option<LocalUser>,
    pub(crate) server: Server,
    pub(crate) sessions: Option<Vec<(AuthorizationSession, Server)>>,
    pub(crate) auth: Option<JellyfinAuthorization>,
    pub(crate) session: Option<AuthorizationSession>,
    pub(crate) new_auth: Option<JellyfinAuthorization>,
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
                            return result.add_cause(anyhow::Error::new(error).context(format!(
                                "Media ID lookup failed at {}",
                                json_context.path
                            )));
                        }
                    };
                if let Some(original_media_id) = original_media_id {
                    debug!(
                        "Replacing virtual id {} -> {} for list field: {} in payload",
                        virtual_id, original_media_id, &json_context.parent_path
                    );
                    *value = Value::String(original_media_id);
                    result = result.mark_modified();
                } else {
                    return result.add_error("Playlist track or entry is unavailable on the selected server; mixed-server playlists are unsupported".to_string());
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
                            return result.add_cause(anyhow::Error::new(error).context(format!(
                                "Media ID lookup failed at {}",
                                json_context.path
                            )));
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
                match self
                    .user_ids
                    .resolve(virtual_id, context.session.as_ref(), context.server.id)
                    .await
                {
                    Ok(upstream) => {
                        *value = Value::String(upstream);
                        result = result.mark_modified();
                    }
                    Err(error) => return result.add_cause(error.into()),
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
    use crate::{application::services::MediaServices, test_support::MediaFixture};
    use crate::{
        auth::service::UserAuthorizationService,
        config::{AppConfig, MediaStreamingMode},
        domain::ids::ServerId,
        media::identity::{MediaAlias, MediaKind, MediaObservation, MediaProvider},
        media::storage::{MediaCatalogSnapshot, MediaStorageService},
        persistence::MIGRATOR,
        processors::process_json,
        protocol::device::Device,
        servers::storage::ServerStorageService,
        servers::url::ServerUrl,
    };

    async fn processor_fixture() -> (MediaServices, SqlitePool) {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let media = MediaFixture::new(pool.clone(), AppConfig::default()).services;
        (media, pool)
    }

    #[tokio::test]
    async fn array_object_fields_cannot_claim_top_level_playback_authority() {
        use crate::processors::{
            analyze_json,
            request_analyzer::{
                PlaybackSessionAction, RequestAnalysisContext, RequestAnalyzer,
                RequestBodyAnalysisResult,
            },
        };
        let (data, _) = processor_fixture().await;
        let analyzer = RequestAnalyzer::new(
            data.media_storage,
            data.server_storage,
            data.playback_registry,
            data.user_authorization,
        );
        let analyzed = analyze_json(&json!([{"ItemId":"nested-item", "MediaSourceId":"nested-source", "PlaySessionId":"nested-session"}]), &analyzer,
            &RequestAnalysisContext { authenticated_user_id: Some("viewer".into()), playback_session_action: Some(PlaybackSessionAction::Refresh) },
            RequestBodyAnalysisResult::default()).await.unwrap();
        assert!(analyzed.requested_play_item_id.is_none());
        assert!(analyzed.requested_play_source_id.is_none());
        assert!(analyzed.requested_play_session_id.is_none());
        assert!(analyzed.authoritative_play_session.is_none());
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
            user_id: "proxy-user".into(),
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
    async fn body_transformation_preserves_media_list_and_user_storage_causes() {
        let (data, pool) = processor_fixture().await;
        let processors = data.processors;
        let context = RequestProcessingContext {
            user: None,
            server: test_server(),
            sessions: None,
            auth: None,
            session: None,
            new_auth: None,
        };
        pool.close().await;
        for payload in [
            json!({"ItemId": "item"}),
            json!({"Ids": ["item"]}),
            json!({"UserId": "user"}),
        ] {
            let mut request = reqwest::Client::new()
                .post("http://proxy/BodyLookup")
                .json(&payload)
                .build()
                .unwrap();
            let url = request.url().clone();
            let error = processors
                .process_request_body(&mut request, &context, &url)
                .await
                .unwrap_err();
            let crate::processors::ProcessingError::RequestBody(cause) = error else {
                panic!("expected retained processing failure, got {error:?}");
            };
            assert!(crate::processors::ProcessingErrors::contains_cause::<
                sqlx::Error,
            >(cause.as_ref()));
            let fields = cause
                .downcast_ref::<crate::processors::ProcessingErrors>()
                .unwrap();
            assert!(fields.causes.iter().any(|cause| {
                crate::processors::ProcessingErrors::contains_cause::<sqlx::Error>(cause.as_ref())
            }));
        }
    }

    #[tokio::test]
    async fn playlist_body_lists_preserve_duplicates_aliases_and_reject_unavailable_ids() {
        use crate::processors::{
            analyze_json,
            request_analyzer::{
                RequestAnalysisContext, RequestAnalyzer, RequestBodyAnalysisResult,
            },
        };
        let (data, _) = processor_fixture().await;
        let id = data
            .server_storage
            .add_server(
                "first",
                "http://first.example",
                100,
                MediaStreamingMode::Redirect,
            )
            .await
            .unwrap();
        let server = data
            .server_storage
            .get_server_by_id(id)
            .await
            .unwrap()
            .unwrap();
        let context = RequestProcessingContext {
            user: None,
            server: server.clone(),
            sessions: None,
            auth: None,
            session: None,
            new_auth: None,
        };
        let requests =
            RequestProcessor::new(data.media_storage.clone(), data.user_authorization.clone());
        // HTTP tests own lifecycle and URL routing. Exercise body-list rejection
        // directly here because HTTP validation can reject before processing.
        let mut ids = Vec::new();
        for original in ["song", "entry-1", "entry-2"] {
            ids.push(
                data.media_storage
                    .get_or_create_media_mapping(original, &server)
                    .await
                    .unwrap()
                    .virtual_media_id,
            );
        }
        let mut create = json!({"Ids":[ids[0], ids[0]]});
        let analysis = analyze_json(
            &create,
            &RequestAnalyzer::new(
                data.media_storage.clone(),
                data.server_storage.clone(),
                data.playback_registry.clone(),
                data.user_authorization.clone(),
            ),
            &RequestAnalysisContext {
                authenticated_user_id: None,
                playback_session_action: None,
            },
            RequestBodyAnalysisResult::default(),
        )
        .await
        .unwrap();
        assert_eq!(analysis.found_ids.len(), 2);
        assert_eq!(analysis.get_server().unwrap().id, server.id);
        process_json(&mut create, &requests, &context)
            .await
            .unwrap();
        assert_eq!(create, json!({"Ids":["song", "song"]}));
        let mut remove = json!({"EntryIds":[ids[1], ids[2]], "PlaylistItemIds":[ids[1], ids[2]]});
        process_json(&mut remove, &requests, &context)
            .await
            .unwrap();
        assert_eq!(
            remove,
            json!({"EntryIds":["entry-1", "entry-2"], "PlaylistItemIds":["entry-1", "entry-2"]})
        );
        let foreign_server_id = data
            .server_storage
            .add_server(
                "second",
                "http://second.example",
                100,
                MediaStreamingMode::Redirect,
            )
            .await
            .unwrap();
        let foreign_server = data
            .server_storage
            .get_server_by_id(foreign_server_id)
            .await
            .unwrap()
            .unwrap();
        let foreign_id = data
            .media_storage
            .get_or_create_media_mapping("foreign-song", &foreign_server)
            .await
            .unwrap()
            .virtual_media_id;
        for (case, unavailable_id) in [
            ("foreign server", foreign_id.as_str()),
            ("unknown", "unknown"),
        ] {
            let mut body = json!({"Ids":[ids[0], unavailable_id]});
            let error = process_json(&mut body, &requests, &context)
                .await
                .expect_err(case);
            assert!(
                error.to_string().contains("mixed-server"),
                "{case}: {error}"
            );
        }
    }

    #[tokio::test]
    async fn recipient_body_fields_accept_uuid_formats_and_nested_case_aliases() {
        use crate::processors::request_analyzer::{
            RequestAnalysisContext, RequestAnalyzer, RequestBodyAnalysisResult,
        };
        let (data, _) = processor_fixture().await;
        let server_id = data
            .server_storage
            .add_server(
                "first",
                "http://server.example:8096",
                100,
                MediaStreamingMode::Redirect,
            )
            .await
            .unwrap();
        let server = data
            .server_storage
            .get_server_by_id(server_id)
            .await
            .unwrap()
            .unwrap();
        let user = data
            .user_authorization
            .create_user("recipient", &"password".into())
            .await
            .unwrap();
        data.user_authorization
            .add_server_mapping(
                &user.id,
                server.url.as_str(),
                "recipient",
                &"password".into(),
                Some(&user.local_credential.mapping_key()),
            )
            .await
            .unwrap();
        data.user_authorization
            .store_authorization_session(
                &user.id,
                &server,
                &test_session().to_authorization(),
                "recipient-token".into(),
                "upstream-recipient".into(),
                None,
            )
            .await
            .unwrap();
        let context = RequestProcessingContext {
            user: None,
            server: server.clone(),
            sessions: None,
            auth: None,
            session: Some(test_session()),
            new_auth: None,
        };
        let requests =
            RequestProcessor::new(data.media_storage.clone(), data.user_authorization.clone());
        let uuid = uuid::Uuid::parse_str(&user.id).unwrap();
        for id in [
            uuid.simple().to_string(),
            uuid.hyphenated().to_string(),
            uuid.simple().to_string().to_uppercase(),
            uuid.hyphenated().to_string().to_uppercase(),
        ] {
            for key in ["UserId", "userId", "USERID"] {
                let mut body = json!({key: id, "Users": [{key: id}]});
                let analysis = crate::processors::analyze_json(
                    &body,
                    &RequestAnalyzer::new(
                        data.media_storage.clone(),
                        data.server_storage.clone(),
                        data.playback_registry.clone(),
                        data.user_authorization.clone(),
                    ),
                    &RequestAnalysisContext {
                        authenticated_user_id: Some("proxy-user".into()),
                        playback_session_action: None,
                    },
                    RequestBodyAnalysisResult::default(),
                )
                .await
                .unwrap();
                assert_eq!(analysis.users.len(), 2);
                assert!(analysis.users.iter().all(|resolved| resolved.id == user.id));
                process_json(&mut body, &requests, &context).await.unwrap();
                assert_eq!(body[key], "upstream-recipient");
                assert_eq!(body["Users"][0][key], "upstream-recipient");
            }
        }
    }

    #[tokio::test]
    async fn user_id_rewrite_marks_request_body_modified() {
        let data = processor_fixture().await.0;
        let processor = RequestProcessor::new(data.media_storage, data.user_authorization);
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
        let (data_context, pool) = processor_fixture().await;
        let processor =
            RequestProcessor::new(data_context.media_storage, data_context.user_authorization);
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
        let fields = error
            .downcast_ref::<crate::processors::ProcessingErrors>()
            .unwrap();
        assert!(matches!(
            fields.causes[0].downcast_ref::<sqlx::Error>(),
            Some(sqlx::Error::PoolClosed)
        ));
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
                        kind: MediaKind::Movie,
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
        let processor = RequestProcessor::new(
            Arc::new(media_storage),
            Arc::new(UserAuthorizationService::new(pool)),
        );
        let context = RequestProcessingContext {
            user: None,
            server,
            sessions: None,
            auth: None,
            session: None,
            new_auth: None,
        };
        let mut payload = json!({ "ItemId": aggregate_id, "Ids": [aggregate_id] });

        let response = process_json(&mut payload, &processor, &context)
            .await
            .unwrap();

        assert!(response.was_modified);
        assert_eq!(payload["ItemId"], "upstream-item");
        assert_eq!(payload["Ids"], json!(["upstream-item"]));
    }
}
