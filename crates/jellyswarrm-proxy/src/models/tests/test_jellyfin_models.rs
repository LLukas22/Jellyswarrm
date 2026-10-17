//! Verify Jellyfin model serialization and deserialization against representative payloads.
#[cfg(test)]
mod tests {
    use crate::auth::encryption::Password;
    use crate::models::jellyfin::enums::BaseItemKind;
    use crate::models::jellyfin::{AuthenticateRequest, MediaItem, MediaSource};
    use crate::models::{ItemsResponseWithCount, PlaybackRequest, PlaybackResponse};
    use serde_json::{json, Value};
    use std::fs;

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

    /// Regression test: canonical Jellyfin casing ("Pw") must deserialize correctly.
    #[test]
    fn test_authenticate_request_canonical_pw() {
        let json = r#"{"Username":"alice","Pw":"s3cr3t"}"#;
        let req: AuthenticateRequest =
            serde_json::from_str(json).expect("Failed to deserialize with uppercase 'Pw'");
        assert_eq!(req.username, "alice");
        assert_eq!(req.password.as_str(), "s3cr3t");
    }

    /// Regression test: Kodi / JellyCon casing ("pw") must also deserialize correctly.
    /// Before the `alias = "pw"` was added this would panic/fail.
    #[test]
    fn test_authenticate_request_lowercase_pw() {
        let json = r#"{"Username":"bob","pw":"hunter2"}"#;
        let req: AuthenticateRequest = serde_json::from_str(json)
            .expect("Failed to deserialize with lowercase 'pw' (Kodi/JellyCon)");
        assert_eq!(req.username, "bob");
        assert_eq!(req.password.as_str(), "hunter2");
    }

    /// Serialization should always use the canonical "Pw" key regardless of how
    /// the struct was constructed.
    #[test]
    fn test_authenticate_request_serializes_canonical_pw() {
        let req = AuthenticateRequest {
            username: "carol".to_string(),
            password: Password::from("pass123"),
        };
        let value = serde_json::to_value(&req).expect("Failed to serialize AuthenticateRequest");
        let obj = value
            .as_object()
            .expect("AuthenticateRequest should serialize to a JSON object");
        assert!(
            obj.contains_key("Pw"),
            "Serialized JSON should use canonical 'Pw' key, got: {value}"
        );
        assert!(
            !obj.contains_key("pw"),
            "Serialized JSON must not use lowercase 'pw' key, got: {value}"
        );
    }

    #[test]
    fn test_deserialize_item_from_json() {
        // Read the JSON file from the workspace root

        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let file_path = format!("{manifest_dir}/src/models/tests/files/item.json");

        let json_content = fs::read_to_string(file_path).expect("Failed to read item.json file");

        // Deserialize the JSON into a MediaItem
        let media_item: MediaItem =
            serde_json::from_str(&json_content).expect("Failed to deserialize JSON into MediaItem");

        // Verify basic fields are correct
        assert_eq!(media_item.name.as_ref().unwrap(), "The Batman");
        assert_eq!(
            media_item.server_id.unwrap(),
            "0555e8a91bfc4189a2585ede39a52dc8"
        );
        assert_eq!(media_item.id, "165a66aa5bd2e62c0df0f8da332ae47d");

        // Test optional fields
        assert!(media_item.etag.is_some());
        assert_eq!(media_item.etag.unwrap(), "11a345e866240c2637db0df717aed59b");

        assert!(media_item.can_delete.is_some());
        assert!(media_item.can_delete.unwrap());

        assert!(media_item.can_download.is_some());
        assert!(media_item.can_download.unwrap());

        assert!(media_item.sort_name.is_some());
        assert_eq!(media_item.sort_name.unwrap(), "batman");

        // Test external URLs
        assert!(media_item.external_urls.is_some());
        let external_urls = media_item.external_urls.unwrap();
        assert_eq!(external_urls.len(), 3);
        assert_eq!(external_urls[0].name, "IMDb");
        assert_eq!(external_urls[0].url, "https://www.imdb.com/title/tt1877830");

        // Test media sources
        assert!(media_item.media_sources.is_some());
        let media_sources = media_item.media_sources.unwrap();
        assert!(!media_sources.is_empty());

        let first_source = &media_sources[0];
        assert_eq!(first_source.id, "165a66aa5bd2e62c0df0f8da332ae47d");
        assert_eq!(first_source.container.as_ref().unwrap(), "mkv");
        assert_eq!(first_source.size.unwrap(), 94045682646);

        println!("✅ Successfully deserialized MediaItem from JSON!");
        println!(
            "Media Item: {} ({:?})",
            media_item.name.as_ref().unwrap(),
            media_item.item_type
        );
    }

    #[test]
    fn test_deserialize_items_from_json() {
        // Read the JSON file from the workspace root

        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let file_path = format!("{manifest_dir}/src/models/tests/files/items.json");

        let json_content = fs::read_to_string(file_path).expect("Failed to read items.json file");

        // Deserialize the JSON into a MediaItem
        let _: ItemsResponseWithCount = serde_json::from_str(&json_content)
            .expect("Failed to deserialize JSON into ItemsResponse");
    }

    #[test]
    fn test_deserialize_userviews_from_json() {
        // Read the JSON file from the workspace root

        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let file_path = format!("{manifest_dir}/src/models/tests/files/userviews.json");

        let json_content =
            fs::read_to_string(file_path).expect("Failed to read userviews.json file");

        // Deserialize the JSON into a MediaItem
        let _: ItemsResponseWithCount = serde_json::from_str(&json_content)
            .expect("Failed to deserialize JSON into ItemsResponse");
    }

    #[test]
    fn test_serialize_media_item_to_json() {
        // First deserialize from file

        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let file_path = format!("{manifest_dir}/src/models/tests/files/item.json");

        let json_content = fs::read_to_string(file_path).expect("Failed to read res.json file");

        let media_item: MediaItem =
            serde_json::from_str(&json_content).expect("Failed to deserialize JSON into MediaItem");

        // Now serialize back to JSON
        let serialized_json = serde_json::to_string_pretty(&media_item)
            .expect("Failed to serialize MediaItem to JSON");

        // Verify we can deserialize it again
        let deserialized_again: MediaItem =
            serde_json::from_str(&serialized_json).expect("Failed to deserialize serialized JSON");

        // Verify key fields match
        assert_eq!(media_item.name, deserialized_again.name);
        assert_eq!(media_item.id, deserialized_again.id);
        assert_eq!(media_item.server_id, deserialized_again.server_id);

        println!("✅ Successfully round-trip serialized/deserialized MediaItem!");
    }

    #[test]
    fn test_media_item_partial_deserialization() {
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

        let media_item: MediaItem = serde_json::from_str(minimal_json)
            .expect("Failed to deserialize minimal MediaItem JSON");

        assert_eq!(media_item.name.unwrap(), "Test Movie");
        assert_eq!(media_item.server_id.unwrap(), "test-server-id");
        assert_eq!(media_item.id, "test-id");
        assert_eq!(media_item.is_folder, Some(false));
        assert_eq!(media_item.item_type, BaseItemKind::Movie);

        // Verify optional fields are None when not provided
        assert!(media_item.etag.is_none());
        assert!(media_item.can_delete.is_none());
        assert!(media_item.external_urls.is_none());

        println!("✅ Successfully deserialized minimal MediaItem!");
    }

    #[test]
    fn test_media_items() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let file_path = format!("{manifest_dir}/src/models/tests/files/special_features.json");

        let json_content =
            fs::read_to_string(file_path).expect("Failed to read special_features.json file");

        let media_items: Vec<MediaItem> = serde_json::from_str(&json_content)
            .expect("Failed to deserialize JSON into Vec<MediaItem>");

        assert!(!media_items.is_empty(), "Media items should not be empty");
    }

    #[test]
    fn test_media_nextup() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let file_path = format!("{manifest_dir}/src/models/tests/files/series_nextup.json");

        let json_content =
            fs::read_to_string(file_path).expect("Failed to read series_nextup.json file");

        let media_items: ItemsResponseWithCount = serde_json::from_str(&json_content)
            .expect("Failed to deserialize JSON into ItemsResponse");

        assert!(
            !media_items.items.is_empty(),
            "Media items should not be empty"
        );
    }

    #[test]
    fn test_media_episodes() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let file_path = format!("{manifest_dir}/src/models/tests/files/episodes.json");

        let json_content =
            fs::read_to_string(file_path).expect("Failed to read episodes.json file");

        let media_items: ItemsResponseWithCount = serde_json::from_str(&json_content)
            .expect("Failed to deserialize JSON into ItemsResponse");

        assert!(
            !media_items.items.is_empty(),
            "Media items should not be empty"
        );
    }

    #[test]
    fn test_person() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let file_path = format!("{manifest_dir}/src/models/tests/files/person.json");

        let json_content = fs::read_to_string(file_path).expect("Failed to read person.json file");

        let _person: MediaItem = serde_json::from_str(&json_content)
            .map_err(|e| {
                eprintln!("Deserialization error: {e}");
                eprintln!("JSON content: {json_content}");
                e
            })
            .expect("Failed to deserialize JSON into ItemsResponse");
    }

    #[test]
    fn test_livetv_playback_request() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let file_path =
            format!("{manifest_dir}/src/models/tests/files/livetv_playback_request.json");

        let json_content = fs::read_to_string(file_path)
            .expect("Failed to read livetv_playback_request.json file");

        let _person: PlaybackRequest = serde_json::from_str(&json_content)
            .map_err(|e| {
                eprintln!("Deserialization error: {e}");
                eprintln!("JSON content: {json_content}");
                e
            })
            .expect("Failed to deserialize JSON into ItemsResponse");
    }

    #[test]
    fn test_livetv_playback_response() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let file_path =
            format!("{manifest_dir}/src/models/tests/files/livetv_playback_response.json");

        let json_content = fs::read_to_string(file_path)
            .expect("Failed to read livetv_playback_response.json file");

        let _person: PlaybackResponse = serde_json::from_str(&json_content)
            .map_err(|e| {
                eprintln!("Deserialization error: {e}");
                eprintln!("JSON content: {json_content}");
                e
            })
            .expect("Failed to deserialize JSON into ItemsResponse");
    }
}
