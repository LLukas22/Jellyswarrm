use std::collections::HashMap;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use hyper::StatusCode;
use reqwest::header::{HeaderValue, CONTENT_LENGTH, TRANSFER_ENCODING};
use serde::Serialize;
use tracing::{error, info};

use crate::models::enums::CollectionType;
use crate::{
    media_storage_service::MediaStorageService,
    models::{ItemsResponseVariants, MediaItem, MediaSource, PlaybackRequest, PlaybackResponse},
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

pub async fn get_virtual_id(
    id: &str,
    media_storage: &MediaStorageService,
    server: &Server,
) -> Result<String, StatusCode> {
    let mapping = media_storage
        .get_or_create_media_mapping(id, server)
        .await
        .map_err(|e| {
            error!(
                "Failed to get virtual id for: `{}` on server: {}!/n Error: {}",
                id, server.name, e
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(mapping.virtual_media_id.clone())
}

/// Processes a media item.
/// Replaces the original ids with vitual ids that map back to the original media item and server.
pub async fn process_media_item(
    item: MediaItem,
    state: &AppState,
    server: &Server,
    should_change_name: bool,
    server_id: &str,
) -> Result<MediaItem, StatusCode> {
    let mut item = item;

    let media_storage = &state.media_storage;

    let allowed_to_change_name = state.can_change_item_names().await;

    let can_change_name = if let Some(ref collection_type) = item.collection_type {
        match collection_type {
            CollectionType::LiveTv => false,
            _ => allowed_to_change_name,
        }
    } else {
        allowed_to_change_name
    };

    if can_change_name && should_change_name {
        if let Some(name) = &item.name {
            item.name = Some(format!("{} [{}]", name, server.name));
        }

        if let Some(series_name) = &item.series_name {
            item.series_name = Some(format!("{} [{}]", series_name, server.name));
        }
    }

    item.id = get_virtual_id(&item.id, media_storage, server).await?;
    remap_optional_id(&mut item.parent_id, media_storage, server).await?;
    remap_optional_id(&mut item.item_id, media_storage, server).await?;
    remap_optional_id(&mut item.etag, media_storage, server).await?;
    remap_optional_id(&mut item.series_id, media_storage, server).await?;
    remap_optional_id(&mut item.season_id, media_storage, server).await?;
    remap_optional_id(&mut item.display_preferences_id, media_storage, server).await?;

    if item.can_delete.is_some() {
        item.can_delete = Some(false);
    }

    if item.can_download.is_some() {
        item.can_download = Some(false);
    }

    if let Some(media_sources) = &mut item.media_sources {
        for source in media_sources.iter_mut() {
            *source = process_media_source(source.clone(), media_storage, server).await?;
        }
    }

    remap_optional_id(&mut item.parent_logo_item_id, media_storage, server).await?;
    remap_optional_id(&mut item.parent_backdrop_item_id, media_storage, server).await?;
    remap_optional_id(&mut item.parent_logo_image_tag, media_storage, server).await?;
    remap_optional_id(&mut item.parent_thumb_item_id, media_storage, server).await?;
    remap_optional_id(&mut item.parent_thumb_image_tag, media_storage, server).await?;
    remap_optional_id(&mut item.series_primary_image_tag, media_storage, server).await?;

    if let Some(image_tags) = &mut item.image_tags {
        remap_map_values(image_tags, media_storage, server).await?;
    }

    if let Some(image_blur_hashes) = &mut item.image_blur_hashes {
        let mut updated_blur_hashes = HashMap::new();
        for (image_type, hash_map) in image_blur_hashes.iter() {
            let mut updated_hash_map = HashMap::new();
            for (hash_id, hash_value) in hash_map.iter() {
                let virtual_id = get_virtual_id(hash_id, media_storage, server).await?;
                updated_hash_map.insert(virtual_id, hash_value.clone());
            }
            updated_blur_hashes.insert(image_type.clone(), updated_hash_map);
        }
        *image_blur_hashes = updated_blur_hashes;
    }

    if let Some(backdrop_image_tags) = &mut item.backdrop_image_tags {
        remap_id_vec(backdrop_image_tags, media_storage, server).await?;
    }

    if let Some(parent_backdrop_image_tags) = &mut item.parent_backdrop_image_tags {
        remap_id_vec(parent_backdrop_image_tags, media_storage, server).await?;
    }

    if let Some(chapters) = &mut item.chapters {
        for chapter in chapters.iter_mut() {
            remap_optional_id(&mut chapter.image_tag, media_storage, server).await?;
        }
    }

    if let Some(people) = &mut item.people {
        for person in people.iter_mut() {
            remap_id(&mut person.id, media_storage, server).await?;
        }
    }

    if let Some(trickplay) = &mut item.trickplay {
        remap_map_keys(trickplay, media_storage, server).await?;
    }

    if item.server_id.is_some() {
        item.server_id = Some(server_id.to_string());
    }

    Ok(item)
}

async fn remap_id(
    value: &mut String,
    media_storage: &MediaStorageService,
    server: &Server,
) -> Result<(), StatusCode> {
    let original = value.clone();
    *value = get_virtual_id(&original, media_storage, server).await?;

    Ok(())
}

async fn remap_optional_id(
    value: &mut Option<String>,
    media_storage: &MediaStorageService,
    server: &Server,
) -> Result<(), StatusCode> {
    if let Some(id) = value.clone() {
        *value = Some(get_virtual_id(&id, media_storage, server).await?);
    }

    Ok(())
}

async fn remap_id_vec(
    values: &mut [String],
    media_storage: &MediaStorageService,
    server: &Server,
) -> Result<(), StatusCode> {
    for value in values {
        let original = value.clone();
        *value = get_virtual_id(&original, media_storage, server).await?;
    }

    Ok(())
}

async fn remap_map_values(
    values: &mut HashMap<String, String>,
    media_storage: &MediaStorageService,
    server: &Server,
) -> Result<(), StatusCode> {
    for value in values.values_mut() {
        let original = value.clone();
        *value = get_virtual_id(&original, media_storage, server).await?;
    }

    Ok(())
}

async fn remap_map_keys<T>(
    values: &mut HashMap<String, T>,
    media_storage: &MediaStorageService,
    server: &Server,
) -> Result<(), StatusCode> {
    let old_values = std::mem::take(values);
    for (key, value) in old_values {
        values.insert(get_virtual_id(&key, media_storage, server).await?, value);
    }

    Ok(())
}

pub async fn process_items_response(
    response: &mut ItemsResponseVariants,
    state: &AppState,
    server: &Server,
    should_change_name: bool,
    server_id: &str,
) -> Result<(), StatusCode> {
    for item in response.iter_mut_items() {
        *item =
            process_media_item(item.clone(), state, server, should_change_name, server_id).await?;
    }

    Ok(())
}

pub async fn process_media_items(
    response: &mut [MediaItem],
    state: &AppState,
    server: &Server,
    should_change_name: bool,
    server_id: &str,
) -> Result<(), StatusCode> {
    for item in response {
        *item =
            process_media_item(item.clone(), state, server, should_change_name, server_id).await?;
    }

    Ok(())
}

pub async fn process_media_source(
    item: MediaSource,
    media_storage: &MediaStorageService,
    server: &Server,
) -> Result<MediaSource, StatusCode> {
    let mut item = item;

    item.id = get_virtual_id(&item.id, media_storage, server).await?;
    // TODO check media streams

    Ok(item)
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
    user_id: &str,
) -> Result<(), StatusCode> {
    for item in &mut response.media_sources {
        *item = process_media_source(item.clone(), &state.media_storage, server).await?;
        track_play_session(item, &response.play_session_id, user_id, server, state).await?;
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
    if let Some(transcoding_url) = &item.transcoding_url {
        let id = transcoding_url
            .split("/videos/")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .unwrap_or_default();
        info!(
            "Tracking play session for item: {}, server: {}",
            id, server.name
        );
        state
            .play_sessions
            .add_session(PlaybackSession {
                item_id: id.to_string(),
                session_id: session_id.to_string(),
                user_id: user_id.to_string(),
                server_id: server.id,
            })
            .await;
    } else {
        // Some clients can not set the transcoding url, track by it then
        info!(
            "Tracking play session for media source: {}, server: {}",
            item.id, server.name
        );
        state
            .play_sessions
            .add_session(PlaybackSession {
                item_id: item.id.clone(),
                session_id: session_id.to_string(),
                user_id: user_id.to_string(),
                server_id: server.id,
            })
            .await;
    }

    Ok(())
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
        processors::{request_analyzer::RequestAnalyzer, request_processor::RequestProcessor},
        server_id::ServerId,
        server_storage::ServerStorageService,
        server_url::ServerUrl,
        session_storage::SessionStorage,
        user_authorization_service::UserAuthorizationService,
        DataContext, JsonProcessors,
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

        let data_context = DataContext {
            user_authorization: Arc::new(UserAuthorizationService::new(pool.clone())),
            server_storage: Arc::new(ServerStorageService::new(pool.clone())),
            media_storage: Arc::new(MediaStorageService::new(pool)),
            play_sessions: Arc::new(SessionStorage::new()),
            config: Arc::new(tokio::sync::RwLock::new(AppConfig::default())),
        };

        let processors = JsonProcessors {
            request_processor: RequestProcessor::new(data_context.clone()),
            request_analyzer: RequestAnalyzer::new(data_context.clone()),
        };

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
    async fn process_media_item_remaps_nested_people_ids() {
        let (state, server) = create_test_state().await;
        let original_person_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let item: MediaItem = serde_json::from_value(json!({
            "Id": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "Type": "Movie",
            "Name": "Movie With People",
            "People": [
                {
                    "Name": "Actor",
                    "Id": original_person_id,
                    "Type": "Actor",
                    "PrimaryImageTag": "person-image-tag"
                }
            ]
        }))
        .unwrap();

        let processed = process_media_item(item, &state, &server, false, "proxy-server")
            .await
            .unwrap();
        let person = processed
            .people
            .as_ref()
            .and_then(|people| people.first())
            .unwrap();

        assert_ne!(person.id, original_person_id);
        let mapping = state
            .media_storage
            .get_media_mapping_by_virtual(&person.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(mapping.original_media_id, original_person_id);
        assert_eq!(mapping.server_id, server.id);
    }
}
