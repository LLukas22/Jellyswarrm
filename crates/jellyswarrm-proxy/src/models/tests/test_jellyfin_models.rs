use crate::models::MediaItem;
use std::fs;

#[cfg(test)]
mod tests {
    use crate::models::{ItemsResponseWithCount, PlaybackRequest, PlaybackResponse};

    use super::*;

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

        assert!(media_item.has_subtitles.is_some());
        assert!(media_item.has_subtitles.unwrap());

        assert!(media_item.container.is_some());
        assert_eq!(media_item.container.unwrap(), "mkv");

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
            "Media Item: {} ({})",
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
        assert_eq!(media_item.item_type, "Movie");

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
