use serde::{Deserialize, Serialize};
use serde_default::DefaultFromSerde;
use sqlx::migrate::Migrator;
use std::fmt;
use std::fs;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::LazyLock;
use tower_sessions::cookie::Key;
use tracing::info;
use uuid::Uuid;

use jellyfin_api::ClientInfo;

use base64::prelude::*;

use crate::encryption::Password;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaStreamingMode {
    Redirect,
    Proxy,
}

impl std::str::FromStr for MediaStreamingMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "redirect" => Ok(MediaStreamingMode::Redirect),
            "proxy" => Ok(MediaStreamingMode::Proxy),
            _ => Err(format!("Invalid media streaming mode: {}", s)),
        }
    }
}

impl fmt::Display for MediaStreamingMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MediaStreamingMode::Redirect => write!(f, "Redirect"),
            MediaStreamingMode::Proxy => write!(f, "Proxy"),
        }
    }
}

pub static MIGRATOR: Migrator = sqlx::migrate!();

pub static CLIENT_INFO: LazyLock<ClientInfo> = LazyLock::new(|| ClientInfo {
    client: "Jellyswarrm Proxy".to_string(),
    device: "Server".to_string(),
    device_id: "jellyswarrm-proxy".to_string(),
    version: env!("CARGO_PKG_VERSION").to_string(),
});

pub static CLIENT_STORAGE: LazyLock<jellyfin_api::storage::JellyfinClientStorage> =
    LazyLock::new(|| {
        jellyfin_api::storage::JellyfinClientStorage::new(
            300,
            std::time::Duration::from_secs(60 * 15),
        )
        // 15 minutes
    });

// Lazily-resolved data directory shared across the application.
// Priority: env var JELLYSWARRM_DATA_DIR, else "./data" relative to current working dir.
// The directory is created on first access.
pub static DATA_DIR: LazyLock<PathBuf> = LazyLock::new(|| {
    let base = std::env::var("JELLYSWARRM_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap().join("data"));
    if let Err(e) = std::fs::create_dir_all(&base) {
        eprintln!("Failed to create data directory {base:?}: {e}");
    }
    base
});

fn default_server_id() -> String {
    Uuid::new_v4().simple().to_string()
}

fn default_public_address() -> String {
    "localhost:3000".to_string()
}

fn default_server_name() -> String {
    "Jellyswarrm Proxy".to_string()
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}

fn default_port() -> u16 {
    3000
}

fn default_include_server_name_in_media() -> bool {
    true
}

fn default_username() -> String {
    "admin".to_string()
}

fn default_password() -> Password {
    "jellyswarrm".to_string().into()
}

fn default_session_key() -> Vec<u8> {
    Key::generate().master().to_vec()
}

fn default_timeout() -> u64 {
    20
}

fn default_ui_route() -> UrlSegment {
    UrlSegment("ui".to_string())
}

fn default_media_streaming_mode() -> MediaStreamingMode {
    MediaStreamingMode::Redirect
}

mod base64_serde {
    use super::*;
    use serde::de::Error as DeError;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S>(bytes: &Vec<u8>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let s = BASE64_STANDARD.encode(bytes);
        serializer.serialize_str(&s)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        BASE64_STANDARD.decode(&s).map_err(D::Error::custom)
    }
}

macro_rules! define_fallback_deserializer {
    ($name:ident, $type:ty, $fallback_fn:path) => {
        fn $name<'de, D>(deserializer: D) -> Result<$type, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            use serde::Deserialize;
            let v: Result<serde_json::Value, _> = Deserialize::deserialize(deserializer);
            match v {
                Ok(val) => {
                    // First try direct deserialization (handles numbers, booleans, etc.)
                    if let Ok(t) = serde_json::from_value::<$type>(val.clone()) {
                        return Ok(t);
                    }

                    // If that fails, try parsing from string if it is a string
                    if let serde_json::Value::String(s) = &val {
                        match s.parse::<$type>() {
                            Ok(parsed) => return Ok(parsed),
                            Err(_) => tracing::info!(
                                "Ignoring invalid value for {}: '{}', falling back to default",
                                stringify!($name),
                                s
                            ),
                        }
                    } else {
                        tracing::info!(
                            "Ignoring invalid value for {}, falling back to default",
                            stringify!($name)
                        );
                    }

                    Ok($fallback_fn())
                }
                Err(_) => {
                    tracing::info!(
                        "Ignoring invalid configuration structure for {}, falling back to default",
                        stringify!($name)
                    );
                    Ok($fallback_fn())
                }
            }
        }
    };
}

define_fallback_deserializer!(deserialize_port, u16, default_port);
define_fallback_deserializer!(deserialize_host, String, default_host);
define_fallback_deserializer!(
    deserialize_include_server_name_in_media,
    bool,
    default_include_server_name_in_media
);
define_fallback_deserializer!(deserialize_timeout, u64, default_timeout);
define_fallback_deserializer!(deserialize_ui_route, UrlSegment, default_ui_route);
define_fallback_deserializer!(
    deserialize_media_streaming_mode,
    MediaStreamingMode,
    default_media_streaming_mode
);

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PreconfiguredServer {
    pub url: String,
    pub name: String,
    pub priority: i32,
}

#[derive(Debug, Clone, Deserialize, Serialize, DefaultFromSerde)]
pub struct AppConfig {
    #[serde(default = "default_server_id")]
    pub server_id: String,
    #[serde(default = "default_public_address")]
    pub public_address: String,
    #[serde(default = "default_server_name")]
    pub server_name: String,
    #[serde(default = "default_host", deserialize_with = "deserialize_host")]
    pub host: String,
    #[serde(default = "default_port", deserialize_with = "deserialize_port")]
    pub port: u16,
    #[serde(
        default = "default_include_server_name_in_media",
        deserialize_with = "deserialize_include_server_name_in_media"
    )]
    pub include_server_name_in_media: bool,

    #[serde(default = "default_username")]
    pub username: String,
    #[serde(default = "default_password")]
    pub password: Password,

    #[serde(default)]
    pub preconfigured_servers: Vec<PreconfiguredServer>,

    #[serde(default = "default_session_key", with = "base64_serde")]
    pub session_key: Vec<u8>,

    #[serde(default = "default_timeout", deserialize_with = "deserialize_timeout")]
    pub timeout: u64, // in seconds

    #[serde(
        default = "default_ui_route",
        deserialize_with = "deserialize_ui_route"
    )]
    pub ui_route: UrlSegment,

    #[serde(default)]
    pub url_prefix: Option<UrlSegment>,

    #[serde(
        default = "default_media_streaming_mode",
        deserialize_with = "deserialize_media_streaming_mode"
    )]
    pub media_streaming_mode: MediaStreamingMode,
}

pub const DEFAULT_CONFIG_FILENAME: &str = "jellyswarrm.toml";

fn config_path() -> PathBuf {
    DATA_DIR.join(DEFAULT_CONFIG_FILENAME)
}

#[allow(dead_code)]
fn dev_config_path() -> PathBuf {
    const DEV_CONFIG_FILENAME: &str = "jellyswarrm.dev.toml";
    DATA_DIR.join(DEV_CONFIG_FILENAME)
}

/// Load configuration from known files and environment. Falls back to defaults.
pub fn load_config() -> AppConfig {
    let path = config_path();
    let builder = if cfg!(debug_assertions) {
        // In debug mode, also load a dev-specific config file if it exists.
        info!(
            "Loading config from {path:?} and dev config from {dev_config_path:?}",
            dev_config_path = dev_config_path()
        );
        config::Config::builder()
            .add_source(config::File::with_name(path.to_string_lossy().as_ref()).required(false))
            .add_source(
                config::File::with_name(dev_config_path().to_string_lossy().as_ref())
                    .required(false),
            )
            .add_source(config::Environment::with_prefix("JELLYSWARRM").separator("_"))
    } else {
        config::Config::builder()
            .add_source(config::File::with_name(path.to_string_lossy().as_ref()).required(false))
            .add_source(config::Environment::with_prefix("JELLYSWARRM").separator("_"))
    };

    let config = match builder.build() {
        Ok(c) => c.try_deserialize().unwrap_or_default(),
        Err(e) => {
            let config = AppConfig::default();
            eprintln!("Failed to load config using defaults: {e}");
            config
        }
    };

    if !path.exists() {
        if let Err(e) = save_config(&config) {
            eprintln!("Failed to save default config to {path:?}: {e}");
        }
    }

    config
}

/// Persist configuration to the first existing file or the primary default file.
pub fn save_config(cfg: &AppConfig) -> std::io::Result<()> {
    let toml_str = toml::to_string_pretty(cfg).expect("serialize config");
    fs::write(config_path(), toml_str)?;
    info!("Configuration saved to {:?}", config_path());
    Ok(())
}

// A normalized URL path segment (no leading/trailing slashes, non-empty).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UrlSegment(String);

impl UrlSegment {
    pub fn new<S: Into<String>>(s: S) -> Result<Self, &'static str> {
        let t = s
            .into()
            .trim_start_matches('/')
            .trim_end_matches('/')
            .to_string();
        if t.is_empty() {
            Err("empty UrlSegment")
        } else {
            Ok(UrlSegment(t))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Deref for UrlSegment {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<str> for UrlSegment {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for UrlSegment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for UrlSegment {
    fn from(s: String) -> Self {
        // best-effort: create without returning error (used for programmatic conversions)
        UrlSegment(s.trim_start_matches('/').trim_end_matches('/').to_string())
    }
}

impl From<&str> for UrlSegment {
    fn from(s: &str) -> Self {
        UrlSegment::from(s.to_string())
    }
}

impl std::str::FromStr for UrlSegment {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        UrlSegment::new(s)
    }
}

impl serde::Serialize for UrlSegment {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for UrlSegment {
    fn deserialize<D>(deserializer: D) -> Result<UrlSegment, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let t = s.trim_start_matches('/').trim_end_matches('/').to_string();
        if t.is_empty() {
            Err(serde::de::Error::custom("url segment must not be empty"))
        } else {
            Ok(UrlSegment(t))
        }
    }
}
