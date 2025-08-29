use serde::{Deserialize, Serialize, Serializer};

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum StreamIndex {
    Int(i32),
    Str(String),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PlaybackRequest {
    #[serde(
        rename = "AlwaysBurnInSubtitleWhenTranscoding",
        skip_serializing_if = "Option::is_none"
    )]
    pub always_burn_in_subtitle_when_transcoding: Option<bool>,
    #[serde(rename = "AudioStreamIndex", skip_serializing_if = "Option::is_none")]
    pub audio_stream_index: Option<StreamIndex>,
    #[serde(rename = "AutoOpenLiveStream", skip_serializing_if = "Option::is_none")]
    pub auto_open_live_stream: Option<bool>,
    #[serde(rename = "IsPlayback", skip_serializing_if = "Option::is_none")]
    pub is_playback: Option<bool>,
    #[serde(
        rename = "MaxStreamingBitrate",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_streaming_bitrate: Option<i64>,
    #[serde(rename = "MediaSourceId", skip_serializing_if = "Option::is_none")]
    pub media_source_id: Option<String>,
    #[serde(rename = "StartTimeTicks", skip_serializing_if = "Option::is_none")]
    pub start_time_ticks: Option<i64>,
    #[serde(
        rename = "SubtitleStreamIndex",
        skip_serializing_if = "Option::is_none"
    )]
    pub subtitle_stream_index: Option<StreamIndex>,
    #[serde(rename = "UserId")]
    pub user_id: String,

    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

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

#[derive(Debug, Serialize, Deserialize)]
pub struct ProgressRequest {
    #[serde(rename = "AudioStreamIndex", skip_serializing_if = "Option::is_none")]
    pub audio_stream_index: Option<StreamIndex>,
    #[serde(rename = "BufferedRanges", skip_serializing_if = "Option::is_none")]
    pub buffered_ranges: Option<serde_json::Value>,
    #[serde(rename = "CanSeek", skip_serializing_if = "Option::is_none")]
    pub can_seek: Option<bool>,
    #[serde(rename = "EventName", skip_serializing_if = "Option::is_none")]
    pub event_name: Option<String>,
    #[serde(rename = "IsMuted")]
    pub is_muted: bool,
    #[serde(rename = "IsPaused")]
    pub is_paused: bool,
    #[serde(rename = "ItemId")]
    pub item_id: String,
    #[serde(
        rename = "MaxStreamingBitrate",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_streaming_bitrate: Option<i64>,
    #[serde(rename = "MediaSourceId")]
    pub media_source_id: String,
    #[serde(rename = "NowPlayingQueue", skip_serializing_if = "Option::is_none")]
    pub now_playing_queue: Option<Vec<NowPlayingQueueItem>>,
    #[serde(
        rename = "PlaybackRate",
        serialize_with = "serialize_playback_rate",
        skip_serializing_if = "Option::is_none"
    )]
    pub playback_rate: Option<f64>,
    #[serde(
        rename = "PlaybackStartTimeTicks",
        skip_serializing_if = "Option::is_none"
    )]
    pub playback_start_time_ticks: Option<i64>,
    #[serde(rename = "PlaylistItemId", skip_serializing_if = "Option::is_none")]
    pub playlist_item_id: Option<String>,
    #[serde(rename = "PlayMethod")]
    pub play_method: String,
    #[serde(rename = "PlaySessionId")]
    pub play_session_id: String,
    #[serde(rename = "PositionTicks")]
    pub position_ticks: i64,
    #[serde(rename = "RepeatMode")]
    pub repeat_mode: String,
    #[serde(
        rename = "SecondarySubtitleStreamIndex",
        skip_serializing_if = "Option::is_none"
    )]
    pub secondary_subtitle_stream_index: Option<StreamIndex>,
    #[serde(rename = "ShuffleMode", skip_serializing_if = "Option::is_none")]
    pub shuffle_mode: Option<String>,
    #[serde(
        rename = "SubtitleStreamIndex",
        skip_serializing_if = "Option::is_none"
    )]
    pub subtitle_stream_index: Option<StreamIndex>,
    #[serde(rename = "VolumeLevel", skip_serializing_if = "Option::is_none")]
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

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ServerInfo {
    #[serde(
        rename = "OperatingSystemDisplayName",
        skip_serializing_if = "Option::is_none"
    )]
    pub operating_system_display_name: Option<String>,

    #[serde(rename = "HasPendingRestart", skip_serializing_if = "Option::is_none")]
    pub has_pending_restart: Option<bool>,

    #[serde(rename = "IsShuttingDown", skip_serializing_if = "Option::is_none")]
    pub is_shutting_down: Option<bool>,

    #[serde(
        rename = "SupportsLibraryMonitor",
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_library_monitor: Option<bool>,

    #[serde(
        rename = "WebSocketPortNumber",
        skip_serializing_if = "Option::is_none"
    )]
    pub web_socket_port_number: Option<i32>,

    #[serde(
        rename = "CompletedInstallations",
        skip_serializing_if = "Option::is_none"
    )]
    pub completed_installations: Option<serde_json::Value>,

    #[serde(rename = "CanSelfRestart", skip_serializing_if = "Option::is_none")]
    pub can_self_restart: Option<bool>,

    #[serde(
        rename = "CanLaunchWebBrowser",
        skip_serializing_if = "Option::is_none"
    )]
    pub can_launch_web_browser: Option<bool>,

    #[serde(rename = "ProgramDataPath", skip_serializing_if = "Option::is_none")]
    pub program_data_path: Option<String>,

    #[serde(rename = "WebPath", skip_serializing_if = "Option::is_none")]
    pub web_path: Option<String>,

    #[serde(rename = "ItemsByNamePath", skip_serializing_if = "Option::is_none")]
    pub items_by_name_path: Option<String>,

    #[serde(rename = "CachePath", skip_serializing_if = "Option::is_none")]
    pub cache_path: Option<String>,

    #[serde(rename = "LogPath", skip_serializing_if = "Option::is_none")]
    pub log_path: Option<String>,

    #[serde(
        rename = "InternalMetadataPath",
        skip_serializing_if = "Option::is_none"
    )]
    pub internal_metadata_path: Option<String>,

    #[serde(
        rename = "TranscodingTempPath",
        skip_serializing_if = "Option::is_none"
    )]
    pub transcoding_temp_path: Option<String>,

    #[serde(
        rename = "CastReceiverApplications",
        skip_serializing_if = "Option::is_none"
    )]
    pub cast_receiver_applications: Option<Vec<CastReceiverApplication>>,

    #[serde(rename = "HasUpdateAvailable", skip_serializing_if = "Option::is_none")]
    pub has_update_available: Option<bool>,

    #[serde(rename = "EncoderLocation", skip_serializing_if = "Option::is_none")]
    pub encoder_location: Option<String>,

    #[serde(rename = "SystemArchitecture", skip_serializing_if = "Option::is_none")]
    pub system_architecture: Option<String>,

    #[serde(rename = "LocalAddress")]
    pub local_address: String,

    #[serde(rename = "ServerName")]
    pub server_name: String,

    #[serde(rename = "Version", skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,

    #[serde(rename = "OperatingSystem", skip_serializing_if = "Option::is_none")]
    pub operating_system: Option<String>,

    #[serde(rename = "Id")]
    pub id: String,

    #[serde(
        rename = "StartupWizardCompleted",
        skip_serializing_if = "Option::is_none"
    )]
    pub startup_wizard_completed: Option<bool>,
}

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

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MediaItem {
    #[serde(rename = "Name", skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "ServerId", skip_serializing_if = "Option::is_none")]
    pub server_id: Option<String>,
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "ItemId", skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(rename = "SeriesId", skip_serializing_if = "Option::is_none")]
    pub series_id: Option<String>,
    #[serde(rename = "SeriesName", skip_serializing_if = "Option::is_none")]
    pub series_name: Option<String>,
    #[serde(rename = "SeasonId", skip_serializing_if = "Option::is_none")]
    pub season_id: Option<String>,
    #[serde(rename = "Etag", skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(rename = "DateCreated", skip_serializing_if = "Option::is_none")]
    pub date_created: Option<String>,
    #[serde(rename = "CanDelete", skip_serializing_if = "Option::is_none")]
    pub can_delete: Option<bool>,
    #[serde(rename = "CanDownload", skip_serializing_if = "Option::is_none")]
    pub can_download: Option<bool>,
    #[serde(rename = "SortName", skip_serializing_if = "Option::is_none")]
    pub sort_name: Option<String>,
    #[serde(rename = "ExternalUrls", skip_serializing_if = "Option::is_none")]
    pub external_urls: Option<Vec<ExternalUrl>>,
    #[serde(rename = "Path", skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(
        rename = "EnableMediaSourceDisplay",
        skip_serializing_if = "Option::is_none"
    )]
    pub enable_media_source_display: Option<bool>,
    #[serde(rename = "ChannelId")]
    pub channel_id: Option<String>,
    #[serde(rename = "Taglines", skip_serializing_if = "Option::is_none")]
    pub taglines: Option<Vec<String>>,
    #[serde(rename = "Genres", skip_serializing_if = "Option::is_none")]
    pub genres: Option<Vec<String>>,
    #[serde(rename = "PlayAccess", skip_serializing_if = "Option::is_none")]
    pub play_access: Option<String>,
    #[serde(rename = "RemoteTrailers", skip_serializing_if = "Option::is_none")]
    pub remote_trailers: Option<Vec<RemoteTrailer>>,
    #[serde(rename = "ProviderIds", skip_serializing_if = "Option::is_none")]
    pub provider_ids: Option<serde_json::Value>,
    #[serde(rename = "IsFolder", skip_serializing_if = "Option::is_none")]
    pub is_folder: Option<bool>,
    #[serde(rename = "ParentId", skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(rename = "ParentLogoItemId", skip_serializing_if = "Option::is_none")]
    pub parent_logo_item_id: Option<String>,
    #[serde(
        rename = "ParentBackdropItemId",
        skip_serializing_if = "Option::is_none"
    )]
    pub parent_backdrop_item_id: Option<String>,
    #[serde(
        rename = "ParentBackdropImageTags",
        skip_serializing_if = "Option::is_none"
    )]
    pub parent_backdrop_image_tags: Option<Vec<String>>,
    #[serde(rename = "ParentLogoImageTag", skip_serializing_if = "Option::is_none")]
    pub parent_logo_image_tag: Option<String>,
    #[serde(rename = "ParentThumbItemId", skip_serializing_if = "Option::is_none")]
    pub parent_thumb_item_id: Option<String>,
    #[serde(
        rename = "ParentThumbImageTag",
        skip_serializing_if = "Option::is_none"
    )]
    pub parent_thumb_image_tag: Option<String>,
    #[serde(rename = "Type")]
    pub item_type: String,
    #[serde(rename = "People", skip_serializing_if = "Option::is_none")]
    pub people: Option<Vec<Person>>,
    #[serde(rename = "Studios", skip_serializing_if = "Option::is_none")]
    pub studios: Option<Vec<Studio>>,
    #[serde(rename = "GenreItems", skip_serializing_if = "Option::is_none")]
    pub genre_items: Option<Vec<GenreItem>>,
    #[serde(rename = "LocalTrailerCount", skip_serializing_if = "Option::is_none")]
    pub local_trailer_count: Option<i32>,
    #[serde(rename = "UserData", skip_serializing_if = "Option::is_none")]
    pub user_data: Option<UserData>,
    #[serde(rename = "ChildCount", skip_serializing_if = "Option::is_none")]
    pub child_count: Option<i32>,
    #[serde(
        rename = "SpecialFeatureCount",
        skip_serializing_if = "Option::is_none"
    )]
    pub special_feature_count: Option<i32>,
    #[serde(
        rename = "DisplayPreferencesId",
        skip_serializing_if = "Option::is_none"
    )]
    pub display_preferences_id: Option<String>,
    #[serde(rename = "Tags", skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(
        rename = "PrimaryImageAspectRatio",
        skip_serializing_if = "Option::is_none"
    )]
    pub primary_image_aspect_ratio: Option<f64>,
    #[serde(
        rename = "SeriesPrimaryImageTag",
        skip_serializing_if = "Option::is_none"
    )]
    pub series_primary_image_tag: Option<String>,
    #[serde(rename = "CollectionType", skip_serializing_if = "Option::is_none")]
    pub collection_type: Option<String>,
    #[serde(rename = "ImageTags", skip_serializing_if = "Option::is_none")]
    pub image_tags: Option<ImageTags>,
    #[serde(rename = "BackdropImageTags", skip_serializing_if = "Option::is_none")]
    pub backdrop_image_tags: Option<Vec<String>>,
    #[serde(rename = "ImageBlurHashes", skip_serializing_if = "Option::is_none")]
    pub image_blur_hashes: Option<ImageBlurHashes>,
    #[serde(rename = "LocationType", skip_serializing_if = "Option::is_none")]
    pub location_type: Option<String>,
    #[serde(rename = "MediaType", skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(rename = "LockedFields", skip_serializing_if = "Option::is_none")]
    pub locked_fields: Option<Vec<String>>,
    #[serde(rename = "LockData", skip_serializing_if = "Option::is_none")]
    pub lock_data: Option<bool>,
    // New fields from the provided response
    #[serde(rename = "Container", skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(rename = "PremiereDate", skip_serializing_if = "Option::is_none")]
    pub premiere_date: Option<String>,
    #[serde(rename = "CriticRating", skip_serializing_if = "Option::is_none")]
    pub critic_rating: Option<i32>,
    #[serde(rename = "OfficialRating", skip_serializing_if = "Option::is_none")]
    pub official_rating: Option<String>,
    #[serde(rename = "CommunityRating", skip_serializing_if = "Option::is_none")]
    pub community_rating: Option<f64>,
    #[serde(rename = "RunTimeTicks", skip_serializing_if = "Option::is_none")]
    pub run_time_ticks: Option<i64>,
    #[serde(rename = "ProductionYear", skip_serializing_if = "Option::is_none")]
    pub production_year: Option<i32>,
    #[serde(rename = "VideoType", skip_serializing_if = "Option::is_none")]
    pub video_type: Option<String>,
    #[serde(rename = "HasSubtitles", skip_serializing_if = "Option::is_none")]
    pub has_subtitles: Option<bool>,
    #[serde(rename = "OriginalTitle", skip_serializing_if = "Option::is_none")]
    pub original_title: Option<String>,
    #[serde(rename = "Overview", skip_serializing_if = "Option::is_none")]
    pub overview: Option<String>,
    #[serde(
        rename = "ProductionLocations",
        skip_serializing_if = "Option::is_none"
    )]
    pub production_locations: Option<Vec<String>>,
    #[serde(rename = "IsHD", skip_serializing_if = "Option::is_none")]
    pub is_hd: Option<bool>,
    #[serde(rename = "Width", skip_serializing_if = "Option::is_none")]
    pub width: Option<i32>,
    #[serde(rename = "Height", skip_serializing_if = "Option::is_none")]
    pub height: Option<i32>,
    #[serde(rename = "MediaSources", skip_serializing_if = "Option::is_none")]
    pub media_sources: Option<Vec<MediaSource>>,
    #[serde(rename = "MediaStreams", skip_serializing_if = "Option::is_none")]
    pub media_streams: Option<Vec<MediaStream>>,
    #[serde(rename = "Chapters", skip_serializing_if = "Option::is_none")]
    pub chapters: Option<Vec<Chapter>>,
    #[serde(rename = "Trickplay", skip_serializing_if = "Option::is_none")]
    pub trickplay: Option<std::collections::HashMap<String, serde_json::Value>>,
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MediaSource {
    #[serde(rename = "Protocol", skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Path", skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(rename = "Type", skip_serializing_if = "Option::is_none")]
    pub source_type: Option<String>,
    #[serde(rename = "Container", skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(rename = "Size", skip_serializing_if = "Option::is_none")]
    pub size: Option<i64>,
    #[serde(rename = "Name", skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "IsRemote", skip_serializing_if = "Option::is_none")]
    pub is_remote: Option<bool>,
    #[serde(rename = "ETag", skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(rename = "RunTimeTicks", skip_serializing_if = "Option::is_none")]
    pub run_time_ticks: Option<i64>,
    #[serde(
        rename = "ReadAtNativeFramerate",
        skip_serializing_if = "Option::is_none"
    )]
    pub read_at_native_framerate: Option<bool>,
    #[serde(rename = "IgnoreDts", skip_serializing_if = "Option::is_none")]
    pub ignore_dts: Option<bool>,
    #[serde(rename = "IgnoreIndex", skip_serializing_if = "Option::is_none")]
    pub ignore_index: Option<bool>,
    #[serde(rename = "GenPtsInput", skip_serializing_if = "Option::is_none")]
    pub gen_pts_input: Option<bool>,
    #[serde(
        rename = "SupportsTranscoding",
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_transcoding: Option<bool>,
    #[serde(
        rename = "SupportsDirectStream",
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_direct_stream: Option<bool>,
    #[serde(rename = "SupportsDirectPlay", skip_serializing_if = "Option::is_none")]
    pub supports_direct_play: Option<bool>,
    #[serde(rename = "IsInfiniteStream", skip_serializing_if = "Option::is_none")]
    pub is_infinite_stream: Option<bool>,
    #[serde(
        rename = "UseMostCompatibleTranscodingProfile",
        skip_serializing_if = "Option::is_none"
    )]
    pub use_most_compatible_transcoding_profile: Option<bool>,
    #[serde(rename = "RequiresOpening", skip_serializing_if = "Option::is_none")]
    pub requires_opening: Option<bool>,
    #[serde(rename = "RequiresClosing", skip_serializing_if = "Option::is_none")]
    pub requires_closing: Option<bool>,
    #[serde(rename = "RequiresLooping", skip_serializing_if = "Option::is_none")]
    pub requires_looping: Option<bool>,
    #[serde(rename = "SupportsProbing", skip_serializing_if = "Option::is_none")]
    pub supports_probing: Option<bool>,
    #[serde(rename = "VideoType", skip_serializing_if = "Option::is_none")]
    pub video_type: Option<String>,
    #[serde(rename = "MediaStreams", skip_serializing_if = "Option::is_none")]
    pub media_streams: Option<Vec<MediaStream>>,
    #[serde(rename = "MediaAttachments", skip_serializing_if = "Option::is_none")]
    pub media_attachments: Option<Vec<serde_json::Value>>,
    #[serde(rename = "Formats", skip_serializing_if = "Option::is_none")]
    pub formats: Option<Vec<String>>,
    #[serde(rename = "Bitrate", skip_serializing_if = "Option::is_none")]
    pub bitrate: Option<i64>,
    #[serde(
        rename = "RequiredHttpHeaders",
        skip_serializing_if = "Option::is_none"
    )]
    pub required_http_headers: Option<serde_json::Value>,
    #[serde(
        rename = "TranscodingSubProtocol",
        skip_serializing_if = "Option::is_none"
    )]
    pub transcoding_sub_protocol: Option<String>,
    #[serde(rename = "TranscodingUrl", skip_serializing_if = "Option::is_none")]
    pub transcoding_url: Option<String>,
    #[serde(
        rename = "TranscodingContainer",
        skip_serializing_if = "Option::is_none"
    )]
    pub transcoding_container: Option<String>,
    #[serde(
        rename = "DefaultAudioStreamIndex",
        skip_serializing_if = "Option::is_none"
    )]
    pub default_audio_stream_index: Option<i32>,
    #[serde(
        rename = "DefaultSubtitleStreamIndex",
        skip_serializing_if = "Option::is_none"
    )]
    pub default_subtitle_stream_index: Option<i32>,
    #[serde(rename = "HasSegments", skip_serializing_if = "Option::is_none")]
    pub has_segments: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MediaStream {
    #[serde(rename = "Codec", skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    #[serde(rename = "ColorSpace", skip_serializing_if = "Option::is_none")]
    pub color_space: Option<String>,
    #[serde(rename = "ColorTransfer", skip_serializing_if = "Option::is_none")]
    pub color_transfer: Option<String>,
    #[serde(rename = "ColorPrimaries", skip_serializing_if = "Option::is_none")]
    pub color_primaries: Option<String>,
    #[serde(rename = "DvVersionMajor", skip_serializing_if = "Option::is_none")]
    pub dv_version_major: Option<i32>,
    #[serde(rename = "DvVersionMinor", skip_serializing_if = "Option::is_none")]
    pub dv_version_minor: Option<i32>,
    #[serde(rename = "DvProfile", skip_serializing_if = "Option::is_none")]
    pub dv_profile: Option<i32>,
    #[serde(rename = "DvLevel", skip_serializing_if = "Option::is_none")]
    pub dv_level: Option<i32>,
    #[serde(rename = "RpuPresentFlag", skip_serializing_if = "Option::is_none")]
    pub rpu_present_flag: Option<i32>,
    #[serde(rename = "ElPresentFlag", skip_serializing_if = "Option::is_none")]
    pub el_present_flag: Option<i32>,
    #[serde(rename = "BlPresentFlag", skip_serializing_if = "Option::is_none")]
    pub bl_present_flag: Option<i32>,
    #[serde(
        rename = "DvBlSignalCompatibilityId",
        skip_serializing_if = "Option::is_none"
    )]
    pub dv_bl_signal_compatibility_id: Option<i32>,
    #[serde(rename = "TimeBase", skip_serializing_if = "Option::is_none")]
    pub time_base: Option<String>,
    #[serde(rename = "VideoRange", skip_serializing_if = "Option::is_none")]
    pub video_range: Option<String>,
    #[serde(rename = "VideoRangeType", skip_serializing_if = "Option::is_none")]
    pub video_range_type: Option<String>,
    #[serde(rename = "VideoDoViTitle", skip_serializing_if = "Option::is_none")]
    pub video_dovi_title: Option<String>,
    #[serde(rename = "AudioSpatialFormat", skip_serializing_if = "Option::is_none")]
    pub audio_spatial_format: Option<String>,
    #[serde(rename = "DisplayTitle", skip_serializing_if = "Option::is_none")]
    pub display_title: Option<String>,
    #[serde(rename = "IsInterlaced", skip_serializing_if = "Option::is_none")]
    pub is_interlaced: Option<bool>,
    #[serde(rename = "IsAVC", skip_serializing_if = "Option::is_none")]
    pub is_avc: Option<bool>,
    #[serde(rename = "BitRate", skip_serializing_if = "Option::is_none")]
    pub bit_rate: Option<i64>,
    #[serde(rename = "BitDepth", skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<i32>,
    #[serde(rename = "RefFrames", skip_serializing_if = "Option::is_none")]
    pub ref_frames: Option<i32>,
    #[serde(rename = "IsDefault", skip_serializing_if = "Option::is_none")]
    pub is_default: Option<bool>,
    #[serde(rename = "IsForced", skip_serializing_if = "Option::is_none")]
    pub is_forced: Option<bool>,
    #[serde(rename = "IsHearingImpaired", skip_serializing_if = "Option::is_none")]
    pub is_hearing_impaired: Option<bool>,
    #[serde(rename = "Height", skip_serializing_if = "Option::is_none")]
    pub height: Option<i32>,
    #[serde(rename = "Width", skip_serializing_if = "Option::is_none")]
    pub width: Option<i32>,
    #[serde(rename = "AverageFrameRate", skip_serializing_if = "Option::is_none")]
    pub average_frame_rate: Option<f64>,
    #[serde(rename = "RealFrameRate", skip_serializing_if = "Option::is_none")]
    pub real_frame_rate: Option<f64>,
    #[serde(rename = "ReferenceFrameRate", skip_serializing_if = "Option::is_none")]
    pub reference_frame_rate: Option<f64>,
    #[serde(rename = "Profile", skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(rename = "Type", skip_serializing_if = "Option::is_none")]
    pub stream_type: Option<String>,
    #[serde(rename = "AspectRatio", skip_serializing_if = "Option::is_none")]
    pub aspect_ratio: Option<String>,
    #[serde(rename = "Index")]
    pub index: i32,
    #[serde(rename = "IsExternal", skip_serializing_if = "Option::is_none")]
    pub is_external: Option<bool>,
    #[serde(
        rename = "IsTextSubtitleStream",
        skip_serializing_if = "Option::is_none"
    )]
    pub is_text_subtitle_stream: Option<bool>,
    #[serde(
        rename = "SupportsExternalStream",
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_external_stream: Option<bool>,
    #[serde(rename = "PixelFormat", skip_serializing_if = "Option::is_none")]
    pub pixel_format: Option<String>,
    #[serde(rename = "Level")]
    pub level: i32,
    #[serde(rename = "IsAnamorphic", skip_serializing_if = "Option::is_none")]
    pub is_anamorphic: Option<bool>,
    #[serde(rename = "Language", skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(rename = "Title", skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "LocalizedDefault", skip_serializing_if = "Option::is_none")]
    pub localized_default: Option<String>,
    #[serde(rename = "LocalizedExternal", skip_serializing_if = "Option::is_none")]
    pub localized_external: Option<String>,
    #[serde(rename = "ChannelLayout", skip_serializing_if = "Option::is_none")]
    pub channel_layout: Option<String>,
    #[serde(rename = "Channels", skip_serializing_if = "Option::is_none")]
    pub channels: Option<i32>,
    #[serde(rename = "SampleRate", skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<i32>,
    #[serde(rename = "DeliveryUrl", skip_serializing_if = "Option::is_none")]
    pub delivery_url: Option<String>,
    #[serde(rename = "DeliveryMethod", skip_serializing_if = "Option::is_none")]
    pub delivery_method: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Chapter {
    #[serde(rename = "StartPositionTicks", skip_serializing_if = "Option::is_none")]
    pub start_position_ticks: Option<i64>,
    #[serde(rename = "Name", skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "ImagePath", skip_serializing_if = "Option::is_none")]
    pub image_path: Option<String>,
    #[serde(rename = "ImageDateModified", skip_serializing_if = "Option::is_none")]
    pub image_date_modified: Option<String>,
    #[serde(rename = "ImageTag", skip_serializing_if = "Option::is_none")]
    pub image_tag: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RemoteTrailer {
    #[serde(rename = "Url")]
    pub url: String,
    #[serde(rename = "Name", skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Person {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Role", skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(rename = "Type")]
    pub person_type: String,
    #[serde(rename = "PrimaryImageTag", skip_serializing_if = "Option::is_none")]
    pub primary_image_tag: Option<String>,
    #[serde(rename = "ImageBlurHashes", skip_serializing_if = "Option::is_none")]
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
    #[serde(rename = "PlayedPercentage", skip_serializing_if = "Option::is_none")]
    pub played_percentage: Option<f64>,
    #[serde(rename = "LastPlayedDate", skip_serializing_if = "Option::is_none")]
    pub last_played_date: Option<String>,
    #[serde(rename = "UnplayedItemCount", skip_serializing_if = "Option::is_none")]
    pub unplayed_item_count: Option<i32>,
}

pub type ImageTags = std::collections::HashMap<String, String>;

pub type ImageBlurHashes =
    std::collections::HashMap<String, std::collections::HashMap<String, String>>;
