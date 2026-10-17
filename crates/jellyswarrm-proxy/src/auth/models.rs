//! Local users, API keys, backend mappings, authorization sessions, and normalized device records.
use sqlx::{sqlite::SqliteRow, FromRow, Row};

use crate::auth::encryption::EncryptedPassword;
use crate::models::Authorization;
use crate::servers::id::ServerId;

use super::credentials::LocalCredential;
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct User {
    pub id: String,
    pub virtual_key: String,
    pub original_username: String,
    pub local_credential: LocalCredential,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl<'r> FromRow<'r, SqliteRow> for User {
    fn from_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        let password_hash = row.try_get("original_password_hash")?;
        let credential_kind: String = row.try_get("local_credential_kind")?;

        Ok(Self {
            id: row.try_get("id")?,
            virtual_key: row.try_get("virtual_key")?,
            original_username: row.try_get("original_username")?,
            local_credential: LocalCredential::from_storage(&credential_kind, password_hash)?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

#[derive(Clone, FromRow)]
pub struct VirtualUserApiKey {
    pub id: i64,
    pub user_id: String,
    pub app_name: String,
    pub access_token: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone)]
pub enum MappingAuth {
    Password {
        username: String,
        password: EncryptedPassword,
    },
    QuickConnect {
        username: String,
        backend_user_id: String,
        token: EncryptedPassword,
    },
}

impl MappingAuth {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Password { .. } => "Username and password",
            Self::QuickConnect { .. } => "Quick Connect",
        }
    }

    pub fn username(&self) -> &str {
        match self {
            Self::Password { username, .. } | Self::QuickConnect { username, .. } => username,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ServerMapping {
    pub id: i64,
    pub user_id: String,
    pub server_id: ServerId,
    pub server_url: String,
    pub auth: MappingAuth,
    pub credential_format: CredentialFormat,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialFormat {
    Legacy,
    SessionV1,
}

impl CredentialFormat {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::SessionV1 => "session_v1",
        }
    }
}

impl<'r> sqlx::FromRow<'r, SqliteRow> for ServerMapping {
    fn from_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        let credential_format = match row.try_get::<String, _>("credential_format")?.as_str() {
            "legacy" => CredentialFormat::Legacy,
            "session_v1" => CredentialFormat::SessionV1,
            other => {
                return Err(sqlx::Error::Decode(
                    format!("unknown credential format: {other}").into(),
                ))
            }
        };
        let username: String = row.try_get("mapped_username")?;
        let auth = match row.try_get::<String, _>("auth_method")?.as_str() {
            "password" => MappingAuth::Password {
                username,
                password: row.try_get("mapped_password")?,
            },
            "quick_connect" => MappingAuth::QuickConnect {
                username,
                backend_user_id: row
                    .try_get::<Option<String>, _>("backend_user_id")?
                    .ok_or_else(|| sqlx::Error::Decode("missing backend user id".into()))?,
                token: EncryptedPassword::from_raw(
                    row.try_get::<Option<String>, _>("encrypted_token")?
                        .ok_or_else(|| sqlx::Error::Decode("missing mapping token".into()))?,
                ),
            },
            kind => {
                return Err(sqlx::Error::Decode(
                    format!("unknown mapping auth: {kind}").into(),
                ))
            }
        };
        Ok(Self {
            id: row.try_get("id")?,
            user_id: row.try_get("user_id")?,
            server_id: ServerId::new(row.try_get("server_id")?),
            server_url: row.try_get("server_url")?,
            auth,
            credential_format,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct AuthorizationSession {
    pub id: i64,
    pub user_id: String,
    pub mapping_id: i64, // FK to server_mappings.id enabling cascade delete
    pub server_url: String,
    pub device: Device,
    pub jellyfin_token: String,
    pub original_user_id: String,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl<'r> sqlx::FromRow<'r, SqliteRow> for AuthorizationSession {
    fn from_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(AuthorizationSession {
            id: row.try_get("id")?,
            user_id: row.try_get("user_id")?,
            mapping_id: row.try_get("mapping_id")?,
            server_url: row.try_get("server_url")?,
            device: Device {
                client: row.try_get("client")?,
                device: row.try_get("device")?,
                device_id: row.try_get("device_id")?,
                version: row.try_get("version")?,
            },
            jellyfin_token: row.try_get("jellyfin_token")?,
            original_user_id: row.try_get("original_user_id")?,
            expires_at: row.try_get("expires_at")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Device {
    pub client: String,
    pub device: String,
    pub device_id: String,
    pub version: String,
}

pub fn normalize_device(value: &str) -> String {
    value.trim().to_lowercase().replace("+", " ")
}

pub(super) fn is_android_tv_client(client: &str) -> bool {
    normalize_device(client).contains("android tv")
}

impl Device {
    /// Check if this device matches another device based on client and either device_id or device name or version
    pub fn matches(&self, other: &Device) -> bool {
        let self_client = normalize_device(&self.client);
        let other_client = normalize_device(&other.client);

        if self_client != other_client {
            return false;
        }

        let self_device_id = normalize_device(&self.device_id);
        let other_device_id = normalize_device(&other.device_id);
        let short_self_device_id = &self_device_id[..self_device_id.len().min(16)];
        let short_other_device_id = &other_device_id[..other_device_id.len().min(16)];

        let self_has_known_device_id = Self::has_known_device_id(&self_device_id)
            || Self::has_known_device_id(short_self_device_id);
        let other_has_known_device_id = Self::has_known_device_id(&other_device_id)
            || Self::has_known_device_id(short_other_device_id);

        // 1) Strict match when both sides have a known device id.
        if self_has_known_device_id && other_has_known_device_id {
            if self_device_id == other_device_id {
                return true;
            }

            // Web clients derive device_id from base64(user-agent + "|" + timestamp) — see
            // generateDeviceId() in jellyfin-web:
            // https://github.com/jellyfin/jellyfin-web/blob/1363b749b5e01202919f8a35a2caabdaca6a18e0/src/components/apphost.js#L125-L133
            // normalize_device() lowercases it, so every browser's device_id starts with
            // the lowercased base64 of "Mozilla/5.0 (" ("tw96awxs..."), making the 16-char
            // prefix fallback below collide across ALL web sessions of a user. Require an
            // exact device_id match for those instead of falling back to the short prefix.
            const WEB_DEVICE_ID_PREFIX: &str = "tw96awxs";
            if self_device_id.starts_with(WEB_DEVICE_ID_PREFIX)
                || other_device_id.starts_with(WEB_DEVICE_ID_PREFIX)
            {
                return false;
            }

            return short_self_device_id == short_other_device_id;
        }

        // 2) Fallback to device name only when at least one side has no usable device id.
        let self_device = normalize_device(&self.device);
        let other_device = normalize_device(&other.device);
        !self_device.is_empty() && self_device == other_device
    }

    pub(crate) fn has_known_device_id(device_id: &str) -> bool {
        !device_id.is_empty()
            && device_id != "unknown-device-id"
            && device_id != "unknown"
            && device_id != "n/a"
    }

    pub fn from_useragent(user_agent: &str) -> Self {
        let (client, version, device) = Self::parse_user_agent(user_agent);

        Device {
            client,
            device,
            device_id: "unknown-device-id".to_string(),
            version,
        }
    }

    /// Parse user agent string to extract client, version, and device information
    /// Examples:
    /// - "Switchfin/0.7.4 (Linux)" -> ("Switchfin", "0.7.4", "Linux")
    /// - "Jellyfin Web/10.8.13" -> ("Jellyfin Web", "10.8.13", "Unknown")
    /// - "Mozilla/5.0 (Windows NT 10.0; Win64; x64)" -> ("Mozilla", "5.0", "Windows")
    fn parse_user_agent(user_agent: &str) -> (String, String, String) {
        let user_agent = user_agent.trim();

        // Pattern 1: "Client/Version (Device)" - e.g., "Switchfin/0.7.4 (Linux)"
        if let Some(captures) = regex::Regex::new(r"^([^/]+)/([^\s\(]+)\s*\(([^)]+)\)")
            .ok()
            .and_then(|re| re.captures(user_agent))
        {
            let device_info = captures.get(3).map_or("Unknown".to_string(), |m| {
                let device_str = m.as_str();
                // Clean up common OS patterns from device info
                if device_str.contains("Windows") {
                    "Windows".to_string()
                } else if device_str.contains("Mac") || device_str.contains("Darwin") {
                    "macOS".to_string()
                } else if device_str.contains("Linux") && !device_str.contains("Android") {
                    "Linux".to_string()
                } else if device_str.contains("Android") {
                    "Android".to_string()
                } else if device_str.contains("iPhone")
                    || device_str.contains("iPad")
                    || device_str.contains("iOS")
                {
                    "iOS".to_string()
                } else {
                    // For simple cases like "(Linux)" just return as-is
                    device_str.to_string()
                }
            });

            return (
                captures
                    .get(1)
                    .map_or("Unknown".to_string(), |m| m.as_str().to_string()),
                captures
                    .get(2)
                    .map_or("0.0.0".to_string(), |m| m.as_str().to_string()),
                device_info,
            );
        }

        // Pattern 2: "Client/Version" - e.g., "Jellyfin Web/10.8.13"
        if let Some(captures) = regex::Regex::new(r"^([^/]+)/([^\s]+)")
            .ok()
            .and_then(|re| re.captures(user_agent))
        {
            return (
                captures
                    .get(1)
                    .map_or("Unknown".to_string(), |m| m.as_str().to_string()),
                captures
                    .get(2)
                    .map_or("0.0.0".to_string(), |m| m.as_str().to_string()),
                "Unknown".to_string(),
            );
        }

        // Fallback: use the entire user agent as client
        (
            user_agent.to_string(),
            "0.0.0".to_string(),
            "Unknown".to_string(),
        )
    }
}

impl AuthorizationSession {
    /// Create an Authorization struct from this session
    pub fn to_authorization(&self) -> Authorization {
        Authorization {
            client: self.device.client.clone(),
            device: self.device.device.clone(),
            device_id: self.device.device_id.clone(),
            version: self.device.version.clone(),
            token: Some(self.jellyfin_token.clone()),
        }
    }

    pub(super) fn from_user_sessions_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("auth_id")?,
            user_id: row.try_get("auth_user_id")?,
            mapping_id: row.try_get("auth_mapping_id")?,
            server_url: row.try_get("auth_server_url")?,
            device: Device {
                client: row.try_get("client")?,
                device: row.try_get("device")?,
                device_id: row.try_get("device_id")?,
                version: row.try_get("version")?,
            },
            jellyfin_token: row.try_get("jellyfin_token")?,
            original_user_id: row.try_get("original_user_id")?,
            expires_at: row.try_get("expires_at")?,
            created_at: row.try_get("auth_created_at")?,
            updated_at: row.try_get("auth_updated_at")?,
        })
    }
}
