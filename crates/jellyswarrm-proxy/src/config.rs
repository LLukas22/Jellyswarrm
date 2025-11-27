use serde::{Deserialize, Serialize};
use serde_default::DefaultFromSerde;
use sqlx::migrate::Migrator;
use std::fmt;
use std::fs;
use std::ops::Deref;
use std::path::PathBuf;
use tower_sessions::cookie::Key;
use tracing::info;
use uuid::Uuid;

use once_cell::sync::Lazy;

use base64::prelude::*;

pub static MIGRATOR: Migrator = sqlx::migrate!();

// Lazily-resolved data directory shared across the application.
// Priority: env var JELLYSWARRM_DATA_DIR, else "./data" relative to current working dir.
// The directory is created on first access.
pub static DATA_DIR: Lazy<PathBuf> = Lazy::new(|| {
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

fn default_password() -> String {
    "jellyswarrm".to_string()
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

mod base64_serde {
    use super::*;
    use serde::de::Error;
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

macro_rules! create_safe_deserializer {
    ($name:ident, $type:ty, $default_fn:path, $inner_deserializer:path) => {
        fn $name<'de, D>(deserializer: D) -> Result<$type, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            match $inner_deserializer(deserializer) {
                Ok(v) => Ok(v),
                Err(_) => {
                    tracing::warn!(
                        "Failed to deserialize field {}, falling back to default",
                        stringify!($name)
                    );
                    Ok($default_fn())
                }
            }
        }
    };
    ($name:ident, $type:ty, $default_fn:path, $inner_deserializer:path, $validator:path) => {
        fn $name<'de, D>(deserializer: D) -> Result<$type, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            match $inner_deserializer(deserializer) {
                Ok(v) => match $validator(v) {
                    Ok(v) => Ok(v),
                    Err(e) => {
                        tracing::warn!(
                            "Validation failed for field {}: {}, falling back to default",
                            stringify!($name),
                            e
                        );
                        Ok($default_fn())
                    }
                },
                Err(_) => {
                    tracing::warn!(
                        "Failed to deserialize field {}, falling back to default",
                        stringify!($name)
                    );
                    Ok($default_fn())
                }
            }
        }
    };
}

fn validate_not_empty(s: String) -> Result<String, String> {
    if s.trim().is_empty() {
        Err("Value cannot be empty".to_string())
    } else {
        Ok(s)
    }
}

create_safe_deserializer!(
    deserialize_port,
    u16,
    default_port,
    serde_aux::prelude::deserialize_number_from_string
);

create_safe_deserializer!(
    deserialize_timeout,
    u64,
    default_timeout,
    serde_aux::prelude::deserialize_number_from_string
);

create_safe_deserializer!(
    deserialize_include_server_name_in_media,
    bool,
    default_include_server_name_in_media,
    serde_aux::prelude::deserialize_bool_from_anything
);

create_safe_deserializer!(
    deserialize_server_name,
    String,
    default_server_name,
    String::deserialize,
    validate_not_empty
);

create_safe_deserializer!(
    deserialize_host,
    String,
    default_host,
    String::deserialize,
    validate_not_empty
);

create_safe_deserializer!(
    deserialize_ui_route,
    UrlSegment,
    default_ui_route,
    UrlSegment::deserialize
);

create_safe_deserializer!(
    deserialize_session_key,
    Vec<u8>,
    default_session_key,
    base64_serde::deserialize
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
    #[serde(
        default = "default_server_name",
        deserialize_with = "deserialize_server_name"
    )]
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
    pub password: String,

    #[serde(default)]
    pub preconfigured_servers: Vec<PreconfiguredServer>,

    #[serde(
        default = "default_session_key",
        deserialize_with = "deserialize_session_key",
        serialize_with = "base64_serde::serialize"
    )]
    pub session_key: Vec<u8>,

    #[serde(default = "default_timeout", deserialize_with = "deserialize_timeout")]
    pub timeout: u64, // in seconds

    #[serde(default = "default_ui_route", deserialize_with = "deserialize_ui_route")]
    pub ui_route: UrlSegment,

    #[serde(default)]
    pub url_prefix: Option<UrlSegment>,
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
    fs::write(config_path(), toml_str)
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct TestConfig {
        #[serde(default = "default_port", deserialize_with = "deserialize_port")]
        port: u16,
        #[serde(default = "default_host", deserialize_with = "deserialize_host")]
        host: String,
        #[serde(
            default = "default_include_server_name_in_media",
            deserialize_with = "deserialize_include_server_name_in_media"
        )]
        include_server_name_in_media: bool,
        #[serde(
            default = "default_server_name",
            deserialize_with = "deserialize_server_name"
        )]
        server_name: String,
        #[serde(
            default = "default_session_key",
            deserialize_with = "deserialize_session_key"
        )]
        session_key: Vec<u8>,
        #[serde(default = "default_ui_route", deserialize_with = "deserialize_ui_route")]
        ui_route: UrlSegment,
    }

    #[test]
    fn test_deserialize_port_valid() {
        let json = r#"{"port": 8080}"#;
        let config: TestConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.port, 8080);

        let json = r#"{"port": "8080"}"#;
        let config: TestConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.port, 8080);
    }

    #[test]
    fn test_deserialize_port_invalid() {
        let json = r#"{"port": "invalid"}"#;
        let config: TestConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.port, default_port());

        let json = r#"{"port": 999999}"#; // Overflow
        let config: TestConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.port, default_port());
    }

    #[test]
    fn test_deserialize_host_valid() {
        let json = r#"{"host": "127.0.0.1"}"#;
        let config: TestConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.host, "127.0.0.1");
    }

    #[test]
    fn test_deserialize_host_invalid() {
        let json = r#"{"host": ""}"#; // Empty string
        let config: TestConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.host, default_host());
    }

    #[test]
    fn test_deserialize_bool_valid() {
        let json = r#"{"include_server_name_in_media": true}"#;
        let config: TestConfig = serde_json::from_str(json).unwrap();
        assert!(config.include_server_name_in_media);

        let json = r#"{"include_server_name_in_media": "true"}"#;
        let config: TestConfig = serde_json::from_str(json).unwrap();
        assert!(config.include_server_name_in_media);

        let json = r#"{"include_server_name_in_media": "on"}"#;
        let config: TestConfig = serde_json::from_str(json).unwrap();
        assert!(config.include_server_name_in_media);
    }

    #[test]
    fn test_deserialize_bool_invalid() {
        let json = r#"{"include_server_name_in_media": "invalid"}"#;
        let config: TestConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            config.include_server_name_in_media,
            default_include_server_name_in_media()
        );
    }

    #[test]
    fn test_deserialize_server_name_invalid() {
        let json = r#"{"server_name": ""}"#;
        let config: TestConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.server_name, default_server_name());
    }

    #[test]
    fn test_deserialize_session_key_valid() {
        let key = Key::generate().master().to_vec();
        let encoded = BASE64_STANDARD.encode(&key);
        let json = format!(r#"{{"session_key": "{}"}}"#, encoded);
        let config: TestConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config.session_key, key);
    }

    #[test]
    fn test_deserialize_session_key_invalid() {
        let json = r#"{"session_key": "invalid-base64"}"#;
        let config: TestConfig = serde_json::from_str(json).unwrap();
        // Should fall back to default (random key), so we just check it's not empty
        assert!(!config.session_key.is_empty());
    }

    #[test]
    fn test_deserialize_ui_route_valid() {
        let json = r#"{"ui_route": "admin"}"#;
        let config: TestConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.ui_route.as_str(), "admin");
    }

    #[test]
    fn test_deserialize_ui_route_invalid() {
        let json = r#"{"ui_route": ""}"#; // Empty route is invalid for UrlSegment
        let config: TestConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.ui_route.as_str(), default_ui_route().as_str());
    }
}
