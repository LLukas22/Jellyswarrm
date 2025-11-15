pub mod enums;

use std::collections::HashMap;

use jellyswarrm_macros::multi_case_struct;
use serde::{Deserialize, Serialize, Serializer};
use serde_with::skip_serializing_none;

use crate::models::{enums::CollectionType, jellyfin::enums::BaseItemKind};

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum StreamIndex {
    Int(i32),
    Str(String),
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PlaybackRequest {
    pub always_burn_in_subtitle_when_transcoding: Option<bool>,
    pub audio_stream_index: Option<StreamIndex>,
    pub auto_open_live_stream: Option<bool>,
    pub is_playback: Option<bool>,
    pub max_streaming_bitrate: Option<i64>,
    pub media_source_id: Option<String>,
    pub start_time_ticks: Option<i64>,
    pub subtitle_stream_index: Option<StreamIndex>,
    pub user_id: Option<String>,

    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PlaybackResponse {
    pub media_sources: Vec<MediaSource>,
    pub play_session_id: String,
}

#[allow(dead_code)]
fn serialize_playback_rate<S>(value: &Option<f64>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    //For some reason Jellyfin expects playbackrates without decimal point to be integers
    if let Some(value) = value {
        if value.fract() == 0.0 {
            // Serialize as integer if no fractional part
            serializer.serialize_i64(*value as i64)
        } else {
            // Serialize as float if there's a fractional part
            serializer.serialize_f64(*value)
        }
    } else {
        serializer.serialize_none()
    }
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize)]
pub struct ProgressRequest {
    pub audio_stream_index: Option<StreamIndex>,
    pub buffered_ranges: Option<serde_json::Value>,
    pub can_seek: Option<bool>,
    pub event_name: Option<String>,
    pub is_muted: Option<bool>,
    pub is_paused: Option<bool>,
    pub item_id: String,
    pub max_streaming_bitrate: Option<i64>,
    pub media_source_id: Option<String>,
    pub now_playing_queue: Option<Vec<NowPlayingQueueItem>>,
    #[serde(serialize_with = "serialize_playback_rate")]
    pub playback_rate: Option<f64>,
    pub playback_start_time_ticks: Option<i64>,
    pub playlist_item_id: Option<String>,
    pub play_method: Option<String>,
    pub play_session_id: Option<String>,
    pub position_ticks: Option<i64>,
    pub repeat_mode: Option<String>,
    pub secondary_subtitle_stream_index: Option<StreamIndex>,
    pub shuffle_mode: Option<String>,
    pub subtitle_stream_index: Option<StreamIndex>,
    pub volume_level: Option<i32>,

    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize)]
pub struct BufferedRange {
    pub start: i64,
    pub end: i64,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize)]
pub struct NowPlayingQueueItem {
    pub id: String,
    pub playlist_item_id: String,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AuthenticateRequest {
    pub username: String,
    #[serde(rename = "Pw")]
    pub password: String,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AuthenticateResponse {
    pub user: User,
    pub session_info: SessionInfo,
    pub access_token: String,
    pub server_id: String,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PublicServerInfo {
    pub local_address: String,
    pub server_name: String,
    pub version: String,
    pub product_name: String,
    pub operating_system: String,
    pub id: String,
    pub startup_wizard_completed: bool,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CastReceiverApplication {
    pub id: String,
    pub name: String,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ServerInfo {
    pub operating_system_display_name: Option<String>,
    pub has_pending_restart: Option<bool>,
    pub is_shutting_down: Option<bool>,
    pub supports_library_monitor: Option<bool>,
    pub web_socket_port_number: Option<i32>,
    pub completed_installations: Option<serde_json::Value>,
    pub can_self_restart: Option<bool>,
    pub can_launch_web_browser: Option<bool>,
    pub program_data_path: Option<String>,
    pub web_path: Option<String>,
    pub items_by_name_path: Option<String>,
    pub cache_path: Option<String>,
    pub log_path: Option<String>,
    pub internal_metadata_path: Option<String>,
    pub transcoding_temp_path: Option<String>,
    pub cast_receiver_applications: Option<Vec<CastReceiverApplication>>,
    pub has_update_available: Option<bool>,
    pub encoder_location: Option<String>,
    pub system_architecture: Option<String>,
    pub local_address: String,
    pub server_name: String,
    pub version: Option<String>,
    pub operating_system: Option<String>,
    pub id: String,
    pub startup_wizard_completed: Option<bool>,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct User {
    pub name: String,
    pub server_id: String,
    pub id: String,
    pub policy: UserPolicy,
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UserConfiguration {
    pub play_default_audio_track: bool,
    pub subtitle_language_preference: String,
    pub display_missing_episodes: bool,
    pub grouped_folders: Vec<String>,
    pub subtitle_mode: String,
    pub display_collections_view: bool,
    pub enable_local_password: bool,
    pub ordered_views: Vec<String>,
    pub latest_items_excludes: Vec<String>,
    pub my_media_excludes: Vec<String>,
    pub hide_played_in_latest: bool,
    pub remember_audio_selections: bool,
    pub remember_subtitle_selections: bool,
    pub enable_next_episode_auto_play: bool,
    pub cast_receiver_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncPlayUserAccessType {
    CreateAndJoinGroups,
    JoinGroups,
    None,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UserPolicy {
    pub is_administrator: bool,
    pub sync_play_access: SyncPlayUserAccessType,
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SessionInfo {
    pub user_id: String,
    pub user_name: String,
    pub server_id: String,
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PlayState {
    pub can_seek: Option<bool>,
    pub is_paused: Option<bool>,
    pub is_muted: Option<bool>,
    pub repeat_mode: Option<String>,
    pub playback_order: Option<String>,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Capabilities {
    pub playable_media_types: Vec<String>,
    pub supported_commands: Vec<String>,
    pub supports_media_control: bool,
    pub supports_persistent_identifier: bool,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize)]
pub struct BrandingConfig {
    pub login_disclaimer: String,
    pub custom_css: String,
    pub splashscreen_enabled: bool,
}

impl Default for BrandingConfig {
    fn default() -> Self {
        Self {
            login_disclaimer:
                "You are using Jellyswarrm Proxy, a <b>reverse</b> proxy for Jellyfin.".to_string(),
            custom_css: String::new(),
            splashscreen_enabled: false,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ItemsResponseWithCount {
    #[serde(rename = "Items")]
    pub items: Vec<MediaItem>,
    #[serde(rename = "TotalRecordCount")]
    pub total_record_count: i32,
    #[serde(rename = "StartIndex")]
    pub start_index: i32,
}

/// Accept either the wrapped response with count or a bare array of `MediaItem`.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum ItemsResponseVariants {
    WithCount(ItemsResponseWithCount),
    Bare(Vec<MediaItem>),
}

impl ItemsResponseVariants {
    pub fn iter_mut_items(&mut self) -> std::slice::IterMut<'_, MediaItem> {
        match self {
            ItemsResponseVariants::WithCount(w) => w.items.iter_mut(),
            ItemsResponseVariants::Bare(v) => v.iter_mut(),
        }
    }

    /// Return number of items contained in either variant.
    pub fn len(&self) -> usize {
        match self {
            ItemsResponseVariants::WithCount(w) => w.items.len(),
            ItemsResponseVariants::Bare(v) => v.len(),
        }
    }

    /// Return item at `index` if present.
    pub fn get(&self, index: usize) -> Option<&MediaItem> {
        match self {
            ItemsResponseVariants::WithCount(w) => w.items.get(index),
            ItemsResponseVariants::Bare(v) => v.get(index),
        }
    }
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MediaItem {
    pub name: Option<String>,
    pub server_id: Option<String>,
    pub id: String,
    pub item_id: Option<String>,
    pub series_id: Option<String>,
    pub series_name: Option<String>,
    pub season_id: Option<String>,
    pub etag: Option<String>,
    pub date_created: Option<String>,
    pub can_delete: Option<bool>,
    pub can_download: Option<bool>,
    pub sort_name: Option<String>,
    pub external_urls: Option<Vec<ExternalUrl>>,
    pub path: Option<String>,
    pub enable_media_source_display: Option<bool>,
    pub channel_id: Option<String>,
    pub provider_ids: Option<serde_json::Value>,
    pub is_folder: Option<bool>,
    pub parent_id: Option<String>,
    pub parent_logo_item_id: Option<String>,
    pub parent_backdrop_item_id: Option<String>,
    pub parent_backdrop_image_tags: Option<Vec<String>>,
    pub parent_logo_image_tag: Option<String>,
    pub parent_thumb_item_id: Option<String>,
    pub parent_thumb_image_tag: Option<String>,
    #[serde(rename = "Type")]
    pub item_type: BaseItemKind,
    pub collection_type: Option<CollectionType>,
    pub user_data: Option<UserData>,
    pub child_count: Option<i32>,
    pub display_preferences_id: Option<String>,
    pub tags: Option<Vec<String>>,
    pub series_primary_image_tag: Option<String>,
    pub image_tags: Option<ImageTags>,
    pub backdrop_image_tags: Option<Vec<String>>,
    pub image_blur_hashes: Option<ImageBlurHashes>,
    pub original_title: Option<String>,
    pub media_sources: Option<Vec<MediaSource>>,
    pub media_streams: Option<Vec<MediaStream>>,
    pub chapters: Option<Vec<Chapter>>,
    pub trickplay: Option<std::collections::HashMap<String, serde_json::Value>>,

    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MediaSource {
    pub protocol: Option<String>,
    pub id: String,
    pub path: Option<String>,
    #[serde(rename = "Type")]
    pub source_type: Option<String>,
    pub container: Option<String>,
    pub size: Option<i64>,
    pub name: Option<String>,
    pub is_remote: Option<bool>,
    pub etag: Option<String>,
    pub video_type: Option<String>,
    pub media_streams: Option<Vec<MediaStream>>,
    pub media_attachments: Option<Vec<serde_json::Value>>,
    pub formats: Option<Vec<String>>,
    pub bitrate: Option<i64>,
    pub required_http_headers: Option<serde_json::Value>,
    pub transcoding_sub_protocol: Option<String>,
    pub transcoding_url: Option<String>,
    pub transcoding_container: Option<String>,
    pub default_audio_stream_index: Option<i32>,
    pub default_subtitle_stream_index: Option<i32>,

    #[serde(flatten)]
    extra: HashMap<String, serde_json::Value>,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MediaStream {
    pub codec: Option<String>,
    pub display_title: Option<String>,
    pub bit_rate: Option<i64>,
    pub bit_depth: Option<i32>,
    pub is_default: Option<bool>,
    pub is_forced: Option<bool>,
    pub height: Option<i32>,
    pub width: Option<i32>,
    pub average_frame_rate: Option<f64>,
    #[serde(rename = "Type")]
    pub stream_type: Option<String>,
    pub aspect_ratio: Option<String>,
    pub index: i32,
    pub is_text_subtitle_stream: Option<bool>,
    pub supports_external_stream: Option<bool>,
    pub pixel_format: Option<String>,
    pub level: Option<i32>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub localized_default: Option<String>,
    pub localized_external: Option<String>,
    pub channel_layout: Option<String>,
    pub channels: Option<i32>,
    pub sample_rate: Option<i32>,
    pub delivery_url: Option<String>,
    pub delivery_method: Option<String>,

    #[serde(flatten)]
    extra: HashMap<String, serde_json::Value>,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Chapter {
    pub start_position_ticks: Option<i64>,
    pub name: Option<String>,
    pub image_path: Option<String>,
    pub image_date_modified: Option<String>,
    pub image_tag: Option<String>,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Person {
    pub name: String,
    pub id: String,
    pub role: Option<String>,
    #[serde(rename = "Type")]
    pub person_type: String,
    pub primary_image_tag: Option<String>,
    pub image_blur_hashes: Option<ImageBlurHashes>,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Studio {
    pub name: String,
    pub id: String,
}
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GenreItem {
    pub name: String,
    pub id: String,
}
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ExternalUrl {
    pub name: String,
    pub url: String,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UserData {
    pub playback_position_ticks: i64,
    pub play_count: i32,
    pub is_favorite: bool,
    pub played: bool,
    pub key: String,
    pub item_id: String,
    pub played_percentage: Option<f64>,
    pub last_played_date: Option<String>,
    pub unplayed_item_count: Option<i32>,
}

pub type ImageTags = std::collections::HashMap<String, String>;

pub type ImageBlurHashes =
    std::collections::HashMap<String, std::collections::HashMap<String, String>>;
