//! Jellyfin protocol request and response DTOs used by typed proxy behavior.
pub(crate) mod enums;

use std::collections::HashMap;

use jellyswarrm_macros::multi_case_struct;
use serde::{Deserialize, Serialize, Serializer};
use serde_with::skip_serializing_none;

use crate::{
    auth::encryption::Password,
    models::{enums::CollectionType, jellyfin::enums::BaseItemKind},
};

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub(crate) enum StreamIndex {
    Int(i32),
    Str(String),
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct PlaybackRequest {
    pub(crate) always_burn_in_subtitle_when_transcoding: Option<bool>,
    pub(crate) audio_stream_index: Option<StreamIndex>,
    pub(crate) auto_open_live_stream: Option<bool>,
    pub(crate) is_playback: Option<bool>,
    pub(crate) max_streaming_bitrate: Option<i64>,
    pub(crate) media_source_id: Option<String>,
    pub(crate) start_time_ticks: Option<i64>,
    pub(crate) subtitle_stream_index: Option<StreamIndex>,
    pub(crate) user_id: Option<String>,
    pub(crate) device_profile: Option<serde_json::Value>,

    #[serde(flatten)]
    pub(crate) extra: std::collections::HashMap<String, serde_json::Value>,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct PlaybackResponse {
    pub(crate) media_sources: Vec<MediaSource>,
    pub(crate) play_session_id: String,

    #[serde(flatten)]
    pub(crate) extra: std::collections::HashMap<String, serde_json::Value>,
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
pub(crate) struct ProgressRequest {
    pub(crate) audio_stream_index: Option<StreamIndex>,
    pub(crate) buffered_ranges: Option<serde_json::Value>,
    pub(crate) can_seek: Option<bool>,
    pub(crate) event_name: Option<String>,
    pub(crate) is_muted: Option<bool>,
    pub(crate) is_paused: Option<bool>,
    pub(crate) item_id: String,
    pub(crate) max_streaming_bitrate: Option<i64>,
    pub(crate) media_source_id: Option<String>,
    pub(crate) now_playing_queue: Option<Vec<NowPlayingQueueItem>>,
    #[serde(serialize_with = "serialize_playback_rate")]
    pub(crate) playback_rate: Option<f64>,
    pub(crate) playback_start_time_ticks: Option<i64>,
    pub(crate) playlist_item_id: Option<String>,
    pub(crate) play_method: Option<String>,
    pub(crate) play_session_id: Option<String>,
    pub(crate) position_ticks: Option<i64>,
    pub(crate) repeat_mode: Option<String>,
    pub(crate) secondary_subtitle_stream_index: Option<StreamIndex>,
    pub(crate) shuffle_mode: Option<String>,
    pub(crate) subtitle_stream_index: Option<StreamIndex>,
    pub(crate) volume_level: Option<i32>,

    #[serde(flatten)]
    pub(crate) extra: std::collections::HashMap<String, serde_json::Value>,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct BufferedRange {
    pub(crate) start: i64,
    pub(crate) end: i64,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct NowPlayingQueueItem {
    pub(crate) id: String,
    pub(crate) playlist_item_id: String,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct AuthenticateRequest {
    pub(crate) username: String,
    #[serde(rename = "Pw", alias = "pw")]
    pub(crate) password: Password,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct AuthenticateResponse {
    pub(crate) user: User,
    pub(crate) session_info: SessionInfo,
    pub(crate) access_token: String,
    pub(crate) server_id: String,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct PublicServerInfo {
    pub(crate) local_address: String,
    pub(crate) server_name: String,
    pub(crate) version: String,
    pub(crate) product_name: String,
    pub(crate) operating_system: String,
    pub(crate) id: String,
    pub(crate) startup_wizard_completed: bool,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct CastReceiverApplication {
    pub(crate) id: String,
    pub(crate) name: String,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct ServerInfo {
    pub(crate) operating_system_display_name: Option<String>,
    pub(crate) has_pending_restart: Option<bool>,
    pub(crate) is_shutting_down: Option<bool>,
    pub(crate) supports_library_monitor: Option<bool>,
    pub(crate) web_socket_port_number: Option<i32>,
    pub(crate) completed_installations: Option<serde_json::Value>,
    pub(crate) can_self_restart: Option<bool>,
    pub(crate) can_launch_web_browser: Option<bool>,
    pub(crate) program_data_path: Option<String>,
    pub(crate) web_path: Option<String>,
    pub(crate) items_by_name_path: Option<String>,
    pub(crate) cache_path: Option<String>,
    pub(crate) log_path: Option<String>,
    pub(crate) internal_metadata_path: Option<String>,
    pub(crate) transcoding_temp_path: Option<String>,
    pub(crate) cast_receiver_applications: Option<Vec<CastReceiverApplication>>,
    pub(crate) has_update_available: Option<bool>,
    pub(crate) encoder_location: Option<String>,
    pub(crate) system_architecture: Option<String>,
    pub(crate) local_address: String,
    pub(crate) server_name: String,
    pub(crate) version: Option<String>,
    pub(crate) operating_system: Option<String>,
    pub(crate) id: String,
    pub(crate) startup_wizard_completed: Option<bool>,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct User {
    pub(crate) name: String,
    pub(crate) server_id: String,
    pub(crate) id: String,
    pub(crate) policy: UserPolicy,
    #[serde(flatten)]
    pub(crate) extra: std::collections::HashMap<String, serde_json::Value>,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct UserConfiguration {
    pub(crate) play_default_audio_track: bool,
    pub(crate) subtitle_language_preference: String,
    pub(crate) display_missing_episodes: bool,
    pub(crate) grouped_folders: Vec<String>,
    pub(crate) subtitle_mode: String,
    pub(crate) display_collections_view: bool,
    pub(crate) enable_local_password: bool,
    pub(crate) ordered_views: Vec<String>,
    pub(crate) latest_items_excludes: Vec<String>,
    pub(crate) my_media_excludes: Vec<String>,
    pub(crate) hide_played_in_latest: bool,
    pub(crate) remember_audio_selections: bool,
    pub(crate) remember_subtitle_selections: bool,
    pub(crate) enable_next_episode_auto_play: bool,
    pub(crate) cast_receiver_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum SyncPlayUserAccessType {
    CreateAndJoinGroups,
    JoinGroups,
    None,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct UserPolicy {
    pub(crate) is_administrator: bool,
    pub(crate) sync_play_access: SyncPlayUserAccessType,
    #[serde(flatten)]
    pub(crate) extra: std::collections::HashMap<String, serde_json::Value>,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct SessionInfo {
    pub(crate) user_id: String,
    pub(crate) user_name: String,
    pub(crate) server_id: String,
    #[serde(flatten)]
    pub(crate) extra: std::collections::HashMap<String, serde_json::Value>,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct PlayState {
    pub(crate) can_seek: Option<bool>,
    pub(crate) is_paused: Option<bool>,
    pub(crate) is_muted: Option<bool>,
    pub(crate) repeat_mode: Option<String>,
    pub(crate) playback_order: Option<String>,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Default, Serialize, Deserialize, Clone)]
#[serde(default)]
pub(crate) struct Capabilities {
    pub(crate) playable_media_types: Vec<String>,
    pub(crate) supported_commands: Vec<String>,
    pub(crate) supports_media_control: bool,
    pub(crate) supports_persistent_identifier: bool,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct BrandingConfig {
    pub(crate) login_disclaimer: String,
    pub(crate) custom_css: String,
    pub(crate) splashscreen_enabled: bool,
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
pub(crate) struct ItemsResponseWithCount {
    #[serde(rename = "Items")]
    pub(crate) items: Vec<MediaItem>,
    #[serde(rename = "TotalRecordCount")]
    pub(crate) total_record_count: i32,
    #[serde(rename = "StartIndex")]
    pub(crate) start_index: i32,
}

/// Accept either the wrapped response with count or a bare array of `MediaItem`.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub(crate) enum ItemsResponseVariants {
    WithCount(ItemsResponseWithCount),
    Bare(Vec<MediaItem>),
}

impl ItemsResponseVariants {
    pub(crate) fn items(&self) -> &[MediaItem] {
        match self {
            ItemsResponseVariants::WithCount(response) => &response.items,
            ItemsResponseVariants::Bare(items) => items,
        }
    }

    /// Return number of items contained in either variant.
    pub(crate) fn len(&self) -> usize {
        match self {
            ItemsResponseVariants::WithCount(w) => w.items.len(),
            ItemsResponseVariants::Bare(v) => v.len(),
        }
    }

    /// Consume self and return the inner items as a plain `Vec`.
    pub(crate) fn into_items(self) -> Vec<MediaItem> {
        match self {
            ItemsResponseVariants::WithCount(w) => w.items,
            ItemsResponseVariants::Bare(v) => v,
        }
    }
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct MediaItem {
    pub(crate) name: Option<String>,
    pub(crate) album: Option<String>,
    pub(crate) album_artist: Option<String>,
    pub(crate) artists: Option<Vec<String>>,
    pub(crate) server_id: Option<String>,
    pub(crate) id: String,
    pub(crate) item_id: Option<String>,
    pub(crate) series_id: Option<String>,
    pub(crate) series_name: Option<String>,
    pub(crate) season_id: Option<String>,
    pub(crate) etag: Option<String>,
    pub(crate) date_created: Option<String>,
    pub(crate) can_delete: Option<bool>,
    pub(crate) can_download: Option<bool>,
    pub(crate) sort_name: Option<String>,
    pub(crate) external_urls: Option<Vec<ExternalUrl>>,
    pub(crate) path: Option<String>,
    pub(crate) enable_media_source_display: Option<bool>,
    pub(crate) channel_id: Option<String>,
    pub(crate) provider_ids: Option<serde_json::Value>,
    pub(crate) is_folder: Option<bool>,
    pub(crate) parent_id: Option<String>,
    pub(crate) parent_logo_item_id: Option<String>,
    pub(crate) parent_backdrop_item_id: Option<String>,
    pub(crate) parent_backdrop_image_tags: Option<Vec<String>>,
    pub(crate) parent_logo_image_tag: Option<String>,
    pub(crate) parent_thumb_item_id: Option<String>,
    pub(crate) parent_thumb_image_tag: Option<String>,
    #[serde(rename = "Type")]
    pub(crate) item_type: BaseItemKind,
    pub(crate) collection_type: Option<CollectionType>,
    pub(crate) user_data: Option<UserData>,
    pub(crate) child_count: Option<i32>,
    pub(crate) media_source_count: Option<i32>,
    pub(crate) display_preferences_id: Option<String>,
    pub(crate) tags: Option<Vec<String>>,
    pub(crate) series_primary_image_tag: Option<String>,
    pub(crate) image_tags: Option<ImageTags>,
    pub(crate) backdrop_image_tags: Option<Vec<String>>,
    pub(crate) image_blur_hashes: Option<ImageBlurHashes>,
    pub(crate) original_title: Option<String>,
    pub(crate) media_sources: Option<Vec<MediaSource>>,
    pub(crate) media_streams: Option<Vec<MediaStream>>,
    pub(crate) chapters: Option<Vec<Chapter>>,
    pub(crate) people: Option<Vec<Person>>,
    pub(crate) trickplay: Option<std::collections::HashMap<String, serde_json::Value>>,
    pub(crate) production_year: Option<i32>,
    pub(crate) run_time_ticks: Option<i64>,
    pub(crate) community_rating: Option<f32>,
    pub(crate) critic_rating: Option<f32>,
    pub(crate) official_rating: Option<String>,
    pub(crate) premiere_date: Option<String>,

    #[serde(flatten)]
    pub(crate) extra: std::collections::HashMap<String, serde_json::Value>,
}

impl MediaItem {
    pub(crate) fn supports_sort(sort_by: enums::ItemSortBy) -> bool {
        use enums::ItemSortBy::*;
        matches!(
            sort_by,
            SortName
                | Name
                | AlbumArtist
                | Album
                | Artist
                | ProductionYear
                | Runtime
                | CommunityRating
                | CriticRating
                | OfficialRating
                | PremiereDate
                | DateCreated
                | PlayCount
                | DatePlayed
                | ParentIndexNumber
                | IndexNumber
                | StartDate
        )
    }

    fn extra_field(&self, name: &str) -> Option<&serde_json::Value> {
        self.extra
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
    }

    pub(crate) fn item_number(&self, name: &str) -> i64 {
        self.extra_field(name)
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(i64::MAX)
    }

    pub(crate) fn cmp_by(&self, other: &Self, sort_by: enums::ItemSortBy) -> std::cmp::Ordering {
        use enums::ItemSortBy;
        use std::cmp::Ordering;

        match sort_by {
            ItemSortBy::ParentIndexNumber => self
                .item_number("ParentIndexNumber")
                .cmp(&other.item_number("ParentIndexNumber")),
            ItemSortBy::IndexNumber => self
                .item_number("IndexNumber")
                .cmp(&other.item_number("IndexNumber")),
            ItemSortBy::StartDate => compare_dates(
                self.extra_field("StartDate")
                    .and_then(serde_json::Value::as_str),
                other
                    .extra_field("StartDate")
                    .and_then(serde_json::Value::as_str),
            ),
            ItemSortBy::SortName => {
                let left = self
                    .sort_name
                    .as_deref()
                    .or(self.name.as_deref())
                    .unwrap_or("");
                let right = other
                    .sort_name
                    .as_deref()
                    .or(other.name.as_deref())
                    .unwrap_or("");
                left.to_lowercase().cmp(&right.to_lowercase())
            }
            ItemSortBy::Name => self
                .name
                .as_deref()
                .unwrap_or("")
                .to_lowercase()
                .cmp(&other.name.as_deref().unwrap_or("").to_lowercase()),
            ItemSortBy::AlbumArtist => self
                .album_artist
                .as_deref()
                .unwrap_or("")
                .to_lowercase()
                .cmp(&other.album_artist.as_deref().unwrap_or("").to_lowercase()),
            ItemSortBy::Album => compare_text(self.album.as_deref(), other.album.as_deref()),
            ItemSortBy::Artist => compare_text(
                self.artists
                    .as_ref()
                    .and_then(|artists| artists.first())
                    .map(String::as_str),
                other
                    .artists
                    .as_ref()
                    .and_then(|artists| artists.first())
                    .map(String::as_str),
            ),
            ItemSortBy::ProductionYear => self.production_year.cmp(&other.production_year),
            ItemSortBy::Runtime => self.run_time_ticks.cmp(&other.run_time_ticks),
            ItemSortBy::CommunityRating => {
                let left = self.community_rating.unwrap_or(0.0);
                let right = other.community_rating.unwrap_or(0.0);
                left.partial_cmp(&right).unwrap_or(Ordering::Equal)
            }
            ItemSortBy::CriticRating => {
                let left = self.critic_rating.unwrap_or(0.0);
                let right = other.critic_rating.unwrap_or(0.0);
                left.partial_cmp(&right).unwrap_or(Ordering::Equal)
            }
            ItemSortBy::OfficialRating => self.official_rating.cmp(&other.official_rating),
            ItemSortBy::PremiereDate => compare_dates(
                self.premiere_date.as_deref(),
                other.premiere_date.as_deref(),
            ),
            ItemSortBy::DateCreated => {
                compare_dates(self.date_created.as_deref(), other.date_created.as_deref())
            }
            ItemSortBy::PlayCount => {
                let left = self.user_data.as_ref().map(|u| u.play_count).unwrap_or(0);
                let right = other.user_data.as_ref().map(|u| u.play_count).unwrap_or(0);
                left.cmp(&right)
            }
            ItemSortBy::DatePlayed => {
                let left = self
                    .user_data
                    .as_ref()
                    .and_then(|u| u.last_played_date.as_deref());
                let right = other
                    .user_data
                    .as_ref()
                    .and_then(|u| u.last_played_date.as_deref());
                compare_dates(left, right)
            }
            _ => Ordering::Equal,
        }
    }
}

fn compare_text(left: Option<&str>, right: Option<&str>) -> std::cmp::Ordering {
    left.unwrap_or("")
        .to_lowercase()
        .cmp(&right.unwrap_or("").to_lowercase())
}

fn compare_dates(left: Option<&str>, right: Option<&str>) -> std::cmp::Ordering {
    match (left, right) {
        (Some(left), Some(right)) => match (
            chrono::DateTime::parse_from_rfc3339(left),
            chrono::DateTime::parse_from_rfc3339(right),
        ) {
            (Ok(left_date), Ok(right_date)) => left_date.cmp(&right_date),
            _ => left.cmp(right),
        },
        _ => left.cmp(&right),
    }
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct MediaSource {
    pub(crate) protocol: Option<String>,
    pub(crate) id: String,
    pub(crate) path: Option<String>,
    #[serde(rename = "Type")]
    pub(crate) source_type: Option<String>,
    pub(crate) container: Option<String>,
    pub(crate) size: Option<i64>,
    pub(crate) name: Option<String>,
    pub(crate) is_remote: Option<bool>,
    pub(crate) etag: Option<String>,
    pub(crate) video_type: Option<String>,
    pub(crate) media_streams: Option<Vec<MediaStream>>,
    pub(crate) media_attachments: Option<Vec<serde_json::Value>>,
    pub(crate) formats: Option<Vec<String>>,
    pub(crate) bitrate: Option<i64>,
    pub(crate) required_http_headers: Option<serde_json::Value>,
    pub(crate) transcoding_sub_protocol: Option<String>,
    pub(crate) transcoding_url: Option<String>,
    pub(crate) stream_url: Option<String>,
    pub(crate) transcoding_container: Option<String>,
    pub(crate) default_audio_stream_index: Option<i32>,
    pub(crate) default_subtitle_stream_index: Option<i32>,

    #[serde(flatten)]
    extra: HashMap<String, serde_json::Value>,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct MediaStream {
    pub(crate) codec: Option<String>,
    pub(crate) display_title: Option<String>,
    pub(crate) bit_rate: Option<i64>,
    pub(crate) bit_depth: Option<i32>,
    pub(crate) is_default: Option<bool>,
    pub(crate) is_forced: Option<bool>,
    pub(crate) height: Option<i32>,
    pub(crate) width: Option<i32>,
    pub(crate) average_frame_rate: Option<f64>,
    #[serde(rename = "Type")]
    pub(crate) stream_type: Option<String>,
    pub(crate) aspect_ratio: Option<String>,
    pub(crate) index: i32,
    pub(crate) is_text_subtitle_stream: Option<bool>,
    pub(crate) supports_external_stream: Option<bool>,
    pub(crate) pixel_format: Option<String>,
    pub(crate) level: Option<i32>,
    pub(crate) language: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) localized_default: Option<String>,
    pub(crate) localized_external: Option<String>,
    pub(crate) channel_layout: Option<String>,
    pub(crate) channels: Option<i32>,
    pub(crate) sample_rate: Option<i32>,
    pub(crate) delivery_url: Option<String>,
    pub(crate) delivery_method: Option<String>,

    #[serde(flatten)]
    extra: HashMap<String, serde_json::Value>,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct Chapter {
    pub(crate) start_position_ticks: Option<i64>,
    pub(crate) name: Option<String>,
    pub(crate) image_path: Option<String>,
    pub(crate) image_date_modified: Option<String>,
    pub(crate) image_tag: Option<String>,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct Person {
    pub(crate) name: String,
    pub(crate) id: String,
    pub(crate) role: Option<String>,
    #[serde(rename = "Type")]
    pub(crate) person_type: String,
    pub(crate) primary_image_tag: Option<String>,
    pub(crate) image_blur_hashes: Option<ImageBlurHashes>,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct Studio {
    pub(crate) name: String,
    pub(crate) id: String,
}
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct GenreItem {
    pub(crate) name: String,
    pub(crate) id: String,
}
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct ExternalUrl {
    pub(crate) name: String,
    pub(crate) url: String,
}

#[skip_serializing_none]
#[multi_case_struct(pascal, camel)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct UserData {
    pub(crate) playback_position_ticks: i64,
    pub(crate) play_count: i32,
    pub(crate) is_favorite: bool,
    pub(crate) played: bool,
    pub(crate) key: String,
    pub(crate) item_id: String,
    pub(crate) played_percentage: Option<f64>,
    pub(crate) last_played_date: Option<String>,
    pub(crate) unplayed_item_count: Option<i32>,
}

pub(crate) type ImageTags = std::collections::HashMap<String, String>;

pub(crate) type ImageBlurHashes =
    std::collections::HashMap<String, std::collections::HashMap<String, String>>;
