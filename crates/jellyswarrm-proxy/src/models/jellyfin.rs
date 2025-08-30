use serde::{Deserialize, Serialize, Serializer};
use serde_with::skip_serializing_none;

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum StreamIndex {
    Int(i32),
    Str(String),
}

#[skip_serializing_none]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PlaybackRequest {
    #[serde(rename = "AlwaysBurnInSubtitleWhenTranscoding")]
    pub always_burn_in_subtitle_when_transcoding: Option<bool>,
    #[serde(rename = "AudioStreamIndex")]
    pub audio_stream_index: Option<StreamIndex>,
    #[serde(rename = "AutoOpenLiveStream")]
    pub auto_open_live_stream: Option<bool>,
    #[serde(rename = "IsPlayback")]
    pub is_playback: Option<bool>,
    #[serde(rename = "MaxStreamingBitrate")]
    pub max_streaming_bitrate: Option<i64>,
    #[serde(rename = "MediaSourceId")]
    pub media_source_id: Option<String>,
    #[serde(rename = "StartTimeTicks")]
    pub start_time_ticks: Option<i64>,
    #[serde(rename = "SubtitleStreamIndex")]
    pub subtitle_stream_index: Option<StreamIndex>,
    #[serde(rename = "UserId", alias = "userId")]
    pub user_id: String,

    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

#[skip_serializing_none]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PlaybackResponse {
    #[serde(rename = "MediaSources")]
    pub media_sources: Vec<MediaSource>,
    #[serde(rename = "PlaySessionId")]
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
#[derive(Debug, Serialize, Deserialize)]
pub struct ProgressRequest {
    #[serde(rename = "AudioStreamIndex")]
    pub audio_stream_index: Option<StreamIndex>,
    #[serde(rename = "BufferedRanges")]
    pub buffered_ranges: Option<serde_json::Value>,
    #[serde(rename = "CanSeek")]
    pub can_seek: Option<bool>,
    #[serde(rename = "EventName")]
    pub event_name: Option<String>,
    #[serde(rename = "IsMuted")]
    pub is_muted: bool,
    #[serde(rename = "IsPaused")]
    pub is_paused: bool,
    #[serde(rename = "ItemId")]
    pub item_id: String,
    #[serde(rename = "MaxStreamingBitrate")]
    pub max_streaming_bitrate: Option<i64>,
    #[serde(rename = "MediaSourceId")]
    pub media_source_id: String,
    #[serde(rename = "NowPlayingQueue")]
    pub now_playing_queue: Option<Vec<NowPlayingQueueItem>>,
    #[serde(rename = "PlaybackRate", serialize_with = "serialize_playback_rate")]
    pub playback_rate: Option<f64>,
    #[serde(rename = "PlaybackStartTimeTicks")]
    pub playback_start_time_ticks: Option<i64>,
    #[serde(rename = "PlaylistItemId")]
    pub playlist_item_id: Option<String>,
    #[serde(rename = "PlayMethod")]
    pub play_method: String,
    #[serde(rename = "PlaySessionId")]
    pub play_session_id: String,
    #[serde(rename = "PositionTicks")]
    pub position_ticks: i64,
    #[serde(rename = "RepeatMode")]
    pub repeat_mode: String,
    #[serde(rename = "SecondarySubtitleStreamIndex")]
    pub secondary_subtitle_stream_index: Option<StreamIndex>,
    #[serde(rename = "ShuffleMode")]
    pub shuffle_mode: Option<String>,
    #[serde(rename = "SubtitleStreamIndex")]
    pub subtitle_stream_index: Option<StreamIndex>,
    #[serde(rename = "VolumeLevel")]
    pub volume_level: Option<i32>,

    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BufferedRange {
    pub start: i64,
    pub end: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NowPlayingQueueItem {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "PlaylistItemId")]
    pub playlist_item_id: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AuthenticateRequest {
    #[serde(rename = "Username")]
    pub username: String,
    #[serde(rename = "Pw")]
    pub password: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AuthenticateResponse {
    #[serde(rename = "User")]
    pub user: User,
    #[serde(rename = "SessionInfo")]
    pub session_info: SessionInfo,
    #[serde(rename = "AccessToken")]
    pub access_token: String,
    #[serde(rename = "ServerId")]
    pub server_id: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PublicServerInfo {
    #[serde(rename = "LocalAddress")]
    pub local_address: String,
    #[serde(rename = "ServerName")]
    pub server_name: String,
    #[serde(rename = "Version")]
    pub version: String,
    #[serde(rename = "ProductName")]
    pub product_name: String,
    #[serde(rename = "OperatingSystem")]
    pub operating_system: String,
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "StartupWizardCompleted")]
    pub startup_wizard_completed: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CastReceiverApplication {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Name")]
    pub name: String,
}

#[skip_serializing_none]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ServerInfo {
    #[serde(rename = "OperatingSystemDisplayName")]
    pub operating_system_display_name: Option<String>,

    #[serde(rename = "HasPendingRestart")]
    pub has_pending_restart: Option<bool>,

    #[serde(rename = "IsShuttingDown")]
    pub is_shutting_down: Option<bool>,

    #[serde(rename = "SupportsLibraryMonitor")]
    pub supports_library_monitor: Option<bool>,

    #[serde(rename = "WebSocketPortNumber")]
    pub web_socket_port_number: Option<i32>,

    #[serde(rename = "CompletedInstallations")]
    pub completed_installations: Option<serde_json::Value>,

    #[serde(rename = "CanSelfRestart")]
    pub can_self_restart: Option<bool>,

    #[serde(rename = "CanLaunchWebBrowser")]
    pub can_launch_web_browser: Option<bool>,

    #[serde(rename = "ProgramDataPath")]
    pub program_data_path: Option<String>,

    #[serde(rename = "WebPath")]
    pub web_path: Option<String>,

    #[serde(rename = "ItemsByNamePath")]
    pub items_by_name_path: Option<String>,

    #[serde(rename = "CachePath")]
    pub cache_path: Option<String>,

    #[serde(rename = "LogPath")]
    pub log_path: Option<String>,

    #[serde(rename = "InternalMetadataPath")]
    pub internal_metadata_path: Option<String>,

    #[serde(rename = "TranscodingTempPath")]
    pub transcoding_temp_path: Option<String>,

    #[serde(rename = "CastReceiverApplications")]
    pub cast_receiver_applications: Option<Vec<CastReceiverApplication>>,

    #[serde(rename = "HasUpdateAvailable")]
    pub has_update_available: Option<bool>,

    #[serde(rename = "EncoderLocation")]
    pub encoder_location: Option<String>,

    #[serde(rename = "SystemArchitecture")]
    pub system_architecture: Option<String>,

    #[serde(rename = "LocalAddress")]
    pub local_address: String,

    #[serde(rename = "ServerName")]
    pub server_name: String,

    #[serde(rename = "Version")]
    pub version: Option<String>,

    #[serde(rename = "OperatingSystem")]
    pub operating_system: Option<String>,

    #[serde(rename = "Id")]
    pub id: String,

    #[serde(rename = "StartupWizardCompleted")]
    pub startup_wizard_completed: Option<bool>,
}

#[skip_serializing_none]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct User {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "ServerId")]
    pub server_id: String,
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "HasPassword")]
    pub has_password: bool,
    #[serde(rename = "HasConfiguredPassword")]
    pub has_configured_password: bool,
    #[serde(rename = "HasConfiguredEasyPassword")]
    pub has_configured_easy_password: bool,
    #[serde(rename = "EnableAutoLogin")]
    pub enable_auto_login: bool,
    #[serde(rename = "LastLoginDate")]
    pub last_login_date: String,
    #[serde(rename = "LastActivityDate")]
    pub last_activity_date: String,
    #[serde(rename = "Configuration")]
    pub configuration: UserConfiguration,
    #[serde(rename = "Policy")]
    pub policy: UserPolicy,
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

#[skip_serializing_none]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UserConfiguration {
    #[serde(rename = "PlayDefaultAudioTrack")]
    pub play_default_audio_track: bool,
    #[serde(rename = "SubtitleLanguagePreference")]
    pub subtitle_language_preference: String,
    #[serde(rename = "DisplayMissingEpisodes")]
    pub display_missing_episodes: bool,
    #[serde(rename = "GroupedFolders")]
    pub grouped_folders: Vec<String>,
    #[serde(rename = "SubtitleMode")]
    pub subtitle_mode: String,
    #[serde(rename = "DisplayCollectionsView")]
    pub display_collections_view: bool,
    #[serde(rename = "EnableLocalPassword")]
    pub enable_local_password: bool,
    #[serde(rename = "OrderedViews")]
    pub ordered_views: Vec<String>,
    #[serde(rename = "LatestItemsExcludes")]
    pub latest_items_excludes: Vec<String>,
    #[serde(rename = "MyMediaExcludes")]
    pub my_media_excludes: Vec<String>,
    #[serde(rename = "HidePlayedInLatest")]
    pub hide_played_in_latest: bool,
    #[serde(rename = "RememberAudioSelections")]
    pub remember_audio_selections: bool,
    #[serde(rename = "RememberSubtitleSelections")]
    pub remember_subtitle_selections: bool,
    #[serde(rename = "EnableNextEpisodeAutoPlay")]
    pub enable_next_episode_auto_play: bool,
    #[serde(rename = "CastReceiverId")]
    pub cast_receiver_id: String,
}

#[skip_serializing_none]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UserPolicy {
    #[serde(rename = "IsAdministrator")]
    pub is_administrator: bool,
    #[serde(rename = "IsHidden")]
    pub is_hidden: bool,
    #[serde(rename = "EnableCollectionManagement")]
    pub enable_collection_management: bool,
    #[serde(rename = "EnableSubtitleManagement")]
    pub enable_subtitle_management: bool,
    #[serde(rename = "EnableLyricManagement")]
    pub enable_lyric_management: bool,
    #[serde(rename = "IsDisabled")]
    pub is_disabled: bool,
    #[serde(rename = "BlockedTags")]
    pub blocked_tags: Vec<String>,
    #[serde(rename = "AllowedTags")]
    pub allowed_tags: Vec<String>,
    #[serde(rename = "EnableUserPreferenceAccess")]
    pub enable_user_preference_access: bool,
    #[serde(rename = "AccessSchedules")]
    pub access_schedules: Vec<String>,
    #[serde(rename = "BlockUnratedItems")]
    pub block_unrated_items: Vec<String>,
    #[serde(rename = "EnableRemoteControlOfOtherUsers")]
    pub enable_remote_control_of_other_users: bool,
    #[serde(rename = "EnableSharedDeviceControl")]
    pub enable_shared_device_control: bool,
    #[serde(rename = "EnableRemoteAccess")]
    pub enable_remote_access: bool,
    #[serde(rename = "EnableLiveTvManagement")]
    pub enable_live_tv_management: bool,
    #[serde(rename = "EnableLiveTvAccess")]
    pub enable_live_tv_access: bool,
    #[serde(rename = "EnableMediaPlayback")]
    pub enable_media_playback: bool,
    #[serde(rename = "EnableAudioPlaybackTranscoding")]
    pub enable_audio_playback_transcoding: bool,
    #[serde(rename = "EnableVideoPlaybackTranscoding")]
    pub enable_video_playback_transcoding: bool,
    #[serde(rename = "EnablePlaybackRemuxing")]
    pub enable_playback_remuxing: bool,
    #[serde(rename = "ForceRemoteSourceTranscoding")]
    pub force_remote_source_transcoding: bool,
    #[serde(rename = "EnableContentDeletion")]
    pub enable_content_deletion: bool,
    #[serde(rename = "EnableContentDeletionFromFolders")]
    pub enable_content_deletion_from_folders: Vec<String>,
    #[serde(rename = "EnableContentDownloading")]
    pub enable_content_downloading: bool,
    #[serde(rename = "EnableSyncTranscoding")]
    pub enable_sync_transcoding: bool,
    #[serde(rename = "EnableMediaConversion")]
    pub enable_media_conversion: bool,
    #[serde(rename = "EnabledDevices")]
    pub enabled_devices: Vec<String>,
    #[serde(rename = "EnableAllDevices")]
    pub enable_all_devices: bool,
    #[serde(rename = "EnabledChannels")]
    pub enabled_channels: Vec<String>,
    #[serde(rename = "EnableAllChannels")]
    pub enable_all_channels: bool,
    #[serde(rename = "EnabledFolders")]
    pub enabled_folders: Vec<String>,
    #[serde(rename = "EnableAllFolders")]
    pub enable_all_folders: bool,
    #[serde(rename = "InvalidLoginAttemptCount")]
    pub invalid_login_attempt_count: i32,
    #[serde(rename = "LoginAttemptsBeforeLockout")]
    pub login_attempts_before_lockout: i32,
    #[serde(rename = "MaxActiveSessions")]
    pub max_active_sessions: i32,
    #[serde(rename = "EnablePublicSharing")]
    pub enable_public_sharing: bool,
    #[serde(rename = "BlockedMediaFolders")]
    pub blocked_media_folders: Vec<String>,
    #[serde(rename = "BlockedChannels")]
    pub blocked_channels: Vec<String>,
    #[serde(rename = "RemoteClientBitrateLimit")]
    pub remote_client_bitrate_limit: i32,
    #[serde(rename = "AuthenticationProviderId")]
    pub authentication_provider_id: String,
    #[serde(rename = "PasswordResetProviderId")]
    pub password_reset_provider_id: String,
    #[serde(rename = "SyncPlayAccess")]
    pub sync_play_access: String,
}

#[skip_serializing_none]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SessionInfo {
    #[serde(rename = "PlayState")]
    pub play_state: PlayState,
    #[serde(rename = "AdditionalUsers")]
    pub additional_users: Vec<String>,
    #[serde(rename = "Capabilities")]
    pub capabilities: Capabilities,
    #[serde(rename = "RemoteEndPoint")]
    pub remote_end_point: String,
    #[serde(rename = "PlayableMediaTypes")]
    pub playable_media_types: Vec<String>,
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "UserId")]
    pub user_id: String,
    #[serde(rename = "UserName")]
    pub user_name: String,
    #[serde(rename = "Client")]
    pub client: String,
    #[serde(rename = "LastActivityDate")]
    pub last_activity_date: String,
    #[serde(rename = "LastPlaybackCheckIn")]
    pub last_playback_check_in: String,
    #[serde(rename = "DeviceName")]
    pub device_name: String,
    #[serde(rename = "DeviceId")]
    pub device_id: String,
    #[serde(rename = "ApplicationVersion")]
    pub application_version: String,
    #[serde(rename = "IsActive")]
    pub is_active: bool,
    #[serde(rename = "SupportsMediaControl")]
    pub supports_media_control: bool,
    #[serde(rename = "SupportsRemoteControl")]
    pub supports_remote_control: bool,
    #[serde(rename = "NowPlayingQueue")]
    pub now_playing_queue: Vec<String>,
    #[serde(rename = "NowPlayingQueueFullItems")]
    pub now_playing_queue_full_items: Vec<String>,
    #[serde(rename = "HasCustomDeviceName")]
    pub has_custom_device_name: bool,
    #[serde(rename = "ServerId")]
    pub server_id: String,
    #[serde(rename = "SupportedCommands")]
    pub supported_commands: Vec<String>,
}

#[skip_serializing_none]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PlayState {
    #[serde(rename = "CanSeek")]
    pub can_seek: bool,
    #[serde(rename = "IsPaused")]
    pub is_paused: bool,
    #[serde(rename = "IsMuted")]
    pub is_muted: bool,
    #[serde(rename = "RepeatMode")]
    pub repeat_mode: String,
    #[serde(rename = "PlaybackOrder")]
    pub playback_order: String,
}

#[skip_serializing_none]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Capabilities {
    #[serde(rename = "PlayableMediaTypes")]
    pub playable_media_types: Vec<String>,
    #[serde(rename = "SupportedCommands")]
    pub supported_commands: Vec<String>,
    #[serde(rename = "SupportsMediaControl")]
    pub supports_media_control: bool,
    #[serde(rename = "SupportsPersistentIdentifier")]
    pub supports_persistent_identifier: bool,
}

#[skip_serializing_none]
#[derive(Debug, Serialize, Deserialize)]
pub struct BrandingConfig {
    #[serde(rename = "LoginDisclaimer")]
    pub login_disclaimer: String,
    #[serde(rename = "CustomCss")]
    pub custom_css: String,
    #[serde(rename = "SplashscreenEnabled")]
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
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MediaItem {
    #[serde(rename = "Name")]
    pub name: Option<String>,
    #[serde(rename = "ServerId")]
    pub server_id: Option<String>,
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "ItemId")]
    pub item_id: Option<String>,
    #[serde(rename = "SeriesId")]
    pub series_id: Option<String>,
    #[serde(rename = "SeriesName")]
    pub series_name: Option<String>,
    #[serde(rename = "SeasonId")]
    pub season_id: Option<String>,
    #[serde(rename = "Etag")]
    pub etag: Option<String>,
    #[serde(rename = "DateCreated")]
    pub date_created: Option<String>,
    #[serde(rename = "CanDelete")]
    pub can_delete: Option<bool>,
    #[serde(rename = "CanDownload")]
    pub can_download: Option<bool>,
    #[serde(rename = "SortName")]
    pub sort_name: Option<String>,
    #[serde(rename = "ExternalUrls")]
    pub external_urls: Option<Vec<ExternalUrl>>,
    #[serde(rename = "Path")]
    pub path: Option<String>,
    #[serde(rename = "EnableMediaSourceDisplay")]
    pub enable_media_source_display: Option<bool>,
    #[serde(rename = "ChannelId")]
    pub channel_id: Option<String>,
    #[serde(rename = "Taglines")]
    pub taglines: Option<Vec<String>>,
    #[serde(rename = "Genres")]
    pub genres: Option<Vec<String>>,
    #[serde(rename = "PlayAccess")]
    pub play_access: Option<String>,
    #[serde(rename = "RemoteTrailers")]
    pub remote_trailers: Option<Vec<RemoteTrailer>>,
    #[serde(rename = "ProviderIds")]
    pub provider_ids: Option<serde_json::Value>,
    #[serde(rename = "IsFolder")]
    pub is_folder: Option<bool>,
    #[serde(rename = "ParentId")]
    pub parent_id: Option<String>,
    #[serde(rename = "ParentLogoItemId")]
    pub parent_logo_item_id: Option<String>,
    #[serde(rename = "ParentBackdropItemId")]
    pub parent_backdrop_item_id: Option<String>,
    #[serde(rename = "ParentBackdropImageTags")]
    pub parent_backdrop_image_tags: Option<Vec<String>>,
    #[serde(rename = "ParentLogoImageTag")]
    pub parent_logo_image_tag: Option<String>,
    #[serde(rename = "ParentThumbItemId")]
    pub parent_thumb_item_id: Option<String>,
    #[serde(rename = "ParentThumbImageTag")]
    pub parent_thumb_image_tag: Option<String>,
    #[serde(rename = "Type")]
    pub item_type: String,
    #[serde(rename = "People")]
    pub people: Option<Vec<Person>>,
    #[serde(rename = "Studios")]
    pub studios: Option<Vec<Studio>>,
    #[serde(rename = "GenreItems")]
    pub genre_items: Option<Vec<GenreItem>>,
    #[serde(rename = "LocalTrailerCount")]
    pub local_trailer_count: Option<i32>,
    #[serde(rename = "UserData")]
    pub user_data: Option<UserData>,
    #[serde(rename = "ChildCount")]
    pub child_count: Option<i32>,
    #[serde(rename = "SpecialFeatureCount")]
    pub special_feature_count: Option<i32>,
    #[serde(rename = "DisplayPreferencesId")]
    pub display_preferences_id: Option<String>,
    #[serde(rename = "Tags")]
    pub tags: Option<Vec<String>>,
    #[serde(rename = "PrimaryImageAspectRatio")]
    pub primary_image_aspect_ratio: Option<f64>,
    #[serde(rename = "SeriesPrimaryImageTag")]
    pub series_primary_image_tag: Option<String>,
    #[serde(rename = "CollectionType")]
    pub collection_type: Option<String>,
    #[serde(rename = "ImageTags")]
    pub image_tags: Option<ImageTags>,
    #[serde(rename = "BackdropImageTags")]
    pub backdrop_image_tags: Option<Vec<String>>,
    #[serde(rename = "ImageBlurHashes")]
    pub image_blur_hashes: Option<ImageBlurHashes>,
    #[serde(rename = "LocationType")]
    pub location_type: Option<String>,
    #[serde(rename = "MediaType")]
    pub media_type: Option<String>,
    #[serde(rename = "LockedFields")]
    pub locked_fields: Option<Vec<String>>,
    #[serde(rename = "LockData")]
    pub lock_data: Option<bool>,
    // New fields from the provided response
    #[serde(rename = "Container")]
    pub container: Option<String>,
    #[serde(rename = "PremiereDate")]
    pub premiere_date: Option<String>,
    #[serde(rename = "CriticRating")]
    pub critic_rating: Option<i32>,
    #[serde(rename = "OfficialRating")]
    pub official_rating: Option<String>,
    #[serde(rename = "CommunityRating")]
    pub community_rating: Option<f64>,
    #[serde(rename = "RunTimeTicks")]
    pub run_time_ticks: Option<i64>,
    #[serde(rename = "ProductionYear")]
    pub production_year: Option<i32>,
    #[serde(rename = "VideoType")]
    pub video_type: Option<String>,
    #[serde(rename = "HasSubtitles")]
    pub has_subtitles: Option<bool>,
    #[serde(rename = "OriginalTitle")]
    pub original_title: Option<String>,
    #[serde(rename = "Overview")]
    pub overview: Option<String>,
    #[serde(rename = "ProductionLocations")]
    pub production_locations: Option<Vec<String>>,
    #[serde(rename = "IsHD")]
    pub is_hd: Option<bool>,
    #[serde(rename = "Width")]
    pub width: Option<i32>,
    #[serde(rename = "Height")]
    pub height: Option<i32>,
    #[serde(rename = "MediaSources")]
    pub media_sources: Option<Vec<MediaSource>>,
    #[serde(rename = "MediaStreams")]
    pub media_streams: Option<Vec<MediaStream>>,
    #[serde(rename = "Chapters")]
    pub chapters: Option<Vec<Chapter>>,
    #[serde(rename = "Trickplay")]
    pub trickplay: Option<std::collections::HashMap<String, serde_json::Value>>,
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

#[skip_serializing_none]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MediaSource {
    #[serde(rename = "Protocol")]
    pub protocol: Option<String>,
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Path")]
    pub path: Option<String>,
    #[serde(rename = "Type")]
    pub source_type: Option<String>,
    #[serde(rename = "Container")]
    pub container: Option<String>,
    #[serde(rename = "Size")]
    pub size: Option<i64>,
    #[serde(rename = "Name")]
    pub name: Option<String>,
    #[serde(rename = "IsRemote")]
    pub is_remote: Option<bool>,
    #[serde(rename = "ETag")]
    pub etag: Option<String>,
    #[serde(rename = "RunTimeTicks")]
    pub run_time_ticks: Option<i64>,
    #[serde(rename = "ReadAtNativeFramerate")]
    pub read_at_native_framerate: Option<bool>,
    #[serde(rename = "IgnoreDts")]
    pub ignore_dts: Option<bool>,
    #[serde(rename = "IgnoreIndex")]
    pub ignore_index: Option<bool>,
    #[serde(rename = "GenPtsInput")]
    pub gen_pts_input: Option<bool>,
    #[serde(rename = "SupportsTranscoding")]
    pub supports_transcoding: Option<bool>,
    #[serde(rename = "SupportsDirectStream")]
    pub supports_direct_stream: Option<bool>,
    #[serde(rename = "SupportsDirectPlay")]
    pub supports_direct_play: Option<bool>,
    #[serde(rename = "IsInfiniteStream")]
    pub is_infinite_stream: Option<bool>,
    #[serde(rename = "UseMostCompatibleTranscodingProfile")]
    pub use_most_compatible_transcoding_profile: Option<bool>,
    #[serde(rename = "RequiresOpening")]
    pub requires_opening: Option<bool>,
    #[serde(rename = "RequiresClosing")]
    pub requires_closing: Option<bool>,
    #[serde(rename = "RequiresLooping")]
    pub requires_looping: Option<bool>,
    #[serde(rename = "SupportsProbing")]
    pub supports_probing: Option<bool>,
    #[serde(rename = "VideoType")]
    pub video_type: Option<String>,
    #[serde(rename = "MediaStreams")]
    pub media_streams: Option<Vec<MediaStream>>,
    #[serde(rename = "MediaAttachments")]
    pub media_attachments: Option<Vec<serde_json::Value>>,
    #[serde(rename = "Formats")]
    pub formats: Option<Vec<String>>,
    #[serde(rename = "Bitrate")]
    pub bitrate: Option<i64>,
    #[serde(rename = "RequiredHttpHeaders")]
    pub required_http_headers: Option<serde_json::Value>,
    #[serde(rename = "TranscodingSubProtocol")]
    pub transcoding_sub_protocol: Option<String>,
    #[serde(rename = "TranscodingUrl")]
    pub transcoding_url: Option<String>,
    #[serde(rename = "TranscodingContainer")]
    pub transcoding_container: Option<String>,
    #[serde(rename = "DefaultAudioStreamIndex")]
    pub default_audio_stream_index: Option<i32>,
    #[serde(rename = "DefaultSubtitleStreamIndex")]
    pub default_subtitle_stream_index: Option<i32>,
    #[serde(rename = "HasSegments")]
    pub has_segments: Option<bool>,
}

#[skip_serializing_none]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MediaStream {
    #[serde(rename = "Codec")]
    pub codec: Option<String>,
    #[serde(rename = "ColorSpace")]
    pub color_space: Option<String>,
    #[serde(rename = "ColorTransfer")]
    pub color_transfer: Option<String>,
    #[serde(rename = "ColorPrimaries")]
    pub color_primaries: Option<String>,
    #[serde(rename = "DvVersionMajor")]
    pub dv_version_major: Option<i32>,
    #[serde(rename = "DvVersionMinor")]
    pub dv_version_minor: Option<i32>,
    #[serde(rename = "DvProfile")]
    pub dv_profile: Option<i32>,
    #[serde(rename = "DvLevel")]
    pub dv_level: Option<i32>,
    #[serde(rename = "RpuPresentFlag")]
    pub rpu_present_flag: Option<i32>,
    #[serde(rename = "ElPresentFlag")]
    pub el_present_flag: Option<i32>,
    #[serde(rename = "BlPresentFlag")]
    pub bl_present_flag: Option<i32>,
    #[serde(rename = "DvBlSignalCompatibilityId")]
    pub dv_bl_signal_compatibility_id: Option<i32>,
    #[serde(rename = "TimeBase")]
    pub time_base: Option<String>,
    #[serde(rename = "VideoRange")]
    pub video_range: Option<String>,
    #[serde(rename = "VideoRangeType")]
    pub video_range_type: Option<String>,
    #[serde(rename = "VideoDoViTitle")]
    pub video_dovi_title: Option<String>,
    #[serde(rename = "AudioSpatialFormat")]
    pub audio_spatial_format: Option<String>,
    #[serde(rename = "DisplayTitle")]
    pub display_title: Option<String>,
    #[serde(rename = "IsInterlaced")]
    pub is_interlaced: Option<bool>,
    #[serde(rename = "IsAVC")]
    pub is_avc: Option<bool>,
    #[serde(rename = "BitRate")]
    pub bit_rate: Option<i64>,
    #[serde(rename = "BitDepth")]
    pub bit_depth: Option<i32>,
    #[serde(rename = "RefFrames")]
    pub ref_frames: Option<i32>,
    #[serde(rename = "IsDefault")]
    pub is_default: Option<bool>,
    #[serde(rename = "IsForced")]
    pub is_forced: Option<bool>,
    #[serde(rename = "IsHearingImpaired")]
    pub is_hearing_impaired: Option<bool>,
    #[serde(rename = "Height")]
    pub height: Option<i32>,
    #[serde(rename = "Width")]
    pub width: Option<i32>,
    #[serde(rename = "AverageFrameRate")]
    pub average_frame_rate: Option<f64>,
    #[serde(rename = "RealFrameRate")]
    pub real_frame_rate: Option<f64>,
    #[serde(rename = "ReferenceFrameRate")]
    pub reference_frame_rate: Option<f64>,
    #[serde(rename = "Profile")]
    pub profile: Option<String>,
    #[serde(rename = "Type")]
    pub stream_type: Option<String>,
    #[serde(rename = "AspectRatio")]
    pub aspect_ratio: Option<String>,
    #[serde(rename = "Index")]
    pub index: i32,
    #[serde(rename = "IsExternal")]
    pub is_external: Option<bool>,
    #[serde(rename = "IsTextSubtitleStream")]
    pub is_text_subtitle_stream: Option<bool>,
    #[serde(rename = "SupportsExternalStream")]
    pub supports_external_stream: Option<bool>,
    #[serde(rename = "PixelFormat")]
    pub pixel_format: Option<String>,
    #[serde(rename = "Level")]
    pub level: i32,
    #[serde(rename = "IsAnamorphic")]
    pub is_anamorphic: Option<bool>,
    #[serde(rename = "Language")]
    pub language: Option<String>,
    #[serde(rename = "Title")]
    pub title: Option<String>,
    #[serde(rename = "LocalizedDefault")]
    pub localized_default: Option<String>,
    #[serde(rename = "LocalizedExternal")]
    pub localized_external: Option<String>,
    #[serde(rename = "ChannelLayout")]
    pub channel_layout: Option<String>,
    #[serde(rename = "Channels")]
    pub channels: Option<i32>,
    #[serde(rename = "SampleRate")]
    pub sample_rate: Option<i32>,
    #[serde(rename = "DeliveryUrl")]
    pub delivery_url: Option<String>,
    #[serde(rename = "DeliveryMethod")]
    pub delivery_method: Option<String>,
}

#[skip_serializing_none]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Chapter {
    #[serde(rename = "StartPositionTicks")]
    pub start_position_ticks: Option<i64>,
    #[serde(rename = "Name")]
    pub name: Option<String>,
    #[serde(rename = "ImagePath")]
    pub image_path: Option<String>,
    #[serde(rename = "ImageDateModified")]
    pub image_date_modified: Option<String>,
    #[serde(rename = "ImageTag")]
    pub image_tag: Option<String>,
}

#[skip_serializing_none]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RemoteTrailer {
    #[serde(rename = "Url")]
    pub url: String,
    #[serde(rename = "Name")]
    pub name: Option<String>,
}

#[skip_serializing_none]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Person {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Role")]
    pub role: Option<String>,
    #[serde(rename = "Type")]
    pub person_type: String,
    #[serde(rename = "PrimaryImageTag")]
    pub primary_image_tag: Option<String>,
    #[serde(rename = "ImageBlurHashes")]
    pub image_blur_hashes: Option<ImageBlurHashes>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Studio {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Id")]
    pub id: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GenreItem {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Id")]
    pub id: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ExternalUrl {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Url")]
    pub url: String,
}

#[skip_serializing_none]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UserData {
    #[serde(rename = "PlaybackPositionTicks")]
    pub playback_position_ticks: i64,
    #[serde(rename = "PlayCount")]
    pub play_count: i32,
    #[serde(rename = "IsFavorite")]
    pub is_favorite: bool,
    #[serde(rename = "Played")]
    pub played: bool,
    #[serde(rename = "Key")]
    pub key: String,
    #[serde(rename = "ItemId")]
    pub item_id: String,
    #[serde(rename = "PlayedPercentage")]
    pub played_percentage: Option<f64>,
    #[serde(rename = "LastPlayedDate")]
    pub last_played_date: Option<String>,
    #[serde(rename = "UnplayedItemCount")]
    pub unplayed_item_count: Option<i32>,
}

pub type ImageTags = std::collections::HashMap<String, String>;

pub type ImageBlurHashes =
    std::collections::HashMap<String, std::collections::HashMap<String, String>>;
