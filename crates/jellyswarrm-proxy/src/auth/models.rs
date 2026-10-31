//! Local users, API keys, backend mappings, authorization sessions, and normalized device records.
use sqlx::{sqlite::SqliteRow, FromRow, Row};

use crate::domain::ids::ServerId;
use crate::domain::security::EncryptedPassword;
use crate::domain::user_id::UserId;
use crate::protocol::authorization::Authorization;

use super::credentials::LocalCredential;
use crate::protocol::device::Device;
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub(crate) struct LocalUser {
    pub(crate) id: UserId,
    pub(crate) virtual_key: String,
    pub(crate) original_username: String,
    pub(crate) local_credential: LocalCredential,
    pub(crate) created_at: chrono::DateTime<chrono::Utc>,
    pub(crate) updated_at: chrono::DateTime<chrono::Utc>,
}

impl<'r> FromRow<'r, SqliteRow> for LocalUser {
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
pub(crate) struct VirtualUserApiKey {
    #[expect(dead_code, reason = "Retain the decoded API-key row identity")]
    pub(crate) id: i64,
    #[expect(dead_code, reason = "Retain the decoded API-key owner")]
    pub(crate) user_id: UserId,
    pub(crate) app_name: String,
    pub(crate) access_token: String,
    #[expect(
        dead_code,
        reason = "Retain timestamp decoding for the stored API-key row"
    )]
    pub(crate) created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone)]
pub(crate) enum MappingAuth {
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
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Password { .. } => "Username and password",
            Self::QuickConnect { .. } => "Quick Connect",
        }
    }

    pub(crate) fn username(&self) -> &str {
        match self {
            Self::Password { username, .. } | Self::QuickConnect { username, .. } => username,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ServerMapping {
    pub(crate) id: i64,
    pub(crate) user_id: UserId,
    pub(crate) server_id: ServerId,
    #[expect(
        dead_code,
        reason = "Retain the decoded canonical URL in mapping diagnostics"
    )]
    pub(crate) server_url: String,
    pub(crate) auth: MappingAuth,
    pub(crate) credential_format: CredentialFormat,
    #[expect(
        dead_code,
        reason = "Retain timestamp decoding and mapping diagnostics"
    )]
    pub(crate) created_at: chrono::DateTime<chrono::Utc>,
    #[expect(
        dead_code,
        reason = "Retain timestamp decoding and mapping diagnostics"
    )]
    pub(crate) updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CredentialFormat {
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
pub(crate) struct AuthorizationSession {
    pub(crate) id: i64,
    pub(crate) user_id: UserId,
    pub(crate) mapping_id: i64, // FK to server_mappings.id enabling cascade delete
    pub(crate) server_url: String,
    pub(crate) device: Device,
    pub(crate) jellyfin_token: String,
    pub(crate) original_user_id: String,
    #[expect(
        dead_code,
        reason = "Expiry is filtered in SQL; retain decoding of persisted expiry"
    )]
    pub(crate) expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub(crate) created_at: chrono::DateTime<chrono::Utc>,
    pub(crate) updated_at: chrono::DateTime<chrono::Utc>,
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

impl AuthorizationSession {
    /// Create an Authorization struct from this session
    pub(crate) fn to_authorization(&self) -> Authorization {
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
