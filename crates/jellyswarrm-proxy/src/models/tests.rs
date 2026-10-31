//! Verify Jellyfin model serialization and deserialization against representative payloads.
use crate::models::jellyfin::enums::BaseItemKind;
use crate::models::jellyfin::{AuthenticateRequest, MediaItem, MediaSource};
use crate::models::{ItemsResponseWithCount, PlaybackRequest, PlaybackResponse};
use serde_json::{json, Value};
use std::fs;

fn fixture<T: serde::de::DeserializeOwned>(name: &str) -> T {
    let path = format!(
        "{}/src/models/tests/files/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let json = fs::read_to_string(path).unwrap_or_else(|error| panic!("{name}: {error}"));
    serde_json::from_str(&json).unwrap_or_else(|error| panic!("{name}: {error}"))
}

fn live_source_with_extensions() -> Value {
    json!({
        "Id": "backend-source",
        "Type": "Default",
        "LiveStreamId": "backend-live-stream",
        "OpenToken": "opaque-open-token",
        "RequiresOpening": true,
        "RequiresClosing": false,
        "FutureSourceOptions": {
            "Modes": ["direct", "transcode"],
            "Nullable": null,
            "CaseSensitiveKey": "preserve"
        },
        "MediaStreams": [{
            "Index": 0,
            "Type": "Audio",
            "Codec": "aac",
            "FutureStreamMetadata": {
                "Flags": [true, false],
                "VendorData": {"ExactKey": "opaque"}
            }
        }]
    })
}

#[test]
fn media_detail_roundtrip_preserves_source_stream_and_live_extensions() {
    // These DTOs have flatten maps. This does not promise the same contract
    // for other nested DTOs such as Chapter or UserData, which lack them.
    let source_json = live_source_with_extensions();
    let mut source: MediaSource = serde_json::from_value(source_json.clone()).unwrap();
    source.id = "proxy-source".into();
    let mut expected_source = source_json;
    expected_source["Id"] = json!("proxy-source");
    let serialized_source = serde_json::to_value(&source).unwrap();
    assert_eq!(serialized_source, expected_source);

    let item_json = json!({
        "Id": "backend-item",
        "Type": "Movie",
        "FutureItemMetadata": {
            "Version": 2,
            "Values": [null, "opaque", {"Enabled": false}]
        },
        "MediaSources": [serialized_source],
        "MediaStreams": [{
            "Index": 1,
            "Type": "Subtitle",
            "FutureSubtitleMetadata": {"Styles": ["positioned"], "Nullable": null}
        }]
    });
    let mut item: MediaItem = serde_json::from_value(item_json.clone()).unwrap();
    item.id = "proxy-item".into();
    item.media_sources.as_mut().unwrap()[0].name = Some("Selected version".into());
    let mut expected_item = item_json;
    expected_item["Id"] = json!("proxy-item");
    expected_item["MediaSources"][0]["Name"] = json!("Selected version");
    assert_eq!(serde_json::to_value(&item).unwrap(), expected_item);
}

#[test]
fn playback_roundtrip_preserves_request_response_and_nested_live_extensions() {
    let request_json = json!({
        "MediaSourceId": "proxy-source",
        "AutoOpenLiveStream": true,
        "AudioStreamIndex": 0,
        "DeviceProfile": {
            "Name": "Future client",
            "FutureProfileOptions": {"Values": [null, false, "opaque"]}
        },
        "FuturePlaybackOptions": {"Modes": ["live"], "Nullable": null},
        "FuturePlaybackFlag": false
    });
    let mut request: PlaybackRequest = serde_json::from_value(request_json.clone()).unwrap();
    request.media_source_id = Some("backend-source".into());
    let mut expected_request = request_json;
    expected_request["MediaSourceId"] = json!("backend-source");
    assert_eq!(serde_json::to_value(&request).unwrap(), expected_request);

    let response_json = json!({
        "PlaySessionId": "play-session",
        "MediaSources": [live_source_with_extensions()],
        "FutureNegotiationMetadata": {"Capabilities": ["live"], "Nullable": null}
    });
    let mut response: PlaybackResponse = serde_json::from_value(response_json.clone()).unwrap();
    response.media_sources[0].id = "proxy-source".into();
    let mut expected_response = response_json;
    expected_response["MediaSources"][0]["Id"] = json!("proxy-source");
    assert_eq!(serde_json::to_value(&response).unwrap(), expected_response);
}

#[test]
fn authentication_accepts_jellyfin_and_kodi_password_keys_but_writes_canonical_key() {
    for key in ["Pw", "pw"] {
        let request: AuthenticateRequest =
            serde_json::from_value(json!({"Username":"alice", key:"s3cr3t"}))
                .unwrap_or_else(|error| panic!("{key}: {error}"));
        assert_eq!(request.username, "alice", "{key}");
        assert_eq!(request.password.as_str(), "s3cr3t", "{key}");
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            json!({"Username":"alice", "Pw":"s3cr3t"}),
            "{key}"
        );
    }
}

#[test]
fn media_item_fixture_preserves_fields_and_round_trips() {
    let media_item: MediaItem = fixture("item");

    let serialized = serde_json::to_value(&media_item).unwrap();
    let reparsed: MediaItem = serde_json::from_value(serialized.clone()).unwrap();
    assert_eq!(reparsed.name, media_item.name);
    assert_eq!(reparsed.id, media_item.id);
    assert_eq!(reparsed.server_id, media_item.server_id);
    assert_eq!(serde_json::to_value(reparsed).unwrap(), serialized);

    // Verify basic fields are correct
    assert_eq!(media_item.name.as_ref().unwrap(), "The Batman");
    assert_eq!(
        media_item.server_id.unwrap(),
        "0555e8a91bfc4189a2585ede39a52dc8"
    );
    assert_eq!(media_item.id, "165a66aa5bd2e62c0df0f8da332ae47d");

    // Test optional fields
    assert_eq!(media_item.etag.unwrap(), "11a345e866240c2637db0df717aed59b");

    assert_eq!(media_item.can_delete, Some(true));
    assert_eq!(media_item.can_download, Some(true));

    assert_eq!(media_item.sort_name.unwrap(), "batman");

    // Test external URLs
    let external_urls = media_item.external_urls.unwrap();
    assert_eq!(external_urls.len(), 3);
    assert_eq!(external_urls[0].name, "IMDb");
    assert_eq!(external_urls[0].url, "https://www.imdb.com/title/tt1877830");

    // Test media sources
    let media_sources = media_item.media_sources.unwrap();
    assert!(!media_sources.is_empty());

    let first_source = &media_sources[0];
    assert_eq!(first_source.id, "165a66aa5bd2e62c0df0f8da332ae47d");
    assert_eq!(first_source.container.as_ref().unwrap(), "mkv");
    assert_eq!(first_source.size.unwrap(), 94045682646);
}

#[test]
fn counted_item_fixtures_preserve_items_and_kinds() {
    for (name, count, kind) in [
        ("items", 6, BaseItemKind::Series),
        ("userviews", 2, BaseItemKind::CollectionFolder),
        ("series_nextup", 1, BaseItemKind::Episode),
        ("episodes", 12, BaseItemKind::Episode),
    ] {
        let response: ItemsResponseWithCount = fixture(name);
        assert_eq!(response.items.len(), count, "{name}");
        assert_eq!(response.items[0].item_type, kind, "{name}");
        assert!(
            response.items.iter().all(|item| !item.id.is_empty()),
            "{name}"
        );
    }
}

#[test]
fn partial_media_item_leaves_optional_fields_absent() {
    // Test with minimal JSON data
    let minimal_json = r#"{
        "Name": "Test Movie",
        "ServerId": "test-server-id",
        "Id": "test-id",
        "IsFolder": false,
        "Type": "Movie",
        "UserData": {
            "PlaybackPositionTicks": 0,
            "PlayCount": 0,
            "IsFavorite": false,
            "Played": false,
            "Key": "test-key",
            "ItemId": "test-id"
        }
    }"#;

    let media_item: MediaItem =
        serde_json::from_str(minimal_json).expect("Failed to deserialize minimal MediaItem JSON");

    assert_eq!(media_item.name.unwrap(), "Test Movie");
    assert_eq!(media_item.server_id.unwrap(), "test-server-id");
    assert_eq!(media_item.id, "test-id");
    assert_eq!(media_item.is_folder, Some(false));
    assert_eq!(media_item.item_type, BaseItemKind::Movie);

    // Verify optional fields are None when not provided
    assert!(media_item.etag.is_none());
    assert!(media_item.can_delete.is_none());
    assert!(media_item.external_urls.is_none());
}

#[test]
fn special_features_fixture_deserializes_as_bare_video_list() {
    let items: Vec<MediaItem> = fixture("special_features");
    assert_eq!(items.len(), 14);
    assert_eq!(items[0].item_type, BaseItemKind::Video);
}

#[test]
fn person_fixture_preserves_kind_and_name() {
    let person: MediaItem = fixture("person");
    assert_eq!(person.item_type, BaseItemKind::Person);
    assert_eq!(person.name.as_deref(), Some("Jared Harris"));
}

#[test]
fn live_playback_request_fixture_retains_auto_open_and_user() {
    let request: PlaybackRequest = fixture("livetv_playback_request");
    assert_eq!(request.auto_open_live_stream, Some(true));
    assert_eq!(
        request.user_id.as_deref(),
        Some("807dd942804c4fe7bb6042e7fad22324")
    );
}

#[test]
fn live_playback_response_fixture_retains_session_and_sources() {
    let response: PlaybackResponse = fixture("livetv_playback_response");
    assert_eq!(response.play_session_id, "680d095d98b64cbe879924c4419eb014");
    assert!(!response.media_sources.is_empty());
}
