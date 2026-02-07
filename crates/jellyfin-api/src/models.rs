use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPolicy {
    #[serde(rename = "IsAdministrator")]
    pub is_administrator: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "ServerId")]
    pub server_id: Option<String>,
    #[serde(rename = "Policy")]
    pub policy: Option<UserPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthResponse {
    #[serde(rename = "AccessToken")]
    pub access_token: String,
    #[serde(rename = "User")]
    pub user: User,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaFolder {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "CollectionType")]
    pub collection_type: Option<String>,
    #[serde(rename = "Id")]
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaFoldersResponse {
    #[serde(rename = "Items")]
    pub items: Vec<MediaFolder>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewUserRequest {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Password")]
    pub password: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PublicSystemInfo {
    #[serde(rename = "LocalAddress")]
    pub local_address: Option<String>,
    #[serde(rename = "ServerName")]
    pub server_name: Option<String>,
    #[serde(rename = "Version")]
    pub version: Option<String>,
    #[serde(rename = "ProductName")]
    pub product_name: Option<String>,
    #[serde(rename = "Id")]
    pub id: Option<String>,
    #[serde(rename = "StartupWizardCompleted")]
    pub startup_wizard_completed: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IncludeBaseItemFields {
    #[serde(rename = "AirTime")]
    AirTime,
    #[serde(rename = "CanDelete")]
    CanDelete,
    #[serde(rename = "CanDownload")]
    CanDownload,
    #[serde(rename = "ChannelInfo")]
    ChannelInfo,
    #[serde(rename = "Chapters")]
    Chapters,
    #[serde(rename = "Trickplay")]
    Trickplay,
    #[serde(rename = "ChildCount")]
    ChildCount,
    #[serde(rename = "CumulativeRunTimeTicks")]
    CumulativeRunTimeTicks,
    #[serde(rename = "CustomRating")]
    CustomRating,
    #[serde(rename = "DateCreated")]
    DateCreated,
    #[serde(rename = "DateLastMediaAdded")]
    DateLastMediaAdded,
    #[serde(rename = "DisplayPreferencesId")]
    DisplayPreferencesId,
    #[serde(rename = "Etag")]
    Etag,
    #[serde(rename = "ExternalUrls")]
    ExternalUrls,
    #[serde(rename = "Genres")]
    Genres,
    #[serde(rename = "ItemCounts")]
    ItemCounts,
    #[serde(rename = "MediaSourceCount")]
    MediaSourceCount,
    #[serde(rename = "MediaSources")]
    MediaSources,
    #[serde(rename = "OriginalTitle")]
    OriginalTitle,
    #[serde(rename = "Overview")]
    Overview,
    #[serde(rename = "ParentId")]
    ParentId,
    #[serde(rename = "Path")]
    Path,
    #[serde(rename = "People")]
    People,
    #[serde(rename = "PlayAccess")]
    PlayAccess,
    #[serde(rename = "ProductionLocations")]
    ProductionLocations,
    #[serde(rename = "ProviderIds")]
    ProviderIds,
    #[serde(rename = "PrimaryImageAspectRatio")]
    PrimaryImageAspectRatio,
    #[serde(rename = "RecursiveItemCount")]
    RecursiveItemCount,
    #[serde(rename = "Settings")]
    Settings,
    #[serde(rename = "SeriesStudio")]
    SeriesStudio,
    #[serde(rename = "SortName")]
    SortName,
    #[serde(rename = "SpecialEpisodeNumbers")]
    SpecialEpisodeNumbers,
    #[serde(rename = "Studios")]
    Studios,
    #[serde(rename = "Taglines")]
    Taglines,
    #[serde(rename = "Tags")]
    Tags,
    #[serde(rename = "RemoteTrailers")]
    RemoteTrailers,
    #[serde(rename = "MediaStreams")]
    MediaStreams,
    #[serde(rename = "SeasonUserData")]
    SeasonUserData,
    #[serde(rename = "DateLastRefreshed")]
    DateLastRefreshed,
    #[serde(rename = "DateLastSaved")]
    DateLastSaved,
    #[serde(rename = "RefreshState")]
    RefreshState,
    #[serde(rename = "ChannelImage")]
    ChannelImage,
    #[serde(rename = "EnableMediaSourceDisplay")]
    EnableMediaSourceDisplay,
    #[serde(rename = "Width")]
    Width,
    #[serde(rename = "Height")]
    Height,
    #[serde(rename = "ExtraIds")]
    ExtraIds,
    #[serde(rename = "LocalTrailerCount")]
    LocalTrailerCount,
    #[serde(rename = "IsHD")]
    IsHD,
    #[serde(rename = "SpecialFeatureCount")]
    SpecialFeatureCount,
}

impl std::fmt::Display for IncludeBaseItemFields {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match serde_plain::to_string(self) {
            Ok(value) => f.write_str(&value),
            Err(_) => Err(std::fmt::Error),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IncludeItemTypes {
    #[serde(rename = "AggregateFolder")]
    AggregateFolder,
    #[serde(rename = "Audio")]
    Audio,
    #[serde(rename = "AudioBook")]
    AudioBook,
    #[serde(rename = "BasePluginFolder")]
    BasePluginFolder,
    #[serde(rename = "Book")]
    Book,
    #[serde(rename = "BoxSet")]
    BoxSet,
    #[serde(rename = "Channel")]
    Channel,
    #[serde(rename = "ChannelFolderItem")]
    ChannelFolderItem,
    #[serde(rename = "CollectionFolder")]
    CollectionFolder,
    #[serde(rename = "Episode")]
    Episode,
    #[serde(rename = "Folder")]
    Folder,
    #[serde(rename = "Genre")]
    Genre,
    #[serde(rename = "ManualPlaylistsFolder")]
    ManualPlaylistsFolder,
    #[serde(rename = "Movie")]
    Movie,
    #[serde(rename = "LiveTvChannel")]
    LiveTvChannel,
    #[serde(rename = "LiveTvProgram")]
    LiveTvProgram,
    #[serde(rename = "MusicAlbum")]
    MusicAlbum,
    #[serde(rename = "MusicArtist")]
    MusicArtist,
    #[serde(rename = "MusicGenre")]
    MusicGenre,
    #[serde(rename = "MusicVideo")]
    MusicVideo,
    #[serde(rename = "Person")]
    Person,
    #[serde(rename = "Photo")]
    Photo,
    #[serde(rename = "PhotoAlbum")]
    PhotoAlbum,
    #[serde(rename = "Playlist")]
    Playlist,
    #[serde(rename = "PlaylistsFolder")]
    PlaylistsFolder,
    #[serde(rename = "Program")]
    Program,
    #[serde(rename = "Recording")]
    Recording,
    #[serde(rename = "Season")]
    Season,
    #[serde(rename = "Series")]
    Series,
    #[serde(rename = "Studio")]
    Studio,
    #[serde(rename = "Trailer")]
    Trailer,
    #[serde(rename = "TvChannel")]
    TvChannel,
    #[serde(rename = "TvProgram")]
    TvProgram,
    #[serde(rename = "UserRootFolder")]
    UserRootFolder,
    #[serde(rename = "UserView")]
    UserView,
    #[serde(rename = "Video")]
    Video,
    #[serde(rename = "Year")]
    Year,
}

impl std::fmt::Display for IncludeItemTypes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match serde_plain::to_string(self) {
            Ok(value) => f.write_str(&value),
            Err(_) => Err(std::fmt::Error),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaseItem {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Type")]
    pub type_: String,
    #[serde(rename = "ImageTags")]
    pub image_tags: Option<std::collections::HashMap<String, String>>,
    #[serde(rename = "ProductionYear")]
    pub production_year: Option<i32>,
    #[serde(rename = "RunTimeTicks")]
    pub run_time_ticks: Option<i64>,
    #[serde(rename = "CommunityRating")]
    pub community_rating: Option<f32>,
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemsResponse {
    #[serde(rename = "Items")]
    pub items: Vec<BaseItem>,
    #[serde(rename = "TotalRecordCount")]
    pub total_record_count: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrandingConfiguration {
    #[serde(rename = "LoginDisclaimer")]
    pub login_disclaimer: Option<String>,
    #[serde(rename = "CustomCss")]
    pub custom_css: Option<String>,
    #[serde(rename = "SplashscreenEnabled")]
    pub splashscreen_enabled: Option<bool>,
}
