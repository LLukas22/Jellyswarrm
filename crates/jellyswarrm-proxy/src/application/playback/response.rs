//! Translate and validate PlaybackInfo media sources before publishing retained bindings.
use crate::application::error::ApplicationError;
use crate::application::services::MediaServices;
use tracing::info;

#[cfg(test)]
use crate::test_support::MediaFixture;
use crate::{
    application::upstream::response_json_to_payload,
    auth::service::AuthorizationSession,
    models::{MediaSource, PlaybackRequest, PlaybackResponse},
    processors::response_processor::ResponseProcessingProfile,
    servers::storage::Server,
};

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

pub(crate) async fn remap_playback_request(
    payload: &mut PlaybackRequest,
    state: &MediaServices,
    session: &AuthorizationSession,
    server: &Server,
) -> Result<(), ApplicationError> {
    if payload.user_id.is_some() {
        payload.user_id = Some(session.original_user_id.clone());
    }

    if let Some(media_source_id) = &payload.media_source_id {
        if let Some(media_mapping) = state
            .media_storage
            .get_media_mapping_by_virtual(media_source_id)
            .await
            .map_err(ApplicationError::Storage)?
        {
            payload.media_source_id = Some(media_mapping.original_media_id);
        }
    }

    for (key, value) in &mut payload.extra {
        if key.eq_ignore_ascii_case("LiveStreamId") {
            if let Some(id) = value.as_str().filter(|id| !id.is_empty()) {
                let binding = state
                    .play_sessions
                    .live_binding(&id.into(), &session.user_id.as_str().into())
                    .await
                    .ok_or(ApplicationError::InvalidInput("unknown live identity"))?;
                if binding.server_id != server.id {
                    return Err(ApplicationError::InvalidInput(
                        "live identity belongs to another backend",
                    ));
                }
                *value = serde_json::Value::String(binding.original_id.as_str().to_owned());
            }
        }
    }
    Ok(())
}

pub(crate) async fn process_playback_response(
    response: &mut PlaybackResponse,
    state: &MediaServices,
    server: &Server,
    session: &AuthorizationSession,
    proxy_api_key: Option<&str>,
) -> Result<Vec<std::sync::Arc<crate::playback::PreparedLiveStream>>, ApplicationError> {
    let mut plans = Vec::new();
    for source in &mut response.media_sources {
        if let Some(plan) = crate::application::playback::live::prepare_live_source(
            &state.play_sessions,
            source,
            &session.user_id,
            server,
        )
        .await?
        {
            plans.push(plan);
        }
    }
    let mut response_json =
        serde_json::to_value(&*response).map_err(ApplicationError::Serialization)?;

    state
        .process_response_json(
            &mut response_json,
            server,
            ResponseProcessingProfile::Media,
            proxy_api_key,
        )
        .await?;

    *response = response_json_to_payload(response_json)?;

    Ok(plans)
}

pub(crate) async fn track_play_session(
    item: &MediaSource,
    session_id: &str,
    user_id: &str,
    server: &Server,
    state: &MediaServices,
) {
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
    state: &MediaServices,
) {
    info!(
        "Tracking play session for item: {}, server: {}",
        item_id, server.name
    );
    super::lifecycle::PlaybackLifecycle::new(&state.play_sessions)
        .alias(item_id, session_id, user_id, server.id)
        .await;
}

/// Associates an aggregate client-facing item ID with the upstream play
/// session selected during playback negotiation. Subsequent stream requests
/// can then route by `(PlaySessionId, aggregate item id)` even though the
/// aggregate ID does not exist on any backend.
pub(crate) async fn track_playback_alias(
    item_id: &str,
    session_id: &str,
    user_id: &str,
    server: &Server,
    state: &MediaServices,
) {
    add_tracked_play_session(item_id, session_id, user_id, server, state).await;
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

    use serde_json::json;
    use sqlx::SqlitePool;

    use super::*;
    use crate::{
        config::{AppConfig, MediaStreamingMode},
        domain::ids::ServerId,
        persistence::MIGRATOR,
        servers::url::ServerUrl,
    };

    async fn create_test_state() -> (MediaFixture, Server) {
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
        (MediaFixture::new(pool, config), server)
    }

    #[tokio::test]
    async fn response_processor_remaps_media_item_fields() {
        let (state, server) = create_test_state().await;
        let original_item_id = "11111111111111111111111111111111";
        let original_source_id = "22222222222222222222222222222222";
        let original_album_id = "23232323232323232323232323232323";
        let original_person_id = "18181818181818181818181818181818";
        let mut media_item = json!({
            "Id": original_item_id,
            "AlbumId": original_album_id,
            "Type": "Audio",
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
                Some("proxy-token"),
            )
            .await
            .unwrap();

        assert!(was_modified);
        assert_ne!(media_item["Id"].as_str(), Some(original_item_id));
        assert!(
            state
                .client_sessions
                .media_metadata("proxy-token", media_item["Id"].as_str().unwrap())
                .await
                .is_none(),
            "translation must not cache intermediate metadata"
        );
        let virtual_album_id = media_item["AlbumId"].as_str().unwrap();
        assert_ne!(virtual_album_id, original_album_id);
        let album_mapping = state
            .media_storage
            .get_media_mapping_by_virtual(virtual_album_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(album_mapping.original_media_id, original_album_id);
        assert_eq!(album_mapping.server_id, server.id);
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
    async fn item_name_policy_preserves_titles_or_decorates_only_listing_items() {
        use crate::media::presentation::ItemNamePolicy;
        let (state, server) = create_test_state().await;
        let title = format!("Original title [{}]", server.name);
        for enabled in [false, true] {
            state.config.write().await.include_server_name_in_media = enabled;
            for policy in [ItemNamePolicy::Preserve, ItemNamePolicy::IncludeServerName] {
                let mut payload = json!({"Items": [
                    {"Id": "movie", "Type": "Movie", "Name": title,
                     "People": [{"Id": "actor", "Name": "Actor", "Type": "Actor"}],
                     "MediaSources": [{"Id": "version", "Name": "Version", "Type": "Default"}]},
                    {"Id": "live", "Type": "CollectionFolder", "CollectionType": "livetv", "Name": "Live TV"}
                ]});
                state
                    .process_response_json(
                        &mut payload,
                        &server,
                        ResponseProcessingProfile::Media,
                        None,
                    )
                    .await
                    .unwrap();
                let mut items: crate::models::ItemsResponseWithCount =
                    serde_json::from_value(json!({
                        "Items": payload["Items"], "TotalRecordCount": 2, "StartIndex": 0
                    }))
                    .unwrap();
                if enabled {
                    for item in &mut items.items {
                        policy.apply(item, &server.name);
                    }
                }
                payload = serde_json::to_value(items).unwrap();
                let expected = if enabled && policy == ItemNamePolicy::IncludeServerName {
                    format!("{title} [{}]", server.name)
                } else {
                    title.clone()
                };
                assert_eq!(payload["Items"][0]["Name"], expected);
                assert_eq!(payload["Items"][0]["People"][0]["Name"], "Actor");
                assert_eq!(payload["Items"][0]["MediaSources"][0]["Name"], "Version");
                assert_eq!(payload["Items"][1]["Name"], "Live TV");
            }
        }
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
    async fn response_processor_removes_upstream_key_when_proxy_key_is_unavailable() {
        let (state, server) = create_test_state().await;
        let mut payload = json!({
            "DeliveryUrl": "/Videos/item/stream?api_key=upstream-token&Static=true"
        });

        state
            .process_response_json(
                &mut payload,
                &server,
                ResponseProcessingProfile::Media,
                None,
            )
            .await
            .unwrap();

        assert_eq!(payload["DeliveryUrl"], "/Videos/item/stream?Static=true");
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

        track_play_session(&source, "session-1", "user-1", &server, &state).await;

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

        track_play_session(&source, "response-session", "user-1", &server, &state).await;

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
