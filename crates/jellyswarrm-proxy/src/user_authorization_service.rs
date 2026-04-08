use sqlx::{FromRow, Row, SqlitePool};
use tracing::{debug, error, info, warn};

use crate::config::MediaStreamingMode;
use crate::encryption::{
    decrypt_password, decrypt_password_with_key_material, encrypt_password, EncryptedPassword,
    HashedPassword, Password,
};
use crate::models::{generate_token, Authorization};
use crate::server_storage::Server;

#[derive(Debug, Clone, FromRow, Eq, PartialEq, Hash)]
pub struct User {
    pub id: String,
    pub virtual_key: String,
    pub original_username: String,
    pub original_password_hash: HashedPassword,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, FromRow)]
pub struct ServerMapping {
    pub id: i64,
    pub user_id: String,
    pub server_url: String,
    pub mapped_username: String,
    pub mapped_password: EncryptedPassword,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
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

// Manual implementation of FromRow for AuthorizationSession to support nested Device
use sqlx::sqlite::SqliteRow;

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

fn is_android_tv_client(client: &str) -> bool {
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
            return self_device_id == other_device_id
                || short_self_device_id == short_other_device_id;
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
}

#[derive(Debug, Clone)]
pub struct UserAuthorizationService {
    pool: SqlitePool,
}

impl UserAuthorizationService {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    fn normalized_username_key(username: &str) -> String {
        username.trim().to_string()
    }

    /// Create or get a user by username.
    ///
    /// If the user already exists and the password changed, the stored password hash is updated.
    pub async fn get_or_create_user(
        &self,
        username: &str,
        password: &Password,
    ) -> Result<User, sqlx::Error> {
        let password_hash: HashedPassword = password.into();
        let username_key = Self::normalized_username_key(username);

        if let Some(mut user) = self.get_user_by_username(username).await? {
            if user.original_password_hash != password_hash {
                let now = chrono::Utc::now();
                sqlx::query(
                    r#"
                    UPDATE users
                    SET original_password_hash = ?, updated_at = ?
                    WHERE id = ?
                    "#,
                )
                .bind(&password_hash)
                .bind(now)
                .bind(&user.id)
                .execute(&self.pool)
                .await?;

                user.original_password_hash = password_hash;
                user.updated_at = now;
                info!(
                    "Updated password hash for existing user: {}",
                    user.original_username
                );
            }
            return Ok(user);
        }

        // Create new user
        let virtual_key = generate_token();
        let user_id = generate_token();
        let now = chrono::Utc::now();

        sqlx::query(
            r#"
            INSERT INTO users (id, virtual_key, original_username, original_password_hash, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&user_id)
        .bind(&virtual_key)
        .bind(&username_key)
        .bind(&password_hash)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        info!("Created new user for: {}", username);

        Ok(User {
            id: user_id,
            virtual_key,
            original_username: username_key,
            original_password_hash: password_hash,
            created_at: now,
            updated_at: now,
        })
    }

    /// Create a new user. Fails if a user with the same normalized username already exists.
    pub async fn create_user(
        &self,
        username: &str,
        password: &Password,
    ) -> Result<User, sqlx::Error> {
        let password_hash: HashedPassword = password.into();
        let username_key = Self::normalized_username_key(username);
        let virtual_key = generate_token();
        let user_id = generate_token();
        let now = chrono::Utc::now();

        sqlx::query(
            r#"
            INSERT INTO users (id, virtual_key, original_username, original_password_hash, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&user_id)
        .bind(&virtual_key)
        .bind(&username_key)
        .bind(&password_hash)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(User {
            id: user_id,
            virtual_key,
            original_username: username_key,
            original_password_hash: password_hash,
            created_at: now,
            updated_at: now,
        })
    }

    /// Get user by username (case-insensitive, trimmed)
    pub async fn get_user_by_username(&self, username: &str) -> Result<Option<User>, sqlx::Error> {
        let user = sqlx::query_as::<_, User>(
            r#"
            SELECT id, virtual_key, original_username, original_password_hash, created_at, updated_at
            FROM users
            WHERE lower(trim(original_username)) = lower(trim(?))
            "#,
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await?;

        Ok(user)
    }

    /// Get user by virtual key
    pub async fn get_user_by_virtual_key(
        &self,
        virtual_key: &str,
    ) -> Result<Option<User>, sqlx::Error> {
        let user = sqlx::query_as::<_, User>(
            r#"
            SELECT id, virtual_key, original_username, original_password_hash, created_at, updated_at
            FROM users
            WHERE virtual_key = ?
            "#,
        )
        .bind(virtual_key)
        .fetch_optional(&self.pool)
        .await?;

        Ok(user)
    }

    /// Get user by virtual id
    pub async fn get_user_by_id(&self, id: &str) -> Result<Option<User>, sqlx::Error> {
        let user = sqlx::query_as::<_, User>(
            r#"
            SELECT id, virtual_key, original_username, original_password_hash, created_at, updated_at
            FROM users
            WHERE id = ?
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(user)
    }

    /// Get user by credentials
    pub async fn get_user_by_credentials(
        &self,
        username: &str,
        password: &Password,
    ) -> Result<Option<User>, sqlx::Error> {
        let password_hash: HashedPassword = password.into();

        let user = sqlx::query_as::<_, User>(
            r#"
            SELECT id, virtual_key, original_username, original_password_hash, created_at, updated_at
            FROM users
            WHERE lower(trim(original_username)) = lower(trim(?)) AND original_password_hash = ?
            "#,
        )
        .bind(username)
        .bind(&password_hash)
        .fetch_optional(&self.pool)
        .await?;

        Ok(user)
    }

    /// Add or update server mapping for a user
    pub async fn add_server_mapping(
        &self,
        user_id: &str,
        server_url: &str,
        mapped_username: &str,
        mapped_password: &Password,
        master_password: Option<&HashedPassword>,
    ) -> Result<i64, sqlx::Error> {
        let now = chrono::Utc::now();

        let existing_mapping = self.get_server_mapping(user_id, server_url).await?;

        let final_password = if let Some(master) = master_password {
            match encrypt_password(mapped_password, master) {
                Ok(encrypted) => encrypted,
                Err(e) => {
                    warn!("Failed to encrypt password: {}. Storing as plaintext.", e);
                    EncryptedPassword::from_raw(mapped_password.as_str().into())
                }
            }
        } else {
            warn!("No encryption password provided. Storing as plaintext!");
            EncryptedPassword::from_raw(mapped_password.as_str().into())
        };

        sqlx::query(
            r#"
            INSERT INTO server_mappings
            (user_id, server_url, mapped_username, mapped_password, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?)
            ON CONFLICT(user_id, server_url) DO UPDATE SET
                mapped_username = excluded.mapped_username,
                mapped_password = excluded.mapped_password,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(user_id)
        .bind(server_url)
        .bind(mapped_username)
        .bind(final_password)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        let mapping_id = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT id
            FROM server_mappings
            WHERE user_id = ? AND server_url = ?
            "#,
        )
        .bind(user_id)
        .bind(server_url)
        .fetch_one(&self.pool)
        .await?;

        if let Some(existing_mapping) = existing_mapping {
            if !existing_mapping
                .mapped_username
                .trim()
                .eq_ignore_ascii_case(mapped_username.trim())
            {
                info!(
                    "Mapped username changed for user {} on server {} from '{}' to '{}'. Sessions were preserved; callers must revoke affected mapping sessions explicitly if needed.",
                    user_id,
                    server_url,
                    existing_mapping.mapped_username,
                    mapped_username
                );
            }
        }

        info!(
            "Added or updated server mapping for user {} to server {}",
            user_id, server_url
        );
        Ok(mapping_id)
    }

    /// Decrypt a server mapping password
    pub fn decrypt_server_mapping_password(
        &self,
        mapping: &ServerMapping,
        user_password: &HashedPassword,
        admin_password: &HashedPassword,
        user_password_plain: Option<&Password>,
        admin_password_plain: Option<&Password>,
    ) -> Password {
        // Try user password first
        if let Ok(decrypted) = decrypt_password(&mapping.mapped_password, user_password) {
            return decrypted;
        }

        // Try admin password
        if let Ok(decrypted) = decrypt_password(&mapping.mapped_password, admin_password) {
            return decrypted;
        }

        // Backward compatibility: try raw user password key material if available
        if let Some(user_password_plain) = user_password_plain {
            if let Ok(decrypted) = decrypt_password_with_key_material(
                &mapping.mapped_password,
                user_password_plain.as_str(),
            ) {
                return decrypted;
            }
        }

        // Backward compatibility: try raw admin password key material if available
        if let Some(admin_password_plain) = admin_password_plain {
            if let Ok(decrypted) = decrypt_password_with_key_material(
                &mapping.mapped_password,
                admin_password_plain.as_str(),
            ) {
                return decrypted;
            }
        }

        // If decryption fails, assume it's plaintext (legacy or fallback)
        warn!(
            "Failed to decrypt password for mapping {}. Assuming plaintext.",
            mapping.id
        );
        mapping.mapped_password.clone().into_inner().into()
    }

    /// Get server mapping
    pub async fn get_server_mapping(
        &self,
        user_id: &str,
        server_url: &str,
    ) -> Result<Option<ServerMapping>, sqlx::Error> {
        let mapping = sqlx::query_as::<_, ServerMapping>(
            r#"
            SELECT id, user_id, server_url, mapped_username, mapped_password, created_at, updated_at
            FROM server_mappings
            WHERE user_id = ? AND server_url = ?
            "#,
        )
        .bind(user_id)
        .bind(server_url)
        .fetch_optional(&self.pool)
        .await?;

        Ok(mapping)
    }

    /// List all server mappings for a user
    pub async fn list_server_mappings(
        &self,
        user_id: &str,
    ) -> Result<Vec<ServerMapping>, sqlx::Error> {
        let mappings = sqlx::query_as::<_, ServerMapping>(
            r#"
            SELECT id, user_id, server_url, mapped_username, mapped_password, created_at, updated_at
            FROM server_mappings
            WHERE user_id = ?
            ORDER BY server_url
            "#,
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(mappings)
    }

    /// Store authorization session
    pub async fn store_authorization_session(
        &self,
        user_id: &str,
        server_url: &str,
        authorization: &Authorization,
        jellyfin_token: String,
        original_user_id: String,
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<i64, sqlx::Error> {
        let now = chrono::Utc::now();

        // Find mapping to obtain mapping_id (required for referential integrity & cascade deletes)
        let mapping = self
            .get_server_mapping(user_id, server_url)
            .await?
            .ok_or(sqlx::Error::RowNotFound)?;

        let result = sqlx::query(
            r#"
            INSERT OR REPLACE INTO authorization_sessions
            (user_id, mapping_id, server_url, client, device, device_id, version, jellyfin_token, original_user_id, expires_at, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(user_id)
        .bind(mapping.id)
        .bind(server_url)
        .bind(&authorization.client)
        .bind(&authorization.device)
        .bind(&authorization.device_id)
        .bind(&authorization.version)
        .bind(jellyfin_token)
        .bind(original_user_id)
        .bind(expires_at)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        let session_id = result.last_insert_rowid();
        info!(
            "Stored authorization session for user {} on server {}",
            user_id, server_url
        );
        Ok(session_id)
    }

    /// Get authorization sessions and servers for a user by user ID
    pub async fn get_user_sessions_by_user_id(
        &self,
        user_id: &str,
    ) -> Result<Option<(User, Vec<(AuthorizationSession, Server)>)>, sqlx::Error> {
        // First, find the user by their ID
        let user = sqlx::query_as::<_, User>(
            r#"
            SELECT id, virtual_key, original_username, original_password_hash, created_at, updated_at
            FROM users
            WHERE id = ?
            "#,
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;

        let user = match user {
            Some(user) => user,
            None => return Ok(None),
        };

        let sessions = self.get_user_sessions(&user.id, None).await?;
        Ok(Some((user, sessions)))
    }

    /// Get authorization sessions and servers for a user by virtual token
    pub async fn get_user_sessions_by_virtual_token(
        &self,
        virtual_token: &str,
    ) -> Result<Option<(User, Vec<(AuthorizationSession, Server)>)>, sqlx::Error> {
        // First, find the user by their virtual key
        let user = match self.get_user_by_virtual_key(virtual_token).await? {
            Some(user) => user,
            None => return Ok(None),
        };

        let sessions = self.get_user_sessions(&user.id, None).await?;
        Ok(Some((user, sessions)))
    }

    ///Get authorization sessions with servers for a user
    pub async fn get_user_sessions(
        &self,
        user_id: &str,
        device: Option<Device>,
    ) -> Result<Vec<(AuthorizationSession, Server)>, sqlx::Error> {
        let query = String::from(
            r#"
    SELECT
        auth.id as auth_id,
        auth.user_id as auth_user_id,
        auth.mapping_id as auth_mapping_id,
        auth.server_url as auth_server_url,
        auth.client,
        auth.device,
        auth.device_id,
        auth.version,
        auth.jellyfin_token,
        auth.original_user_id,
        auth.expires_at,
        auth.created_at as auth_created_at,
        auth.updated_at as auth_updated_at,

        s.id as server_id,
        s.name as server_name,
        s.url as server_url_full,
        s.priority,
        s.media_streaming_mode,
        s.created_at as server_created_at,
        s.updated_at as server_updated_at
    FROM authorization_sessions auth
    JOIN servers s ON RTRIM(auth.server_url, '/') = RTRIM(s.url, '/')
    WHERE auth.user_id = ?
    AND (auth.expires_at IS NULL OR auth.expires_at > ?)
    ORDER BY s.priority DESC, s.name ASC
"#,
        );

        let rows = sqlx::query(&query)
            .bind(user_id)
            .bind(chrono::Utc::now())
            .fetch_all(&self.pool)
            .await?;

        let sessions: Vec<(AuthorizationSession, Server)> = rows
            .into_iter()
            .map(|row| {
                let device = Device {
                    client: row.get("client"),
                    device: row.get("device"),
                    device_id: row.get("device_id"),
                    version: row.get("version"),
                };
                let auth_session = AuthorizationSession {
                    id: row.get("auth_id"),
                    user_id: row.get("auth_user_id"),
                    mapping_id: row.get("auth_mapping_id"),
                    server_url: row.get("auth_server_url"),
                    device,
                    jellyfin_token: row.get("jellyfin_token"),
                    original_user_id: row.get("original_user_id"),
                    expires_at: row.get("expires_at"),
                    created_at: row.get("auth_created_at"),
                    updated_at: row.get("auth_updated_at"),
                };

                let server = Server {
                    id: row.get("server_id"),
                    name: row.get("server_name"),
                    url: url::Url::parse(row.get::<String, _>("server_url_full").as_str()).unwrap(),
                    priority: row.get("priority"),
                    media_streaming_mode: row
                        .get::<String, _>("media_streaming_mode")
                        .parse()
                        .unwrap_or(MediaStreamingMode::Redirect),
                    created_at: row.get("server_created_at"),
                    updated_at: row.get("server_updated_at"),
                };

                (auth_session, server)
            })
            .collect();

        debug!("Found {} sessions for user_id: {}", sessions.len(), user_id);

        let sessions = if let Some(device) = device {
            debug!("Filtering sessions for device: {:?}", device);
            sessions
                .into_iter()
                .filter(|(session, _)| device.matches(&session.device))
                .collect()
        } else {
            sessions
        };

        Ok(sessions)
    }

    /// Rebind Android TV authorization sessions to a new device ID when the client rotates
    /// from username-derived to user-id-derived device IDs after login.
    ///
    /// This is intentionally scoped to Android TV clients and is only meant for a one-time
    /// reconciliation path when strict device matching would otherwise miss existing sessions.
    pub async fn rebind_android_tv_device_sessions_if_needed(
        &self,
        user_id: &str,
        incoming_device: &Device,
    ) -> Result<bool, sqlx::Error> {
        if !is_android_tv_client(&incoming_device.client) {
            return Ok(false);
        }

        let incoming_device_id = normalize_device(&incoming_device.device_id);
        if !Device::has_known_device_id(&incoming_device_id) {
            return Ok(false);
        }

        let incoming_client = normalize_device(&incoming_device.client);
        let incoming_name = normalize_device(&incoming_device.device);
        if incoming_name.is_empty() {
            return Ok(false);
        }

        let sessions = self.get_user_sessions(user_id, None).await?;

        let mut stale_sessions_by_mapping: std::collections::BTreeMap<
            i64,
            Vec<AuthorizationSession>,
        > = std::collections::BTreeMap::new();
        let mut incoming_session_exists_by_mapping = std::collections::BTreeSet::new();

        for (session, _) in sessions {
            let session_client = normalize_device(&session.device.client);
            let session_name = normalize_device(&session.device.device);
            if session_client != incoming_client || session_name != incoming_name {
                continue;
            }

            let session_device_id = normalize_device(&session.device.device_id);
            if !Device::has_known_device_id(&session_device_id) {
                continue;
            }

            if session_device_id == incoming_device_id {
                incoming_session_exists_by_mapping.insert(session.mapping_id);
                continue;
            }

            stale_sessions_by_mapping
                .entry(session.mapping_id)
                .or_default()
                .push(session);
        }

        if stale_sessions_by_mapping.is_empty() {
            return Ok(false);
        }

        let collapsed_device_ids = stale_sessions_by_mapping
            .iter()
            .map(|(mapping_id, sessions)| {
                let mut ids = sessions
                    .iter()
                    .map(|session| session.device.device_id.clone())
                    .collect::<Vec<_>>();
                ids.sort();
                ids.dedup();
                format!("mapping {}: {}", mapping_id, ids.join(", "))
            })
            .collect::<Vec<_>>();

        warn!(
            "Collapsing Android TV device IDs for user {} on device '{}' (client '{}') to '{}': {}",
            user_id,
            incoming_device.device,
            incoming_device.client,
            incoming_device.device_id,
            collapsed_device_ids.join("; ")
        );

        let now = chrono::Utc::now();

        let mut tx = self.pool.begin().await?;

        let mut changed = false;

        for (mapping_id, mut stale_sessions) in stale_sessions_by_mapping {
            stale_sessions.sort_by(|left, right| {
                left.updated_at
                    .cmp(&right.updated_at)
                    .then(left.created_at.cmp(&right.created_at))
                    .then(left.id.cmp(&right.id))
            });

            if incoming_session_exists_by_mapping.contains(&mapping_id) {
                for session in stale_sessions {
                    let deleted = sqlx::query(
                        r#"
                        DELETE FROM authorization_sessions
                        WHERE id = ?
                        "#,
                    )
                    .bind(session.id)
                    .execute(&mut *tx)
                    .await?;

                    changed |= deleted.rows_affected() > 0;
                }

                continue;
            }

            let canonical_session = stale_sessions.pop().unwrap();

            let updated = sqlx::query(
                r#"
                UPDATE authorization_sessions
                SET device_id = ?, updated_at = ?
                WHERE id = ?
                "#,
            )
            .bind(&incoming_device.device_id)
            .bind(now)
            .bind(canonical_session.id)
            .execute(&mut *tx)
            .await?;

            changed |= updated.rows_affected() > 0;

            for session in stale_sessions {
                let deleted = sqlx::query(
                    r#"
                    DELETE FROM authorization_sessions
                    WHERE id = ?
                    "#,
                )
                .bind(session.id)
                .execute(&mut *tx)
                .await?;

                changed |= deleted.rows_affected() > 0;
            }
        }

        tx.commit().await?;

        Ok(changed)
    }

    /// List all users
    pub async fn list_users(&self) -> Result<Vec<User>, sqlx::Error> {
        let users = sqlx::query_as::<_, User>(
            r#"
            SELECT id, virtual_key, original_username, original_password_hash, created_at, updated_at
            FROM users
            ORDER BY original_username COLLATE NOCASE
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(users)
    }

    /// Delete a user
    pub async fn delete_user(&self, user_id: &str) -> Result<bool, sqlx::Error> {
        let res = sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Delete a server mapping
    pub async fn delete_server_mapping(&self, mapping_id: i64) -> Result<bool, sqlx::Error> {
        let res = sqlx::query("DELETE FROM server_mappings WHERE id = ?")
            .bind(mapping_id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Update user password and re-encrypt server mappings
    pub async fn update_user_password(
        &self,
        user_id: &str,
        old_password: &Password,
        new_password: &Password,
        admin_password: &Password,
    ) -> Result<bool, sqlx::Error> {
        let mut transaction = self.pool.begin().await?;

        // 1. Update user password hash
        let password_hash: HashedPassword = new_password.into();
        let now = chrono::Utc::now();

        let res = sqlx::query(
            r#"
            UPDATE users
            SET original_password_hash = ?, updated_at = ?
            WHERE id = ?
            "#,
        )
        .bind(password_hash)
        .bind(now)
        .bind(user_id)
        .execute(&mut *transaction)
        .await?;

        if res.rows_affected() == 0 {
            return Ok(false);
        }

        // 2. Re-encrypt all server mappings
        let mappings = sqlx::query_as::<_, ServerMapping>(
            r#"
            SELECT id, user_id, server_url, mapped_username, mapped_password, created_at, updated_at
            FROM server_mappings
            WHERE user_id = ?
            "#,
        )
        .bind(user_id)
        .fetch_all(&mut *transaction)
        .await?;

        let old_password_hash = old_password.into();
        let admin_password_hash = admin_password.into();

        for mapping in mappings {
            // Decrypt with old credentials
            let decrypted_password = self.decrypt_server_mapping_password(
                &mapping,
                &old_password_hash,
                &admin_password_hash,
                Some(old_password),
                Some(admin_password),
            );

            // Encrypt with new password
            let new_encrypted_password =
                match encrypt_password(&decrypted_password, &new_password.into()) {
                    Ok(p) => p,
                    Err(e) => {
                        error!("Failed to encrypt password during update: {}", e);
                        return Err(sqlx::Error::Protocol(format!("Encryption failed: {}", e)));
                    }
                };

            // Update mapping in DB
            sqlx::query(
                r#"
                UPDATE server_mappings
                SET mapped_password = ?, updated_at = ?
                WHERE id = ?
                "#,
            )
            .bind(new_encrypted_password)
            .bind(now)
            .bind(mapping.id)
            .execute(&mut *transaction)
            .await?;
        }

        transaction.commit().await?;

        Ok(true)
    }

    /// Verify user password
    pub async fn verify_user_password(
        &self,
        user_id: &str,
        password: &Password,
    ) -> Result<bool, sqlx::Error> {
        let user = self.get_user_by_id(user_id).await?;

        if let Some(user) = user {
            Ok(user.original_password_hash.verify(password.as_str()))
        } else {
            Ok(false)
        }
    }

    /// Get counts of authorization sessions per normalized server URL for a user
    pub async fn session_counts_by_server(
        &self,
        user_id: &str,
    ) -> Result<Vec<(String, i64)>, sqlx::Error> {
        let rows = sqlx::query(
            r#"SELECT RTRIM(server_url,'/') as url_norm, COUNT(*) as cnt
                FROM authorization_sessions
                WHERE user_id = ?
                GROUP BY url_norm"#,
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get::<String, _>("url_norm"), r.get::<i64, _>("cnt")))
            .collect())
    }

    /// Aggregate session counts for all users (user_id, server_url_normalized, count)
    pub async fn all_session_counts(&self) -> Result<Vec<(String, String, i64)>, sqlx::Error> {
        let rows = sqlx::query(
            r#"SELECT user_id, RTRIM(server_url,'/') as url_norm, COUNT(*) as cnt
                FROM authorization_sessions
                GROUP BY user_id, url_norm"#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get("user_id"), r.get("url_norm"), r.get("cnt")))
            .collect())
    }

    /// Delete all authorization sessions for a given user.
    pub async fn delete_all_sessions_for_user(&self, user_id: &str) -> Result<u64, sqlx::Error> {
        let res = sqlx::query("DELETE FROM authorization_sessions WHERE user_id = ?")
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }

    /// Delete authorization sessions for a specific mapping.
    pub async fn delete_sessions_for_mapping(&self, mapping_id: i64) -> Result<u64, sqlx::Error> {
        let res = sqlx::query("DELETE FROM authorization_sessions WHERE mapping_id = ?")
            .bind(mapping_id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }

    /// Get all servers mapped to a user, sorted by priority
    pub async fn get_mapped_servers(&self, user_id: &str) -> Result<Vec<Server>, sqlx::Error> {
        let rows = sqlx::query(
            r#"
            SELECT s.id, s.name, s.url, s.priority, s.media_streaming_mode, s.created_at, s.updated_at
            FROM servers s
            JOIN server_mappings sm ON RTRIM(s.url, '/') = RTRIM(sm.server_url, '/')
            WHERE sm.user_id = ?
            ORDER BY s.priority DESC, s.name ASC
            "#,
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;

        let servers = rows
            .into_iter()
            .map(|row| Server {
                id: row.get("id"),
                name: row.get("name"),
                url: url::Url::parse(row.get::<String, _>("url").as_str())
                    .unwrap_or_else(|_| url::Url::parse("http://invalid-url-in-db").unwrap()),
                priority: row.get("priority"),
                media_streaming_mode: row
                    .get::<String, _>("media_streaming_mode")
                    .parse()
                    .unwrap_or(MediaStreamingMode::Redirect),
                created_at: row.get("created_at"),
                updated_at: row.get("updated_at"),
            })
            .collect();

        Ok(servers)
    }
}

#[cfg(test)]
mod tests {
    use crate::config::MIGRATOR;

    use super::*;

    #[test]
    fn test_device_from_useragent_parsing() {
        // Test Switchfin format
        let device = Device::from_useragent("Switchfin/0.7.4 (Linux)");
        assert_eq!(device.client, "Switchfin");
        assert_eq!(device.version, "0.7.4");
        assert_eq!(device.device, "Linux");

        // Test Jellyfin Web format
        let device = Device::from_useragent("Jellyfin Web/10.8.13");
        assert_eq!(device.client, "Jellyfin Web");
        assert_eq!(device.version, "10.8.13");
        assert_eq!(device.device, "Unknown");

        // Test browser format
        let device =
            Device::from_useragent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36");
        assert_eq!(device.client, "Mozilla");
        assert_eq!(device.version, "5.0");
        assert_eq!(device.device, "Windows");

        // Test mobile format
        let device = Device::from_useragent("Jellyfin Mobile/1.0.0 (iOS)");
        assert_eq!(device.client, "Jellyfin Mobile");
        assert_eq!(device.version, "1.0.0");
        assert_eq!(device.device, "iOS");

        // Test macOS Safari
        let device = Device::from_useragent(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15",
        );
        assert_eq!(device.client, "Mozilla");
        assert_eq!(device.version, "5.0");
        assert_eq!(device.device, "macOS");

        // Test Android Chrome
        let device =
            Device::from_useragent("Mozilla/5.0 (Linux; Android 11; SM-G991B) AppleWebKit/537.36");
        assert_eq!(device.client, "Mozilla");
        assert_eq!(device.version, "5.0");
        assert_eq!(device.device, "Android");

        // Test fallback for unknown format
        let device = Device::from_useragent("SomeUnknownClient");
        assert_eq!(device.client, "SomeUnknownClient");
        assert_eq!(device.version, "0.0.0");
        assert_eq!(device.device, "Unknown");
    }

    #[tokio::test]
    async fn test_get_or_create_user_uses_stable_username_identity() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let service = UserAuthorizationService::new(pool.clone());

        let first = service
            .get_or_create_user("testuser", &"password-1".into())
            .await
            .unwrap();

        let second = service
            .get_or_create_user(" TestUser ", &"password-2".into())
            .await
            .unwrap();

        assert_eq!(
            first.id, second.id,
            "user identity should be username-based"
        );
        assert!(
            second.original_password_hash.verify("password-2"),
            "stored password hash should be updated to the latest successful login password"
        );

        let all_users = service.list_users().await.unwrap();
        assert_eq!(
            all_users.len(),
            1,
            "should not create duplicate local users"
        );
    }

    #[tokio::test]
    async fn test_device_session_fallback_matching() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let service = UserAuthorizationService::new(pool.clone());

        // Create servers table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS servers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                url TEXT NOT NULL,
                priority INTEGER NOT NULL DEFAULT 100,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        // Insert server
        sqlx::query(
            r#"
            INSERT INTO servers (name, url, priority, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind("Test Server")
        .bind("http://localhost:8096")
        .bind(100)
        .bind(chrono::Utc::now())
        .bind(chrono::Utc::now())
        .execute(&pool)
        .await
        .unwrap();

        // Create user
        let user = service
            .get_or_create_user("testuser", &"testpass".into())
            .await
            .unwrap();

        // Add server mapping
        service
            .add_server_mapping(
                &user.id,
                "http://localhost:8096",
                "mappeduser",
                &"mappedpass".into(),
                None,
            )
            .await
            .unwrap();

        // Store a session with specific device info
        let auth = Authorization {
            client: "Switchfin".to_string(),
            device: "Linux".to_string(),
            device_id: "1234567890abcdef-stored".to_string(),
            version: "0.7.4".to_string(),
            token: None,
        };

        service
            .store_authorization_session(
                &user.id,
                "http://localhost:8096",
                &auth,
                "jellyfin-token".to_string(),
                "original-jellyfin-user-id".to_string(),
                None,
            )
            .await
            .unwrap();

        // Test 1: Exact match (device_id + client)
        let query_device1 = Device {
            client: "Switchfin".to_string(),
            device: "Linux".to_string(),
            device_id: "1234567890abcdef-stored".to_string(),
            version: "0.7.4".to_string(),
        };
        let sessions1 = service
            .get_user_sessions(&user.id, Some(query_device1))
            .await
            .unwrap();
        assert_eq!(sessions1.len(), 1, "Should find exact match");

        // Test 2: Strict match when known device ids share the same short prefix
        let query_device2 = Device {
            client: "Switchfin".to_string(),
            device: "Linux".to_string(),
            device_id: "1234567890abcdef-query".to_string(),
            version: "0.7.4".to_string(),
        };
        let sessions2 = service
            .get_user_sessions(&user.id, Some(query_device2))
            .await
            .unwrap();
        assert_eq!(
            sessions2.len(),
            1,
            "Should find strict match by short device id prefix"
        );

        // Test 3: Fallback to device name + client when device id is truly unknown
        let query_device3 = Device {
            client: "Switchfin".to_string(),
            device: "Linux".to_string(),
            device_id: "unknown".to_string(),
            version: "0.7.4".to_string(),
        };
        let sessions3 = service
            .get_user_sessions(&user.id, Some(query_device3))
            .await
            .unwrap();
        assert_eq!(
            sessions3.len(),
            1,
            "Should find fallback match by device name + client"
        );

        // Test 4: Different known device ids should not fallback by device name
        let query_device4 = Device {
            client: "Switchfin".to_string(),
            device: "Linux".to_string(),
            device_id: "different-device-id".to_string(),
            version: "0.7.4".to_string(),
        };
        let sessions4 = service
            .get_user_sessions(&user.id, Some(query_device4))
            .await
            .unwrap();
        assert_eq!(
            sessions4.len(),
            0,
            "Should not fallback when both device ids are known and different"
        );

        // Test 5: No match when client and version are different
        let query_device4 = Device {
            client: "DifferentClient".to_string(),
            device: "Linux".to_string(),
            device_id: "1234567890abcdef-stored".to_string(),
            version: "1.0.0".to_string(),
        };
        let sessions5 = service
            .get_user_sessions(&user.id, Some(query_device4))
            .await
            .unwrap();
        assert_eq!(
            sessions5.len(),
            0,
            "Should not find any match when client and version differ"
        );
    }

    #[tokio::test]
    async fn test_android_tv_device_id_rebind_after_login_transition() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let service = UserAuthorizationService::new(pool.clone());

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS servers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                url TEXT NOT NULL,
                priority INTEGER NOT NULL DEFAULT 100,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            r#"
            INSERT INTO servers (name, url, priority, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind("Test Server")
        .bind("http://localhost:8096")
        .bind(100)
        .bind(chrono::Utc::now())
        .bind(chrono::Utc::now())
        .execute(&pool)
        .await
        .unwrap();

        let user = service
            .get_or_create_user("androidtv", &"testpass".into())
            .await
            .unwrap();

        service
            .add_server_mapping(
                &user.id,
                "http://localhost:8096",
                "mappeduser",
                &"mappedpass".into(),
                None,
            )
            .await
            .unwrap();

        let stored_auth = Authorization {
            client: "Jellyfin Android TV".to_string(),
            device: "Chromecast".to_string(),
            device_id: "username-derived-device-id".to_string(),
            version: "0.18.0".to_string(),
            token: None,
        };

        service
            .store_authorization_session(
                &user.id,
                "http://localhost:8096",
                &stored_auth,
                "jellyfin-token".to_string(),
                "original-jellyfin-user-id".to_string(),
                None,
            )
            .await
            .unwrap();

        let incoming_device = Device {
            client: "Jellyfin Android TV".to_string(),
            device: "Chromecast".to_string(),
            device_id: "userid-derived-device-id".to_string(),
            version: "0.18.0".to_string(),
        };

        let before = service
            .get_user_sessions(&user.id, Some(incoming_device.clone()))
            .await
            .unwrap();
        assert!(before.is_empty(), "precondition: strict lookup should miss");

        let rebound = service
            .rebind_android_tv_device_sessions_if_needed(&user.id, &incoming_device)
            .await
            .unwrap();
        assert!(rebound, "android tv device id should be rebound");

        let after = service
            .get_user_sessions(&user.id, Some(incoming_device.clone()))
            .await
            .unwrap();
        assert_eq!(after.len(), 1, "strict lookup should succeed after rebind");
        assert_eq!(after[0].0.device.device_id, incoming_device.device_id);
    }

    #[tokio::test]
    async fn test_android_tv_rebind_collapses_multiple_stale_device_ids_for_same_user() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let service = UserAuthorizationService::new(pool.clone());

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS servers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                url TEXT NOT NULL,
                priority INTEGER NOT NULL DEFAULT 100,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            r#"
            INSERT INTO servers (name, url, priority, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind("Test Server")
        .bind("http://localhost:8096")
        .bind(100)
        .bind(chrono::Utc::now())
        .bind(chrono::Utc::now())
        .execute(&pool)
        .await
        .unwrap();

        let user = service
            .get_or_create_user("androidtv-multi", &"testpass".into())
            .await
            .unwrap();

        service
            .add_server_mapping(
                &user.id,
                "http://localhost:8096",
                "mappeduser",
                &"mappedpass".into(),
                None,
            )
            .await
            .unwrap();

        for old_device_id in ["old-device-id-1", "old-device-id-2", "old-device-id-3"] {
            let stored_auth = Authorization {
                client: "Jellyfin Android TV".to_string(),
                device: "Chromecast".to_string(),
                device_id: old_device_id.to_string(),
                version: "0.18.0".to_string(),
                token: None,
            };

            service
                .store_authorization_session(
                    &user.id,
                    "http://localhost:8096",
                    &stored_auth,
                    format!("token-{old_device_id}"),
                    "original-jellyfin-user-id".to_string(),
                    None,
                )
                .await
                .unwrap();
        }

        let incoming_device = Device {
            client: "Jellyfin Android TV".to_string(),
            device: "Chromecast".to_string(),
            device_id: "incoming-device-id".to_string(),
            version: "0.18.0".to_string(),
        };

        let before = service
            .get_user_sessions(&user.id, Some(incoming_device.clone()))
            .await
            .unwrap();
        assert!(before.is_empty(), "precondition: strict lookup should miss");

        let rebound = service
            .rebind_android_tv_device_sessions_if_needed(&user.id, &incoming_device)
            .await
            .unwrap();
        assert!(rebound, "android tv stale sessions should be collapsed");

        let after = service
            .get_user_sessions(&user.id, Some(incoming_device.clone()))
            .await
            .unwrap();
        assert_eq!(
            after.len(),
            1,
            "collapse should leave one canonical session"
        );
        assert_eq!(after[0].0.device.device_id, incoming_device.device_id);

        let all_sessions = service.get_user_sessions(&user.id, None).await.unwrap();
        assert_eq!(all_sessions.len(), 1, "stale device ids should be pruned");
    }

    #[tokio::test]
    async fn test_android_tv_rebind_scope_does_not_touch_other_users() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let service = UserAuthorizationService::new(pool.clone());

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS servers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                url TEXT NOT NULL,
                priority INTEGER NOT NULL DEFAULT 100,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            r#"
            INSERT INTO servers (name, url, priority, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind("Test Server")
        .bind("http://localhost:8096")
        .bind(100)
        .bind(chrono::Utc::now())
        .bind(chrono::Utc::now())
        .execute(&pool)
        .await
        .unwrap();

        let user_a = service
            .get_or_create_user("androidtv-a", &"testpass".into())
            .await
            .unwrap();
        let user_b = service
            .get_or_create_user("androidtv-b", &"testpass".into())
            .await
            .unwrap();

        for user in [&user_a, &user_b] {
            service
                .add_server_mapping(
                    &user.id,
                    "http://localhost:8096",
                    "mappeduser",
                    &"mappedpass".into(),
                    None,
                )
                .await
                .unwrap();
        }

        for old_device_id in ["old-device-id-1", "old-device-id-2"] {
            let stored_auth = Authorization {
                client: "Jellyfin Android TV".to_string(),
                device: "Chromecast".to_string(),
                device_id: old_device_id.to_string(),
                version: "0.18.0".to_string(),
                token: None,
            };

            service
                .store_authorization_session(
                    &user_a.id,
                    "http://localhost:8096",
                    &stored_auth,
                    format!("token-a-{old_device_id}"),
                    "original-jellyfin-user-id-a".to_string(),
                    None,
                )
                .await
                .unwrap();

            service
                .store_authorization_session(
                    &user_b.id,
                    "http://localhost:8096",
                    &stored_auth,
                    format!("token-b-{old_device_id}"),
                    "original-jellyfin-user-id-b".to_string(),
                    None,
                )
                .await
                .unwrap();
        }

        let incoming_device = Device {
            client: "Jellyfin Android TV".to_string(),
            device: "Chromecast".to_string(),
            device_id: "incoming-device-id".to_string(),
            version: "0.18.0".to_string(),
        };

        let rebound = service
            .rebind_android_tv_device_sessions_if_needed(&user_a.id, &incoming_device)
            .await
            .unwrap();
        assert!(rebound);

        let user_a_sessions = service.get_user_sessions(&user_a.id, None).await.unwrap();
        assert_eq!(user_a_sessions.len(), 1);
        assert_eq!(
            user_a_sessions[0].0.device.device_id,
            incoming_device.device_id
        );

        let user_b_sessions = service.get_user_sessions(&user_b.id, None).await.unwrap();
        assert_eq!(
            user_b_sessions.len(),
            2,
            "other users must remain untouched"
        );
        let user_b_device_ids = user_b_sessions
            .iter()
            .map(|(session, _)| session.device.device_id.as_str())
            .collect::<Vec<_>>();
        assert!(user_b_device_ids.contains(&"old-device-id-1"));
        assert!(user_b_device_ids.contains(&"old-device-id-2"));
    }

    #[tokio::test]
    async fn test_android_tv_rebind_is_not_applied_to_other_clients() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let service = UserAuthorizationService::new(pool.clone());

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS servers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                url TEXT NOT NULL,
                priority INTEGER NOT NULL DEFAULT 100,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            r#"
            INSERT INTO servers (name, url, priority, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind("Test Server")
        .bind("http://localhost:8096")
        .bind(100)
        .bind(chrono::Utc::now())
        .bind(chrono::Utc::now())
        .execute(&pool)
        .await
        .unwrap();

        let user = service
            .get_or_create_user("webuser", &"testpass".into())
            .await
            .unwrap();

        service
            .add_server_mapping(
                &user.id,
                "http://localhost:8096",
                "mappeduser",
                &"mappedpass".into(),
                None,
            )
            .await
            .unwrap();

        let stored_auth = Authorization {
            client: "Jellyfin Web".to_string(),
            device: "Firefox".to_string(),
            device_id: "web-old-device-id".to_string(),
            version: "10.10.7".to_string(),
            token: None,
        };

        service
            .store_authorization_session(
                &user.id,
                "http://localhost:8096",
                &stored_auth,
                "jellyfin-token".to_string(),
                "original-jellyfin-user-id".to_string(),
                None,
            )
            .await
            .unwrap();

        let incoming_device = Device {
            client: "Jellyfin Web".to_string(),
            device: "Firefox".to_string(),
            device_id: "web-new-device-id".to_string(),
            version: "10.10.7".to_string(),
        };

        let rebound = service
            .rebind_android_tv_device_sessions_if_needed(&user.id, &incoming_device)
            .await
            .unwrap();
        assert!(!rebound, "non-android clients must not be rebound");

        let old_match = service
            .get_user_sessions(
                &user.id,
                Some(Device {
                    client: "Jellyfin Web".to_string(),
                    device: "Firefox".to_string(),
                    device_id: "web-old-device-id".to_string(),
                    version: "10.10.7".to_string(),
                }),
            )
            .await
            .unwrap();
        assert_eq!(old_match.len(), 1);

        let new_match = service
            .get_user_sessions(&user.id, Some(incoming_device))
            .await
            .unwrap();
        assert_eq!(new_match.len(), 0);
    }

    #[tokio::test]
    async fn test_user_authorization_service() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let service = UserAuthorizationService::new(pool.clone());

        // Create the servers table (normally done by ServerStorageService)
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS servers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                url TEXT NOT NULL,
                priority INTEGER NOT NULL DEFAULT 100,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        // Create a server in the servers table
        sqlx::query(
            r#"
            INSERT INTO servers (name, url, priority, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind("Test Server")
        .bind("http://localhost:8096")
        .bind(100)
        .bind(chrono::Utc::now())
        .bind(chrono::Utc::now())
        .execute(&pool)
        .await
        .unwrap();

        // Create user
        let user = service
            .get_or_create_user("testuser", &"testpass".into())
            .await
            .unwrap();

        // Add server mapping
        let _mapping_id = service
            .add_server_mapping(
                &user.id,
                "http://localhost:8096",
                "mappeduser",
                &"mappedpass".into(),
                None,
            )
            .await
            .unwrap();

        // Create authorization
        let auth = Authorization {
            client: "Test Client".to_string(),
            device: "Test Device".to_string(),
            device_id: "test-device-id".to_string(),
            version: "1.0.0".to_string(),
            token: None,
        };

        // Store authorization session
        let _session_id = service
            .store_authorization_session(
                &user.id,
                "http://localhost:8096",
                &auth,
                "jellyfin-token".to_string(),
                "original-jellyfin-user-id".to_string(),
                None,
            )
            .await
            .unwrap();

        // Retrieve user sessions by virtual token
        let user_sessions = service
            .get_user_sessions_by_virtual_token(&user.virtual_key)
            .await
            .unwrap()
            .unwrap();

        let (retrieved_user, sessions) = user_sessions;
        assert_eq!(retrieved_user.original_username, "testuser");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].0.device.client, "Test Client");
        assert_eq!(sessions[0].0.server_url, "http://localhost:8096");
        assert_eq!(sessions[0].1.name, "Test Server");
    }

    #[tokio::test]
    async fn test_get_user_sessions_by_virtual_token() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let service = UserAuthorizationService::new(pool.clone());

        // Create the servers table (normally done by ServerStorageService)
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS servers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                url TEXT NOT NULL,
                priority INTEGER NOT NULL DEFAULT 100,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        // Create a server in the servers table
        sqlx::query(
            r#"
            INSERT INTO servers (name, url, priority, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind("Test Server")
        .bind("http://localhost:8096")
        .bind(100)
        .bind(chrono::Utc::now())
        .bind(chrono::Utc::now())
        .execute(&pool)
        .await
        .unwrap();

        // Create user
        let user = service
            .get_or_create_user("testuser", &"testpass".into())
            .await
            .unwrap();

        // Add server mapping
        let _mapping_id = service
            .add_server_mapping(
                &user.id,
                "http://localhost:8096",
                "mappeduser",
                &"mappedpass".into(),
                None,
            )
            .await
            .unwrap();

        // Create authorization
        let auth = Authorization {
            client: "Test Client".to_string(),
            device: "Test Device".to_string(),
            device_id: "test-device-id".to_string(),
            version: "1.0.0".to_string(),
            token: None,
        };

        let jellyfin_token = "test-jellyfin-token".to_string();

        // Store authorization session
        let _session_id = service
            .store_authorization_session(
                &user.id,
                "http://localhost:8096",
                &auth,
                jellyfin_token.clone(),
                "original-jellyfin-user-id-2".to_string(),
                None,
            )
            .await
            .unwrap();

        // Test getting user sessions by virtual token
        let user_sessions = service
            .get_user_sessions_by_virtual_token(&user.virtual_key)
            .await
            .unwrap()
            .unwrap();

        let (retrieved_user, sessions) = user_sessions;
        assert_eq!(retrieved_user.original_username, "testuser");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].0.device.client, "Test Client");
        assert_eq!(sessions[0].1.name, "Test Server");
        assert_eq!(
            sessions[0].1.url.as_str().trim_end_matches('/'),
            "http://localhost:8096"
        );
        assert_eq!(sessions[0].1.priority, 100);
    }

    #[tokio::test]
    async fn test_multiple_servers_with_priority() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let service = UserAuthorizationService::new(pool.clone());

        // Create the servers table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS servers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                url TEXT NOT NULL,
                priority INTEGER NOT NULL DEFAULT 100,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        // Create servers
        sqlx::query(
            r#"
            INSERT INTO servers (name, url, priority, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind("Server 1")
        .bind("http://localhost:8096")
        .bind(100)
        .bind(chrono::Utc::now())
        .bind(chrono::Utc::now())
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            r#"
            INSERT INTO servers (name, url, priority, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind("Server 2")
        .bind("http://localhost:8097")
        .bind(200)
        .bind(chrono::Utc::now())
        .bind(chrono::Utc::now())
        .execute(&pool)
        .await
        .unwrap();

        // Create user
        let user = service
            .get_or_create_user("testuser", &"testpass".into())
            .await
            .unwrap();

        // Add server mappings
        service
            .add_server_mapping(
                &user.id,
                "http://localhost:8096",
                "mappeduser1",
                &"mappedpass1".into(),
                None,
            )
            .await
            .unwrap();

        service
            .add_server_mapping(
                &user.id,
                "http://localhost:8097",
                "mappeduser2",
                &"mappedpass2".into(),
                None,
            )
            .await
            .unwrap();

        // Create authorizations for both servers
        let auth1 = Authorization {
            client: "Test Client".to_string(),
            device: "Test Device".to_string(),
            device_id: "test-device-1".to_string(),
            version: "1.0.0".to_string(),
            token: None,
        };

        let auth2 = Authorization {
            client: "Test Client".to_string(),
            device: "Test Device".to_string(),
            device_id: "test-device-2".to_string(),
            version: "1.0.0".to_string(),
            token: None,
        };

        service
            .store_authorization_session(
                &user.id,
                "http://localhost:8096",
                &auth1,
                "jellyfin-token-1".to_string(),
                "original-jellyfin-user-id-1".to_string(),
                None,
            )
            .await
            .unwrap();

        service
            .store_authorization_session(
                &user.id,
                "http://localhost:8097",
                &auth2,
                "jellyfin-token-2".to_string(),
                "original-jellyfin-user-id-2".to_string(),
                None,
            )
            .await
            .unwrap();

        // Test getting all authorization sessions for the user
        let user_sessions = service
            .get_user_sessions_by_virtual_token(&user.virtual_key)
            .await
            .unwrap()
            .unwrap();

        let (retrieved_user, sessions) = user_sessions;
        assert_eq!(retrieved_user.original_username, "testuser");
        assert_eq!(sessions.len(), 2);
        // Should be sorted by priority (descending), so Server 2 should come first
        assert_eq!(sessions[0].1.name, "Server 2");
        assert_eq!(sessions[0].1.priority, 200);
        assert_eq!(sessions[1].1.name, "Server 1");
        assert_eq!(sessions[1].1.priority, 100);
    }

    #[tokio::test]
    async fn test_cascade_delete_sessions_on_mapping_delete() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let service = UserAuthorizationService::new(pool.clone());

        // Create servers table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS servers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                url TEXT NOT NULL,
                priority INTEGER NOT NULL DEFAULT 100,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        // Insert server
        sqlx::query(
            r#"
            INSERT INTO servers (name, url, priority, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind("Server 1")
        .bind("http://localhost:8096")
        .bind(100)
        .bind(chrono::Utc::now())
        .bind(chrono::Utc::now())
        .execute(&pool)
        .await
        .unwrap();

        // Create user
        let user = service
            .get_or_create_user("testuser", &"testpass".into())
            .await
            .unwrap();

        // Add mapping
        let mapping_id = service
            .add_server_mapping(
                &user.id,
                "http://localhost:8096",
                "mappeduser",
                &"mappedpass".into(),
                None,
            )
            .await
            .unwrap();

        // Store session
        let auth = Authorization {
            client: "Test Client".to_string(),
            device: "Test Device".to_string(),
            device_id: "test-device-id".to_string(),
            version: "1.0.0".to_string(),
            token: None,
        };

        service
            .store_authorization_session(
                &user.id,
                "http://localhost:8096",
                &auth,
                "jellyfin-token".to_string(),
                "original-jellyfin-user-id".to_string(),
                None,
            )
            .await
            .unwrap();

        // Pre-check session exists
        let sessions_before = service
            .get_user_sessions_by_virtual_token(&user.virtual_key)
            .await
            .unwrap()
            .unwrap()
            .1;
        assert_eq!(sessions_before.len(), 1);

        // Delete mapping (should cascade delete session)
        let deleted = service.delete_server_mapping(mapping_id).await.unwrap();
        assert!(deleted);

        // Session should now be gone
        let sessions_after = service
            .get_user_sessions_by_virtual_token(&user.virtual_key)
            .await
            .unwrap()
            .unwrap()
            .1;
        assert_eq!(
            sessions_after.len(),
            0,
            "Session should be deleted via cascade"
        );
    }

    #[tokio::test]
    async fn test_delete_all_sessions_for_user() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let service = UserAuthorizationService::new(pool.clone());

        // Create servers table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS servers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                url TEXT NOT NULL,
                priority INTEGER NOT NULL DEFAULT 100,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        // Insert two servers
        for (name, url) in [
            ("Server 1", "http://localhost:8096"),
            ("Server 2", "http://localhost:8097"),
        ] {
            sqlx::query(
                r#"INSERT INTO servers (name, url, priority, created_at, updated_at) VALUES (?, ?, ?, ?, ?)"#,
            )
            .bind(name)
            .bind(url)
            .bind(100)
            .bind(chrono::Utc::now())
            .bind(chrono::Utc::now())
            .execute(&pool)
            .await
            .unwrap();
        }

        // Create user
        let user = service
            .get_or_create_user("testuser", &"testpass".into())
            .await
            .unwrap();

        // Add mappings for both servers
        for url in ["http://localhost:8096", "http://localhost:8097"] {
            service
                .add_server_mapping(&user.id, url, "mappeduser", &"mappedpass".into(), None)
                .await
                .unwrap();
        }

        // Store two sessions
        for (i, url) in ["http://localhost:8096", "http://localhost:8097"]
            .iter()
            .enumerate()
        {
            let auth = Authorization {
                client: format!("Client {}", i + 1),
                device: "Test Device".to_string(),
                device_id: format!("device-{}", i + 1),
                version: "1.0.0".to_string(),
                token: None,
            };
            service
                .store_authorization_session(
                    &user.id,
                    url,
                    &auth,
                    format!("token-{}", i + 1),
                    format!("orig-user-{}", i + 1),
                    None,
                )
                .await
                .unwrap();
        }

        // Verify 2 sessions exist
        let sessions_before = service
            .get_user_sessions_by_virtual_token(&user.virtual_key)
            .await
            .unwrap()
            .unwrap()
            .1;
        assert_eq!(sessions_before.len(), 2);

        // Delete all sessions
        let deleted_count = service
            .delete_all_sessions_for_user(&user.id)
            .await
            .unwrap();
        assert_eq!(deleted_count, 2);

        let sessions_after = service
            .get_user_sessions_by_virtual_token(&user.virtual_key)
            .await
            .unwrap()
            .unwrap()
            .1;
        assert!(sessions_after.is_empty());
    }

    #[tokio::test]
    async fn test_add_server_mapping_upsert_preserves_mapping_id_and_sessions() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let service = UserAuthorizationService::new(pool.clone());

        // Create servers table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS servers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                url TEXT NOT NULL,
                priority INTEGER NOT NULL DEFAULT 100,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        // Insert server
        sqlx::query(
            r#"
            INSERT INTO servers (name, url, priority, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind("Server 1")
        .bind("http://localhost:8096")
        .bind(100)
        .bind(chrono::Utc::now())
        .bind(chrono::Utc::now())
        .execute(&pool)
        .await
        .unwrap();

        // Create user
        let user = service
            .get_or_create_user("testuser", &"testpass".into())
            .await
            .unwrap();

        // Initial mapping
        let mapping_id_1 = service
            .add_server_mapping(
                &user.id,
                "http://localhost:8096",
                "mappeduser",
                &"mappedpass".into(),
                None,
            )
            .await
            .unwrap();

        // Store first session
        let auth1 = Authorization {
            client: "Jellyfin Web".to_string(),
            device: "Firefox".to_string(),
            device_id: "device-1".to_string(),
            version: "10.0.0".to_string(),
            token: None,
        };

        service
            .store_authorization_session(
                &user.id,
                "http://localhost:8096",
                &auth1,
                "token-1".to_string(),
                "orig-user-1".to_string(),
                None,
            )
            .await
            .unwrap();

        // Re-add mapping for same user/server with same mapped account.
        // Should update in place and preserve sessions.
        let mapping_id_2 = service
            .add_server_mapping(
                &user.id,
                "http://localhost:8096",
                "mappeduser",
                &"mappedpass-updated".into(),
                None,
            )
            .await
            .unwrap();

        assert_eq!(
            mapping_id_1, mapping_id_2,
            "Mapping id should remain stable across UPSERT"
        );

        // Ensure first session is still present after mapping update
        let sessions_after_update = service
            .get_user_sessions_by_virtual_token(&user.virtual_key)
            .await
            .unwrap()
            .unwrap()
            .1;
        assert_eq!(
            sessions_after_update.len(),
            1,
            "Existing sessions should not be cascade-deleted on mapping update"
        );

        // Store second session for another device and ensure both exist
        let auth2 = Authorization {
            client: "Jellyfin Web".to_string(),
            device: "Firefox".to_string(),
            device_id: "device-2".to_string(),
            version: "10.0.0".to_string(),
            token: None,
        };

        service
            .store_authorization_session(
                &user.id,
                "http://localhost:8096",
                &auth2,
                "token-2".to_string(),
                "orig-user-1".to_string(),
                None,
            )
            .await
            .unwrap();

        let final_sessions = service
            .get_user_sessions_by_virtual_token(&user.virtual_key)
            .await
            .unwrap()
            .unwrap()
            .1;

        assert_eq!(final_sessions.len(), 2);
    }

    #[tokio::test]
    async fn test_add_server_mapping_username_change_preserves_sessions_until_explicit_revoke() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let service = UserAuthorizationService::new(pool.clone());

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS servers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                url TEXT NOT NULL,
                priority INTEGER NOT NULL DEFAULT 100,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            r#"
            INSERT INTO servers (name, url, priority, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind("Server 1")
        .bind("http://localhost:8096")
        .bind(100)
        .bind(chrono::Utc::now())
        .bind(chrono::Utc::now())
        .execute(&pool)
        .await
        .unwrap();

        let user = service
            .get_or_create_user("testuser", &"testpass".into())
            .await
            .unwrap();

        service
            .add_server_mapping(
                &user.id,
                "http://localhost:8096",
                "mappeduser-a",
                &"mappedpass-a".into(),
                None,
            )
            .await
            .unwrap();

        let auth = Authorization {
            client: "Jellyfin Web".to_string(),
            device: "Firefox".to_string(),
            device_id: "device-1".to_string(),
            version: "10.0.0".to_string(),
            token: None,
        };

        service
            .store_authorization_session(
                &user.id,
                "http://localhost:8096",
                &auth,
                "token-1".to_string(),
                "orig-user-1".to_string(),
                None,
            )
            .await
            .unwrap();

        let sessions_before = service
            .get_user_sessions_by_virtual_token(&user.virtual_key)
            .await
            .unwrap()
            .unwrap()
            .1;
        assert_eq!(sessions_before.len(), 1);

        // Change mapped username -> sessions stay until the caller explicitly revokes the
        // affected mapping.
        let mapping_id = service
            .add_server_mapping(
                &user.id,
                "http://localhost:8096",
                "mappeduser-b",
                &"mappedpass-b".into(),
                None,
            )
            .await
            .unwrap();

        let sessions_after = service
            .get_user_sessions_by_virtual_token(&user.virtual_key)
            .await
            .unwrap()
            .unwrap()
            .1;

        assert_eq!(sessions_after.len(), 1);

        let deleted = service
            .delete_sessions_for_mapping(mapping_id)
            .await
            .unwrap();
        assert_eq!(deleted, 1);

        let sessions_after_revoke = service
            .get_user_sessions_by_virtual_token(&user.virtual_key)
            .await
            .unwrap()
            .unwrap()
            .1;
        assert_eq!(sessions_after_revoke.len(), 0);
    }
}
