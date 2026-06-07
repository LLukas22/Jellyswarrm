use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use hyper::StatusCode;
use reqwest::header::{HeaderValue, CONTENT_LENGTH, TRANSFER_ENCODING};
use serde::Serialize;
use tracing::{error, info};

use crate::{
    models::{MediaSource, PlaybackRequest, PlaybackResponse},
    processors::response_processor::ResponseProcessingProfile,
    server_storage::Server,
    session_storage::PlaybackSession,
    user_authorization_service::AuthorizationSession,
    AppState,
};

pub fn payload_from_request<T>(request: &reqwest::Request) -> Result<T, StatusCode>
where
    T: serde::de::DeserializeOwned,
{
    let bytes = request
        .body()
        .ok_or(StatusCode::BAD_REQUEST)?
        .as_bytes()
        .ok_or(StatusCode::BAD_REQUEST)?;
    match serde_json::from_slice::<T>(bytes) {
        Ok(val) => Ok(val),
        Err(e) => {
            if let Ok(body_str) = std::str::from_utf8(bytes) {
                error!("Failed to parse JSON body: {e}\nBody: {body_str}");
            } else {
                error!("Failed to parse JSON body: {e}\nBody (non-UTF8)");
            }
            Err(StatusCode::BAD_REQUEST)
        }
    }
}

pub fn set_json_body<T>(request: &mut reqwest::Request, payload: &T) -> Result<(), StatusCode>
where
    T: Serialize,
{
    let json = serde_json::to_vec(payload).map_err(|e| {
        error!("Failed to serialize JSON request body: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let len = json.len();
    *request.body_mut() = Some(reqwest::Body::from(json));
    request.headers_mut().insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&len.to_string()).map_err(|e| {
            error!("Failed to build Content-Length header: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?,
    );
    request.headers_mut().remove(TRANSFER_ENCODING);

    Ok(())
}

/// Execute a reqwest request and parse the JSON response with comprehensive error handling
pub async fn execute_json_request<T>(
    client: &reqwest::Client,
    request: reqwest::Request,
) -> Result<T, StatusCode>
where
    T: serde::de::DeserializeOwned,
{
    let response = client
        .execute(request)
        .await
        .map_err(|e| {
            error!("Failed to execute request: {}", e);
            StatusCode::BAD_GATEWAY
        })?
        .error_for_status()
        .map_err(|e| {
            error!("Request failed with status: {}", e);
            StatusCode::UNAUTHORIZED
        })?;

    let response_text = response.text().await.map_err(|e| {
        error!("Failed to get response text: {}", e);
        StatusCode::BAD_GATEWAY
    })?;

    match serde_json::from_str::<T>(&response_text) {
        Ok(val) => Ok(val),
        Err(_original_error) => {
            // First, try to parse as generic JSON to get a pretty-printed version
            let pretty_json = match serde_json::from_str::<serde_json::Value>(&response_text) {
                Ok(val) => {
                    // JSON is structurally valid, pretty-print it
                    match serde_json::to_string_pretty(&val) {
                        Ok(pretty) => pretty,
                        Err(_) => response_text.clone(),
                    }
                }
                Err(_) => {
                    // JSON is completely invalid, use original
                    response_text.clone()
                }
            };

            // Now try to parse the pretty JSON as our target type to get better error info
            let parse_error = match serde_json::from_str::<T>(&pretty_json) {
                Ok(_) => _original_error, // Shouldn't happen, but use original error
                Err(e) => e,
            };

            // Optional file dump in debug builds
            if cfg!(debug_assertions) {
                let dump_dir = crate::config::DATA_DIR.join("json_dumps");
                if fs::create_dir_all(&dump_dir).is_ok() {
                    let ts = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0);
                    let filename = format!(
                        "json_parse_error_{}_{}.json",
                        ts,
                        std::any::type_name::<T>().replace("::", "_")
                    );
                    let path = dump_dir.join(filename);
                    if fs::write(&path, &pretty_json).is_ok() {
                        info!("Debug: JSON dump saved to {:?}", path);
                    }
                }
            }

            let line = parse_error.line();
            let col = parse_error.column();
            if line > 0 || col > 0 {
                let lines: Vec<&str> = pretty_json.lines().collect();
                let line_idx = line.saturating_sub(1);
                let col_idx = col.saturating_sub(1);

                let mut snippet = String::new();
                let context_before = 3;
                let context_after = 3;
                let start_idx = line_idx.saturating_sub(context_before);
                let end_idx = std::cmp::min(lines.len(), line_idx + context_after + 1);

                for i in start_idx..end_idx {
                    let line_num = i + 1;
                    let line_content = lines.get(i).unwrap_or(&"");

                    if i == line_idx {
                        snippet.push_str(&format!(">>> {line_num:>4} | {line_content}\n"));
                        let visible_col = std::cmp::min(col_idx, line_content.chars().count());
                        let spaces = " ".repeat(visible_col);
                        snippet.push_str(&format!("         | {spaces}^ (column {col})\n"));
                    } else {
                        snippet.push_str(&format!("    {line_num:>4} | {line_content}\n"));
                    }
                }

                error!(
                    "JSON parsing failed: {}\nAt line {}, column {}:\n{}",
                    parse_error, line, col, snippet
                );
                return Err(StatusCode::BAD_GATEWAY);
            }

            // Fallback if no line/column info
            error!("JSON parsing failed: {}", parse_error);
            Err(StatusCode::BAD_GATEWAY)
        }
    }
}

pub async fn execute_processed_json_request(
    state: &AppState,
    request: reqwest::Request,
    server: &Server,
    profile: ResponseProcessingProfile,
    should_change_name: bool,
    proxy_api_key: Option<&str>,
) -> Result<serde_json::Value, StatusCode> {
    let mut response = execute_json_request::<serde_json::Value>(&state.reqwest_client, request)
        .await
        .inspect_err(|e| error!("Failed to get upstream JSON: {:?}", e))?;

    state
        .process_response_json(
            &mut response,
            server,
            profile,
            should_change_name,
            proxy_api_key,
        )
        .await?;

    Ok(response)
}

pub fn response_json_to_payload<T>(payload: serde_json::Value) -> Result<T, StatusCode>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(payload).map_err(|e| {
        error!("Failed to deserialize processed response JSON: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })
}

fn parse_delivery_url(value: &str) -> Option<url::Url> {
    if let Ok(url) = url::Url::parse(value) {
        return Some(url);
    }

    let path = if value.starts_with('/') {
        value.to_string()
    } else {
        format!("/{value}")
    };

    url::Url::parse(&format!("http://localhost{path}")).ok()
}

pub async fn remap_playback_request(
    payload: &mut PlaybackRequest,
    state: &AppState,
    session: &AuthorizationSession,
) -> Result<(), StatusCode> {
    if payload.user_id.is_some() {
        payload.user_id = Some(session.original_user_id.clone());
    }

    if let Some(media_source_id) = &payload.media_source_id {
        if let Some(media_mapping) = state
            .media_storage
            .get_media_mapping_by_virtual(media_source_id)
            .await
            .map_err(|e| {
                error!("Failed to resolve media source id mapping: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?
        {
            payload.media_source_id = Some(media_mapping.original_media_id);
        }
    }

    Ok(())
}

pub async fn process_playback_response(
    response: &mut PlaybackResponse,
    state: &AppState,
    server: &Server,
    session: &AuthorizationSession,
) -> Result<(), StatusCode> {
    let proxy_user = state
        .user_authorization
        .get_user_by_id(&session.user_id)
        .await
        .map_err(|e| {
            error!("Failed to resolve proxy user for playback response: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or_else(|| {
            error!(
                "Failed to resolve proxy user {} for playback response",
                session.user_id
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let mut response_json = serde_json::to_value(&*response).map_err(|e| {
        error!("Failed to serialize playback response JSON: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    state
        .process_response_json(
            &mut response_json,
            server,
            ResponseProcessingProfile::Media,
            false,
            Some(proxy_user.virtual_key.as_str()),
        )
        .await?;

    *response = response_json_to_payload(response_json)?;

    for item in &response.media_sources {
        track_play_session(
            item,
            &response.play_session_id,
            &session.user_id,
            server,
            state,
        )
        .await?;
    }

    Ok(())
}

pub async fn track_play_session(
    item: &MediaSource,
    session_id: &str,
    user_id: &str,
    server: &Server,
    state: &AppState,
) -> Result<(), StatusCode> {
    let mut session_ids = vec![session_id.to_string()];
    let mut item_ids = vec![item.id.clone()];

    collect_delivery_url_tracking_values(
        item.transcoding_url.as_deref(),
        &mut session_ids,
        &mut item_ids,
    );
    collect_delivery_url_tracking_values(
        item.stream_url.as_deref(),
        &mut session_ids,
        &mut item_ids,
    );

    if let Some(media_streams) = &item.media_streams {
        for stream in media_streams {
            collect_delivery_url_tracking_values(
                stream.delivery_url.as_deref(),
                &mut session_ids,
                &mut item_ids,
            );
        }
    }

    if let Some(media_attachments) = &item.media_attachments {
        for attachment in media_attachments {
            collect_delivery_url_tracking_values(
                attachment
                    .get("DeliveryUrl")
                    .and_then(serde_json::Value::as_str),
                &mut session_ids,
                &mut item_ids,
            );
        }
    }

    for tracked_session_id in session_ids {
        for item_id in &item_ids {
            add_tracked_play_session(item_id, &tracked_session_id, user_id, server, state).await;
        }
    }

    Ok(())
}

fn collect_delivery_url_tracking_values(
    value: Option<&str>,
    session_ids: &mut Vec<String>,
    item_ids: &mut Vec<String>,
) {
    let Some(value) = value else {
        return;
    };

    if let Some(session_id) = extract_play_session_id_from_delivery_url(value) {
        push_unique(session_ids, session_id);
    }

    if let Some(item_id) = extract_media_id_from_delivery_url(value) {
        push_unique(item_ids, item_id);
    }
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

async fn add_tracked_play_session(
    item_id: &str,
    session_id: &str,
    user_id: &str,
    server: &Server,
    state: &AppState,
) {
    info!(
        "Tracking play session for item: {}, server: {}",
        item_id, server.name
    );
    state
        .play_sessions
        .add_session(PlaybackSession {
            item_id: item_id.to_string(),
            session_id: session_id.to_string(),
            user_id: user_id.to_string(),
            server_id: server.id,
        })
        .await;
}

fn extract_media_id_from_delivery_url(value: &str) -> Option<String> {
    let url = parse_delivery_url(value)?;
    let mut segments = url.path_segments()?;

    while let Some(segment) = segments.next() {
        if segment.eq_ignore_ascii_case("Videos") || segment.eq_ignore_ascii_case("Audio") {
            return segments
                .next()
                .filter(|segment| !segment.is_empty())
                .map(str::to_string);
        }
    }

    None
}

fn extract_play_session_id_from_delivery_url(value: &str) -> Option<String> {
    let url = parse_delivery_url(value)?;
    url.query_pairs().find_map(|(key, value)| {
        (key.eq_ignore_ascii_case("PlaySessionId") || key.eq_ignore_ascii_case("SessionId"))
            .then(|| value.to_string())
            .filter(|value| !value.is_empty())
    })
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
        server_id::ServerId,
        server_storage::ServerStorageService,
        server_url::ServerUrl,
        session_storage::SessionStorage,
        user_authorization_service::UserAuthorizationService,
        DataContext, ProxyProcessors,
    };

    async fn create_test_state() -> (AppState, Server) {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();

        let now = chrono::Utc::now();
        let result = sqlx::query(
            r#"
            INSERT INTO servers (name, url, priority, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind("People Server")
        .bind("http://people.example:8096")
        .bind(100)
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();

        let server = Server {
            id: ServerId::new(result.last_insert_rowid()),
            name: "People Server".to_string(),
            url: ServerUrl::parse("http://people.example:8096").unwrap(),
            priority: 100,
            media_streaming_mode: MediaStreamingMode::Redirect,
            created_at: now,
            updated_at: now,
        };

        let config = AppConfig {
            server_id: "proxy-server".to_string(),
            ..AppConfig::default()
        };

        let data_context = DataContext {
            user_authorization: Arc::new(UserAuthorizationService::new(pool.clone())),
            server_storage: Arc::new(ServerStorageService::new(pool.clone())),
            media_storage: Arc::new(MediaStorageService::new(pool.clone())),
            merged_library_service: Arc::new(MergedLibraryService::new(pool)),
            play_sessions: Arc::new(SessionStorage::new()),
            config: Arc::new(tokio::sync::RwLock::new(config)),
        };

        let processors = ProxyProcessors::new(data_context.clone());

        (
            AppState::new(
                reqwest::Client::new(),
                reqwest::Client::new(),
                data_context,
                processors,
                crate::handlers::quick_connect::QuickConnectStorage::new(),
            ),
            server,
        )
    }

    #[tokio::test]
    async fn response_processor_remaps_media_item_fields() {
        let (state, server) = create_test_state().await;
        let original_item_id = "11111111111111111111111111111111";
        let original_source_id = "22222222222222222222222222222222";
        let original_person_id = "18181818181818181818181818181818";
        let mut media_item = json!({
            "Id": original_item_id,
            "Type": "Movie",
            "Name": "Parity Movie",
            "SeriesName": "Parity Series",
            "ServerId": "upstream-server",
            "ParentId": "33333333333333333333333333333333",
            "ItemId": "44444444444444444444444444444444",
            "Etag": "55555555555555555555555555555555",
            "SeriesId": "66666666666666666666666666666666",
            "SeasonId": "77777777777777777777777777777777",
            "DisplayPreferencesId": "88888888888888888888888888888888",
            "ParentLogoItemId": "99999999999999999999999999999999",
            "ParentBackdropItemId": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "ParentLogoImageTag": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "ParentThumbItemId": "cccccccccccccccccccccccccccccccc",
            "ParentThumbImageTag": "dddddddddddddddddddddddddddddddd",
            "SeriesPrimaryImageTag": "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            "CanDelete": true,
            "CanDownload": true,
            "ImageTags": {
                "Primary": "12121212121212121212121212121212",
                "Logo": "13131313131313131313131313131313"
            },
            "ImageBlurHashes": {
                "Primary": {
                    "14141414141414141414141414141414": "blur-one"
                }
            },
            "BackdropImageTags": ["15151515151515151515151515151515"],
            "ParentBackdropImageTags": ["16161616161616161616161616161616"],
            "Chapters": [
                {"ImageTag": "17171717171717171717171717171717"}
            ],
            "People": [
                {
                    "Name": "Actor",
                    "Id": original_person_id,
                    "Type": "Actor",
                    "PrimaryImageTag": "person-image-tag"
                }
            ],
            "Trickplay": {
                "19191919191919191919191919191919": {"Width": 320}
            },
            "UserData": {
                "PlaybackPositionTicks": 0,
                "PlayCount": 1,
                "IsFavorite": false,
                "Played": false,
                "Key": "userdata-key",
                "ItemId": "20202020202020202020202020202020"
            },
            "MediaSources": [
                {
                    "Id": original_source_id,
                    "Etag": "source-etag-should-stay",
                    "StreamUrl": format!(
                        "/Audio/{}/universal?api_key=upstream-token&MediaSourceId={}",
                        original_item_id,
                        original_source_id
                    ),
                    "TranscodingUrl": format!(
                        "/Videos/{}/master.m3u8?PlaySessionId=session-1&MediaSourceId={}",
                        original_item_id,
                        original_source_id
                    ),
                    "MediaStreams": [
                        {
                            "Index": 3,
                            "Type": "Subtitle",
                            "DeliveryUrl": format!(
                                "/Videos/{}/{}/Subtitles/3/0/Stream.ass?api_key=upstream-token&MediaSourceId={}",
                                original_item_id,
                                original_source_id,
                                original_source_id
                            )
                        }
                    ],
                    "MediaAttachments": [
                        {
                            "DeliveryUrl": format!(
                                "/Videos/{}/{}/Attachments/5?api_key=upstream-token",
                                original_item_id,
                                original_source_id
                            )
                        }
                    ]
                }
            ]
        });

        let was_modified = state
            .process_response_json(
                &mut media_item,
                &server,
                ResponseProcessingProfile::Media,
                false,
                Some("proxy-token"),
            )
            .await
            .unwrap();

        assert!(was_modified);
        assert_ne!(media_item["Id"].as_str(), Some(original_item_id));
        assert_ne!(
            media_item["People"][0]["Id"].as_str(),
            Some(original_person_id)
        );
        assert_eq!(media_item["CanDelete"], false);
        assert_eq!(media_item["CanDownload"], false);
        assert_eq!(media_item["ServerId"], "proxy-server");
        assert_eq!(
            media_item["UserData"]["ItemId"],
            "20202020202020202020202020202020"
        );
        assert_eq!(
            media_item["MediaSources"][0]["Etag"],
            "source-etag-should-stay"
        );
        assert!(media_item["MediaSources"][0]["StreamUrl"]
            .as_str()
            .unwrap()
            .contains("api_key=proxy-token"));
    }

    #[tokio::test]
    async fn response_processor_remaps_top_level_item_arrays() {
        let (state, server) = create_test_state().await;
        let first_id = "31313131313131313131313131313131";
        let person_id = "33333333333333333333333333333333";
        let mut media_items = json!([
            {
                "Id": first_id,
                "Type": "Movie",
                "Name": "First"
            },
            {
                "Id": "32323232323232323232323232323232",
                "Type": "Movie",
                "Name": "Second",
                "People": [
                    {
                        "Name": "Actor",
                        "Id": person_id,
                        "Type": "Actor"
                    }
                ]
            }
        ]);

        let was_modified = state
            .process_response_json(
                &mut media_items,
                &server,
                ResponseProcessingProfile::Media,
                false,
                None,
            )
            .await
            .unwrap();

        assert!(was_modified);
        assert_ne!(media_items[0]["Id"].as_str(), Some(first_id));
        assert_ne!(media_items[1]["People"][0]["Id"].as_str(), Some(person_id));
    }

    #[tokio::test]
    async fn best_effort_response_profile_remaps_media_like_fields() {
        let (state, server) = create_test_state().await;
        let original_item_id = "41414141414141414141414141414141";
        let mut payload = json!({
            "Id": original_item_id,
            "ServerId": "upstream-server",
            "CanDelete": true,
            "DeliveryUrl": format!("/Videos/{original_item_id}/stream?api_key=upstream-token")
        });

        let was_modified = state
            .process_response_json(
                &mut payload,
                &server,
                ResponseProcessingProfile::BestEffortMedia,
                false,
                Some("proxy-token"),
            )
            .await
            .unwrap();

        assert!(was_modified);
        assert_ne!(payload["Id"].as_str(), Some(original_item_id));
        assert_eq!(payload["ServerId"], "proxy-server");
        assert_eq!(payload["CanDelete"], false);
        assert!(payload["DeliveryUrl"]
            .as_str()
            .unwrap()
            .contains("api_key=proxy-token"));
    }

    #[tokio::test]
    async fn response_processor_preserves_non_media_delivery_url_query_params() {
        let (state, server) = create_test_state().await;
        let original_item_id = "71717171717171717171717171717171";
        let original_source_id = "72727272727272727272727272727272";
        let start_position_ticks = "1234567890";
        let play_session_id = "73737373-7373-4737-9373-737373737373";
        let device_id = "74747474-7474-4747-9474-747474747474";
        let mut payload = json!({
            "DeliveryUrl": format!(
                "/Videos/{original_item_id}/{original_source_id}/Subtitles/3/{start_position_ticks}/Stream.ass?api_key=upstream-token&PlaySessionId={play_session_id}&DeviceId={device_id}&MediaSourceId={original_source_id}"
            )
        });

        let was_modified = state
            .process_response_json(
                &mut payload,
                &server,
                ResponseProcessingProfile::Media,
                false,
                Some("proxy-token"),
            )
            .await
            .unwrap();

        assert!(was_modified);

        let remapped_url = payload["DeliveryUrl"].as_str().unwrap();
        let url = url::Url::parse(&format!("http://localhost{remapped_url}")).unwrap();
        let segments = url.path_segments().unwrap().collect::<Vec<_>>();
        assert_eq!(segments[0], "Videos");
        assert_ne!(segments[1], original_item_id);
        assert_ne!(segments[2], original_source_id);
        assert_eq!(segments[3], "Subtitles");
        assert_eq!(segments[4], "3");
        assert_eq!(segments[5], start_position_ticks);
        assert_eq!(segments[6], "Stream.ass");

        let query_pairs = url
            .query_pairs()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            query_pairs.get("api_key").map(String::as_str),
            Some("proxy-token")
        );
        assert_eq!(
            query_pairs.get("PlaySessionId").map(String::as_str),
            Some(play_session_id)
        );
        assert_eq!(
            query_pairs.get("DeviceId").map(String::as_str),
            Some(device_id)
        );
        assert_eq!(
            query_pairs.get("MediaSourceId").map(String::as_str),
            Some(segments[2])
        );
    }

    #[tokio::test]
    async fn track_play_session_tracks_media_source_and_transcoding_url_ids() {
        let (state, server) = create_test_state().await;
        let source: MediaSource = serde_json::from_value(json!({
            "Id": "media-source-id",
            "TranscodingUrl": "/Videos/video-resource-id/master.m3u8?PlaySessionId=session-1"
        }))
        .unwrap();

        track_play_session(&source, "session-1", "user-1", &server, &state)
            .await
            .unwrap();

        let source_session = state
            .play_sessions
            .get_session_by_session_and_item_id("session-1", "media-source-id")
            .await
            .unwrap();
        let video_session = state
            .play_sessions
            .get_session_by_session_and_item_id("session-1", "video-resource-id")
            .await
            .unwrap();

        assert_eq!(source_session.server_id, server.id);
        assert_eq!(video_session.server_id, server.id);
    }

    #[tokio::test]
    async fn track_play_session_tracks_embedded_url_session_ids_and_resource_ids() {
        let (state, server) = create_test_state().await;
        let source: MediaSource = serde_json::from_value(json!({
            "Id": "media-source-id",
            "StreamUrl": "/Audio/audio-resource-id/universal?PlaySessionId=audio-session",
            "MediaStreams": [
                {
                    "Index": 3,
                    "Type": "Subtitle",
                    "DeliveryUrl": "/Videos/subtitle-resource-id/media-source-id/Subtitles/3/0/Stream.ass?PlaySessionId=subtitle-session"
                }
            ],
            "MediaAttachments": [
                {
                    "DeliveryUrl": "/Videos/attachment-resource-id/media-source-id/Attachments/5?SessionId=attachment-session"
                }
            ]
        }))
        .unwrap();

        track_play_session(&source, "response-session", "user-1", &server, &state)
            .await
            .unwrap();

        for session_id in [
            "response-session",
            "audio-session",
            "subtitle-session",
            "attachment-session",
        ] {
            for item_id in [
                "media-source-id",
                "audio-resource-id",
                "subtitle-resource-id",
                "attachment-resource-id",
            ] {
                assert!(
                    state
                        .play_sessions
                        .get_session_by_session_and_item_id(session_id, item_id)
                        .await
                        .is_some(),
                    "missing tracked session {session_id} for item {item_id}"
                );
            }
        }
    }
}
