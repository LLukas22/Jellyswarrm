use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaseItemKind {
    AggregateFolder,
    Audio,
    AudioBook,
    BasePluginFolder,
    Book,
    BoxSet,
    Channel,
    ChannelFolderItem,
    CollectionFolder,
    Episode,
    Folder,
    Genre,
    ManualPlaylistsFolder,
    Movie,
    LiveTvChannel,
    LiveTvProgram,
    MusicAlbum,
    MusicArtist,
    MusicGenre,
    MusicVideo,
    Person,
    Photo,
    PhotoAlbum,
    Playlist,
    PlaylistsFolder,
    Program,
    Recording,
    Season,
    Series,
    Studio,
    Trailer,
    TvChannel,
    TvProgram,
    UserRootFolder,
    UserView,
    Video,
    Year,
    Unknown(String),
}

impl Serialize for BaseItemKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            BaseItemKind::AggregateFolder => serializer.serialize_str("AggregateFolder"),
            BaseItemKind::Audio => serializer.serialize_str("Audio"),
            BaseItemKind::AudioBook => serializer.serialize_str("AudioBook"),
            BaseItemKind::BasePluginFolder => serializer.serialize_str("BasePluginFolder"),
            BaseItemKind::Book => serializer.serialize_str("Book"),
            BaseItemKind::BoxSet => serializer.serialize_str("BoxSet"),
            BaseItemKind::Channel => serializer.serialize_str("Channel"),
            BaseItemKind::ChannelFolderItem => serializer.serialize_str("ChannelFolderItem"),
            BaseItemKind::CollectionFolder => serializer.serialize_str("CollectionFolder"),
            BaseItemKind::Episode => serializer.serialize_str("Episode"),
            BaseItemKind::Folder => serializer.serialize_str("Folder"),
            BaseItemKind::Genre => serializer.serialize_str("Genre"),
            BaseItemKind::ManualPlaylistsFolder => {
                serializer.serialize_str("ManualPlaylistsFolder")
            }
            BaseItemKind::Movie => serializer.serialize_str("Movie"),
            BaseItemKind::LiveTvChannel => serializer.serialize_str("LiveTvChannel"),
            BaseItemKind::LiveTvProgram => serializer.serialize_str("LiveTvProgram"),
            BaseItemKind::MusicAlbum => serializer.serialize_str("MusicAlbum"),
            BaseItemKind::MusicArtist => serializer.serialize_str("MusicArtist"),
            BaseItemKind::MusicGenre => serializer.serialize_str("MusicGenre"),
            BaseItemKind::MusicVideo => serializer.serialize_str("MusicVideo"),
            BaseItemKind::Person => serializer.serialize_str("Person"),
            BaseItemKind::Photo => serializer.serialize_str("Photo"),
            BaseItemKind::PhotoAlbum => serializer.serialize_str("PhotoAlbum"),
            BaseItemKind::Playlist => serializer.serialize_str("Playlist"),
            BaseItemKind::PlaylistsFolder => serializer.serialize_str("PlaylistsFolder"),
            BaseItemKind::Program => serializer.serialize_str("Program"),
            BaseItemKind::Recording => serializer.serialize_str("Recording"),
            BaseItemKind::Season => serializer.serialize_str("Season"),
            BaseItemKind::Series => serializer.serialize_str("Series"),
            BaseItemKind::Studio => serializer.serialize_str("Studio"),
            BaseItemKind::Trailer => serializer.serialize_str("Trailer"),
            BaseItemKind::TvChannel => serializer.serialize_str("TvChannel"),
            BaseItemKind::TvProgram => serializer.serialize_str("TvProgram"),
            BaseItemKind::UserRootFolder => serializer.serialize_str("UserRootFolder"),
            BaseItemKind::UserView => serializer.serialize_str("UserView"),
            BaseItemKind::Video => serializer.serialize_str("Video"),
            BaseItemKind::Year => serializer.serialize_str("Year"),
            BaseItemKind::Unknown(s) => serializer.serialize_str(s),
        }
    }
}

impl<'de> Deserialize<'de> for BaseItemKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "AggregateFolder" => Ok(BaseItemKind::AggregateFolder),
            "Audio" => Ok(BaseItemKind::Audio),
            "AudioBook" => Ok(BaseItemKind::AudioBook),
            "BasePluginFolder" => Ok(BaseItemKind::BasePluginFolder),
            "Book" => Ok(BaseItemKind::Book),
            "BoxSet" => Ok(BaseItemKind::BoxSet),
            "Channel" => Ok(BaseItemKind::Channel),
            "ChannelFolderItem" => Ok(BaseItemKind::ChannelFolderItem),
            "CollectionFolder" => Ok(BaseItemKind::CollectionFolder),
            "Episode" => Ok(BaseItemKind::Episode),
            "Folder" => Ok(BaseItemKind::Folder),
            "Genre" => Ok(BaseItemKind::Genre),
            "ManualPlaylistsFolder" => Ok(BaseItemKind::ManualPlaylistsFolder),
            "Movie" => Ok(BaseItemKind::Movie),
            "LiveTvChannel" => Ok(BaseItemKind::LiveTvChannel),
            "LiveTvProgram" => Ok(BaseItemKind::LiveTvProgram),
            "MusicAlbum" => Ok(BaseItemKind::MusicAlbum),
            "MusicArtist" => Ok(BaseItemKind::MusicArtist),
            "MusicGenre" => Ok(BaseItemKind::MusicGenre),
            "MusicVideo" => Ok(BaseItemKind::MusicVideo),
            "Person" => Ok(BaseItemKind::Person),
            "Photo" => Ok(BaseItemKind::Photo),
            "PhotoAlbum" => Ok(BaseItemKind::PhotoAlbum),
            "Playlist" => Ok(BaseItemKind::Playlist),
            "PlaylistsFolder" => Ok(BaseItemKind::PlaylistsFolder),
            "Program" => Ok(BaseItemKind::Program),
            "Recording" => Ok(BaseItemKind::Recording),
            "Season" => Ok(BaseItemKind::Season),
            "Series" => Ok(BaseItemKind::Series),
            "Studio" => Ok(BaseItemKind::Studio),
            "Trailer" => Ok(BaseItemKind::Trailer),
            "TvChannel" => Ok(BaseItemKind::TvChannel),
            "TvProgram" => Ok(BaseItemKind::TvProgram),
            "UserRootFolder" => Ok(BaseItemKind::UserRootFolder),
            "UserView" => Ok(BaseItemKind::UserView),
            "Video" => Ok(BaseItemKind::Video),
            "Year" => Ok(BaseItemKind::Year),
            _ => Ok(BaseItemKind::Unknown(s)),
        }
    }
}

impl Default for BaseItemKind {
    fn default() -> Self {
        Self::Unknown("".to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CollectionType {
    #[default]
    Unknown,
    Movies,
    TvShows,
    Music,
    MusicVideos,
    Trailers,
    HomeVideos,
    BoxSets,
    Books,
    Photos,
    LiveTv,
    Playlists,
    Folders,
    UnknownVariant(String),
}

impl Serialize for CollectionType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            CollectionType::Unknown => serializer.serialize_str("unknown"),
            CollectionType::Movies => serializer.serialize_str("movies"),
            CollectionType::TvShows => serializer.serialize_str("tvshows"),
            CollectionType::Music => serializer.serialize_str("music"),
            CollectionType::MusicVideos => serializer.serialize_str("musicvideos"),
            CollectionType::Trailers => serializer.serialize_str("trailers"),
            CollectionType::HomeVideos => serializer.serialize_str("homevideos"),
            CollectionType::BoxSets => serializer.serialize_str("boxsets"),
            CollectionType::Books => serializer.serialize_str("books"),
            CollectionType::Photos => serializer.serialize_str("photos"),
            CollectionType::LiveTv => serializer.serialize_str("livetv"),
            CollectionType::Playlists => serializer.serialize_str("playlists"),
            CollectionType::Folders => serializer.serialize_str("folders"),
            CollectionType::UnknownVariant(s) => serializer.serialize_str(s),
        }
    }
}

impl<'de> Deserialize<'de> for CollectionType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "unknown" => Ok(CollectionType::Unknown),
            "movies" => Ok(CollectionType::Movies),
            "tvshows" => Ok(CollectionType::TvShows),
            "music" => Ok(CollectionType::Music),
            "musicvideos" => Ok(CollectionType::MusicVideos),
            "trailers" => Ok(CollectionType::Trailers),
            "homevideos" => Ok(CollectionType::HomeVideos),
            "boxsets" => Ok(CollectionType::BoxSets),
            "books" => Ok(CollectionType::Books),
            "photos" => Ok(CollectionType::Photos),
            "livetv" => Ok(CollectionType::LiveTv),
            "playlists" => Ok(CollectionType::Playlists),
            "folders" => Ok(CollectionType::Folders),
            _ => Ok(CollectionType::UnknownVariant(s)),
        }
    }
}
