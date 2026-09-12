use async_trait::async_trait;
use serde_json::Value;
use tracing::debug;

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
                let processor =
                    crate::processors::url_processor::UrlProcessor::new(self.data_context.clone());
                match processor
                    .upstream_user_id(virtual_id, &context.session, context.server.id)
                    .await
                {
                    Ok(upstream) => {
                        *value = Value::String(upstream);
                        result = result.mark_modified();
                    }
                    Err(error) => return result.add_error(error.to_string()),
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
    async fn playlist_lifecycle_translation_and_mixed_server_rejection() {
        use crate::processors::{
            analyze_json,
            request_analyzer::{
                RequestAnalysisContext, RequestAnalyzer, RequestBodyAnalysisResult,
            },
            response_processor::{
                ResponseProcessingContext, ResponseProcessingProfile, ResponseProcessor,
            },
            url_processor::UrlProcessor,
        };
        let (data, _) = test_data_context().await;
        let mut servers = Vec::new();
        for name in ["first", "second"] {
            let id = data
                .server_storage
                .add_server(
                    name,
                    &format!("http://{name}.example"),
                    100,
                    MediaStreamingMode::Redirect,
                )
                .await
                .unwrap();
            servers.push(
                data.server_storage
                    .get_server_by_id(id)
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        let server = servers[0].clone();
        let context = RequestProcessingContext {
            user: None,
            server: server.clone(),
            sessions: None,
            auth: None,
            session: None,
            new_auth: None,
        };
        let response_context = ResponseProcessingContext {
            server: server.clone(),
            proxy_server_id: "proxy".into(),
            proxy_api_key: None,
            profile: ResponseProcessingProfile::Media,
            should_change_name: false,
            can_change_item_names: false,
        };
        let responses = ResponseProcessor::new(data.clone());
        let requests = RequestProcessor::new(data.clone());
        let urls = UrlProcessor::new(data.clone());
        // Discover IDs from actual response processing, including two entries for the same song.
        let mut read = json!({"Id":"playlist", "Type":"Playlist", "CanDelete":true,
            "Items":[{"Id":"song", "PlaylistItemId":"entry-1"}, {"Id":"song", "PlaylistItemId":"entry-2"}],
            "Ids":["song", "song"], "EntryIds":["entry-1", "entry-2"]});
        process_json(&mut read, &responses, &response_context)
            .await
            .unwrap();
        assert_eq!(read["CanDelete"], true);
        assert_eq!(read["Items"][0]["Id"], read["Items"][1]["Id"]);
        assert_ne!(read["EntryIds"][0], read["EntryIds"][1]);
        assert_eq!(read["Items"][0]["PlaylistItemId"], read["EntryIds"][0]);
        let mut create = json!({"Ids":read["Ids"]});
        let analysis = analyze_json(
            &create,
            &RequestAnalyzer::new(data.clone()),
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
        let playlist = read["Id"].as_str().unwrap();
        let song = read["Ids"][0].as_str().unwrap();
        let entry1 = read["EntryIds"][0].as_str().unwrap();
        let entry2 = read["EntryIds"][1].as_str().unwrap();
        for (path, expected) in [
            (
                format!("/Playlists/{playlist}/Items"),
                "/Playlists/playlist/Items",
            ),
            (
                format!("/Playlists/{playlist}/Items?Ids={song},{song}"),
                "/Playlists/playlist/Items?Ids=song%2Csong",
            ),
            (
                format!("/Playlists/{playlist}/Items?EntryIds={entry1},{entry2}"),
                "/Playlists/playlist/Items?EntryIds=entry-1%2Centry-2",
            ),
            (
                format!("/Playlists/{playlist}/Items/{entry2}/Move/0"),
                "/Playlists/playlist/Items/entry-2/Move/0",
            ),
            (format!("/Items/{playlist}"), "/Items/playlist"),
        ] {
            let mut url = url::Url::parse(&format!("http://localhost{path}")).unwrap();
            urls.validate_playlist_url(&url, &None, None, server.id)
                .await
                .unwrap();
            urls.client_to_server_url(&mut url, &None, None, Some(server.id))
                .await;
            assert_eq!(
                url.as_str().trim_end_matches('?'),
                format!("http://localhost{expected}")
            );
        }
        let mut remove = json!({"EntryIds":read["EntryIds"], "PlaylistItemIds":read["EntryIds"]});
        process_json(&mut remove, &requests, &context)
            .await
            .unwrap();
        assert_eq!(
            remove,
            json!({"EntryIds":["entry-1", "entry-2"], "PlaylistItemIds":["entry-1", "entry-2"]})
        );
        let foreign = data
            .media_storage
            .get_or_create_media_mapping("foreign-song", &servers[1])
            .await
            .unwrap()
            .virtual_media_id;
        for id in [&foreign, "unknown"] {
            let mut body = json!({"Ids":[song, id]});
            assert!(process_json(&mut body, &requests, &context)
                .await
                .unwrap_err()
                .to_string()
                .contains("mixed-server"));
            for path in [
                format!("/Playlists?Ids={song},{id}"),
                format!("/Playlists/{playlist}/Items?Ids={id}"),
            ] {
                let url = url::Url::parse(&format!("http://localhost{path}")).unwrap();
                assert!(urls
                    .validate_playlist_url(&url, &None, None, server.id)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("mixed-server"));
            }
        }
        let mut movie = json!({"Type":"Movie", "CanDelete":true});
        process_json(&mut movie, &responses, &response_context)
            .await
            .unwrap();
        assert_eq!(movie["CanDelete"], false);
        let mut denied = json!({"Type":"Playlist", "CanDelete":false});
        process_json(&mut denied, &responses, &response_context)
            .await
            .unwrap();
        assert_eq!(denied["CanDelete"], false);
    }

    #[tokio::test]
    async fn sharing_maps_the_recipient_and_rejects_unknown_users() {
        use crate::processors::url_processor::UrlProcessor;
        let (data, _) = test_data_context().await;
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
        let requests = RequestProcessor::new(data.clone());
        let mut body = json!({"UserId": user.id});
        process_json(&mut body, &requests, &context).await.unwrap();
        assert_eq!(body["UserId"], "upstream-recipient");
        let urls = UrlProcessor::new(data.clone());
        let mut url = url::Url::parse(&format!(
            "http://localhost/Playlists/list/Users/{}?UserId={}",
            user.id, user.id
        ))
        .unwrap();
        urls.client_to_server_url(&mut url, &context.session, None, Some(server.id))
            .await;
        assert_eq!(url.path(), "/Playlists/list/Users/upstream-recipient");
        assert_eq!(url.query(), Some("UserId=upstream-recipient"));
        let mut body = json!({"UserId":"unknown"});
        assert!(process_json(&mut body, &requests, &context)
            .await
            .unwrap_err()
            .to_string()
            .contains("no mapping"));
        assert!(urls
            .upstream_user_id("unknown", &context.session, server.id)
            .await
            .is_err());
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
        let mut payload = json!({ "ItemId": aggregate_id, "Ids": [aggregate_id] });

        let response = process_json(&mut payload, &processor, &context)
            .await
            .unwrap();

        assert!(response.was_modified);
        assert_eq!(payload["ItemId"], "upstream-item");
        assert_eq!(payload["Ids"], json!(["upstream-item"]));
    }
}
