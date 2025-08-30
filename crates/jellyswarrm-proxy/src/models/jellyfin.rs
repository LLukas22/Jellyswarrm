use jellyswarrm_macros::multi_case_struct;
use serde::{Deserialize, Serialize, Serializer};
use serde_with::skip_serializing_none;

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
    pub user_id: String,

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
    pub has_password: bool,
    pub has_configured_password: bool,
    pub has_configured_easy_password: bool,
    pub enable_auto_login: bool,
    pub last_login_date: String,
    pub last_activity_date: String,
    pub configuration: UserConfiguration,
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

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UserPolicy {
    pub is_administrator: bool,
    pub is_hidden: bool,
    pub enable_collection_management: bool,
    pub enable_subtitle_management: bool,
    pub enable_lyric_management: bool,
    pub is_disabled: bool,
    pub blocked_tags: Vec<String>,
    pub allowed_tags: Vec<String>,
    pub enable_user_preference_access: bool,
    pub access_schedules: Vec<String>,
    pub block_unrated_items: Vec<String>,
    pub enable_remote_control_of_other_users: bool,
    pub enable_shared_device_control: bool,
    pub enable_remote_access: bool,
    pub enable_live_tv_management: bool,
    pub enable_live_tv_access: bool,
    pub enable_media_playback: bool,
    pub enable_audio_playback_transcoding: bool,
    pub enable_video_playback_transcoding: bool,
    pub enable_playback_remuxing: bool,
    pub force_remote_source_transcoding: bool,
    pub enable_content_deletion: bool,
    pub enable_content_deletion_from_folders: Vec<String>,
    pub enable_content_downloading: bool,
    pub enable_sync_transcoding: bool,
    pub enable_media_conversion: bool,
    pub enabled_devices: Vec<String>,
    pub enable_all_devices: bool,
    pub enabled_channels: Vec<String>,
    pub enable_all_channels: bool,
    pub enabled_folders: Vec<String>,
    pub enable_all_folders: bool,
    pub invalid_login_attempt_count: i32,
    pub login_attempts_before_lockout: i32,
    pub max_active_sessions: i32,
    pub enable_public_sharing: bool,
    pub blocked_media_folders: Vec<String>,
    pub blocked_channels: Vec<String>,
    pub remote_client_bitrate_limit: i32,
    pub authentication_provider_id: String,
    pub password_reset_provider_id: String,
    pub sync_play_access: String,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SessionInfo {
    pub play_state: PlayState,
    pub additional_users: Vec<String>,
    pub capabilities: Capabilities,
    pub remote_end_point: String,
    pub playable_media_types: Vec<String>,
    pub id: String,
    pub user_id: String,
    pub user_name: String,
    pub client: String,
    pub last_activity_date: String,
    pub last_playback_check_in: String,
    pub device_name: String,
    pub device_id: String,
    pub application_version: String,
    pub is_active: bool,
    pub supports_media_control: bool,
    pub supports_remote_control: bool,
    pub now_playing_queue: Vec<String>,
    pub now_playing_queue_full_items: Vec<String>,
    pub has_custom_device_name: bool,
    pub server_id: String,
    pub supported_commands: Vec<String>,
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
    pub taglines: Option<Vec<String>>,
    pub genres: Option<Vec<String>>,
    pub play_access: Option<String>,
    pub remote_trailers: Option<Vec<RemoteTrailer>>,
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
    pub item_type: String,
    pub people: Option<Vec<Person>>,
    pub studios: Option<Vec<Studio>>,
    pub genre_items: Option<Vec<GenreItem>>,
    pub local_trailer_count: Option<i32>,
    pub user_data: Option<UserData>,
    pub child_count: Option<i32>,
    pub special_feature_count: Option<i32>,
    pub display_preferences_id: Option<String>,
    pub tags: Option<Vec<String>>,
    pub primary_image_aspect_ratio: Option<f64>,
    pub series_primary_image_tag: Option<String>,
    pub collection_type: Option<String>,
    pub image_tags: Option<ImageTags>,
    pub backdrop_image_tags: Option<Vec<String>>,
    pub image_blur_hashes: Option<ImageBlurHashes>,
    pub location_type: Option<String>,
    pub media_type: Option<String>,
    pub locked_fields: Option<Vec<String>>,
    pub lock_data: Option<bool>,
    // New fields from the provided response
    pub container: Option<String>,
    pub premiere_date: Option<String>,
    pub critic_rating: Option<i32>,
    pub official_rating: Option<String>,
    pub community_rating: Option<f64>,
    pub run_time_ticks: Option<i64>,
    pub production_year: Option<i32>,
    pub video_type: Option<String>,
    pub has_subtitles: Option<bool>,
    pub original_title: Option<String>,
    pub overview: Option<String>,
    pub production_locations: Option<Vec<String>>,
    pub is_hd: Option<bool>,
    pub width: Option<i32>,
    pub height: Option<i32>,
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
    pub run_time_ticks: Option<i64>,
    pub read_at_native_framerate: Option<bool>,
    pub ignore_dts: Option<bool>,
    pub ignore_index: Option<bool>,
    pub gen_pts_input: Option<bool>,
    pub supports_transcoding: Option<bool>,
    pub supports_direct_stream: Option<bool>,
    pub supports_direct_play: Option<bool>,
    pub is_infinite_stream: Option<bool>,
    pub use_most_compatible_transcoding_profile: Option<bool>,
    pub requires_opening: Option<bool>,
    pub requires_closing: Option<bool>,
    pub requires_looping: Option<bool>,
    pub supports_probing: Option<bool>,
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
    pub has_segments: Option<bool>,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MediaStream {
    pub codec: Option<String>,
    pub color_space: Option<String>,
    pub color_transfer: Option<String>,
    pub color_primaries: Option<String>,
    pub dv_version_major: Option<i32>,
    pub dv_version_minor: Option<i32>,
    pub dv_profile: Option<i32>,
    pub dv_level: Option<i32>,
    pub rpu_present_flag: Option<i32>,
    pub el_present_flag: Option<i32>,
    pub bl_present_flag: Option<i32>,
    pub dv_bl_signal_compatibility_id: Option<i32>,
    pub time_base: Option<String>,
    pub video_range: Option<String>,
    pub video_range_type: Option<String>,
    pub video_dovi_title: Option<String>,
    pub audio_spatial_format: Option<String>,
    pub display_title: Option<String>,
    pub is_interlaced: Option<bool>,
    pub is_avc: Option<bool>,
    pub bit_rate: Option<i64>,
    pub bit_depth: Option<i32>,
    pub ref_frames: Option<i32>,
    pub is_default: Option<bool>,
    pub is_forced: Option<bool>,
    pub is_hearing_impaired: Option<bool>,
    pub height: Option<i32>,
    pub width: Option<i32>,
    pub average_frame_rate: Option<f64>,
    pub real_frame_rate: Option<f64>,
    pub reference_frame_rate: Option<f64>,
    pub profile: Option<String>,
    #[serde(rename = "Type")]
    pub stream_type: Option<String>,
    pub aspect_ratio: Option<String>,
    pub index: i32,
    pub is_external: Option<bool>,
    pub is_text_subtitle_stream: Option<bool>,
    pub supports_external_stream: Option<bool>,
    pub pixel_format: Option<String>,
    pub level: i32,
    pub is_anamorphic: Option<bool>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub localized_default: Option<String>,
    pub localized_external: Option<String>,
    pub channel_layout: Option<String>,
    pub channels: Option<i32>,
    pub sample_rate: Option<i32>,
    pub delivery_url: Option<String>,
    pub delivery_method: Option<String>,
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
pub struct RemoteTrailer {
    pub url: String,
    pub name: Option<String>,
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
