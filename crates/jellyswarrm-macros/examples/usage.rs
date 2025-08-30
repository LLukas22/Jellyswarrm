use jellyswarrm_macros::multi_case_struct;
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PlaybackRequest {
    pub always_burn_in_subtitle_when_transcoding: Option<bool>,
    pub audio_stream_index: Option<i32>,
    pub auto_open_live_stream: Option<bool>,
    pub is_playback: Option<bool>,
    pub max_streaming_bitrate: Option<i64>,
    pub media_source_id: Option<String>,
    pub start_time_ticks: Option<i64>,
    pub subtitle_stream_index: Option<i32>,
    pub user_id: String,

    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

fn main() {
    let json_pascal = r#"{
        "AlwaysBurnInSubtitleWhenTranscoding": true,
        "UserId": "123",
        "MaxStreamingBitrate": 1000000
    }"#;

    let json_camel = r#"{
        "alwaysBurnInSubtitleWhenTranscoding": true,
        "userId": "123", 
        "maxStreamingBitrate": 1000000
    }"#;

    // Both formats should deserialize successfully
    let request1: PlaybackRequest = serde_json::from_str(json_pascal).unwrap();
    let request2: PlaybackRequest = serde_json::from_str(json_camel).unwrap();

    println!("Pascal case JSON parsed: {:?}", request1);
    println!("Camel case JSON parsed: {:?}", request2);

    // Serialization will use the primary format (first case - pascal in this example)
    let serialized = serde_json::to_string_pretty(&request1).unwrap();
    println!("Serialized (PascalCase):\n{}", serialized);
}
