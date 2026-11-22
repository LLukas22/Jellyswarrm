//! Persist and revoke per-login backend credentials.
use super::*;
use crate::auth::mapping_authentication::BackendLogin;
use sha2::{Digest, Sha256};

impl UserAuthorizationService {
    /// Finish the one-time SQL migration using the configured secret.
    pub(crate) async fn enable_session_encryption(
        &mut self,
        secret: &[u8],
    ) -> Result<(), sqlx::Error> {
        let key = MappingEncryptionKey::for_authorization_sessions(secret)
            .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let staging: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='retiring_plaintext_sessions')",
        )
        .fetch_one(&mut *tx)
        .await?;
        if staging {
            let rows: Vec<(String, String)> =
                sqlx::query_as("SELECT server_url, token FROM retiring_plaintext_sessions")
                    .fetch_all(&mut *tx)
                    .await?;
            for (url, token) in rows {
                let encrypted = key
                    .encrypt(&Password::from(token))
                    .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
                sqlx::query(
                    "INSERT INTO pending_upstream_logouts(server_url, encrypted_token) VALUES(?,?)",
                )
                .bind(url)
                .bind(encrypted.as_str())
                .execute(&mut *tx)
                .await?;
            }
            sqlx::query("DROP TABLE retiring_plaintext_sessions")
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        self.session_key = Some(key);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn fixture_user(
        &self,
        name: &str,
        password: &Password,
    ) -> Result<LocalUser, sqlx::Error> {
        let mut user = self.create_user(name, password).await?;
        let id = sqlx::query_scalar("INSERT INTO client_logins(user_id,token_hash,client,device,device_id,version) VALUES(?,?,'Test','Device','fixture','1') RETURNING id")
           .bind(&user.id)
            .bind(Sha256::digest(user.fixture_token.as_bytes()).to_vec())
            .fetch_one(&self.pool)
            .await?;
        user.login_id = Some(id);
        Ok(user)
    }
    #[cfg(test)]
    pub(crate) async fn bind_fixture_login<'a, S>(
        &self,
        user_id: &str,
        server: S,
        authorization: &Authorization,
        jellyfin_token: String,
        original_user_id: String,
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<i64, sqlx::Error>
    where
        S: Into<ServerReference<'a>>,
    {
        let (server_id, server_url) = self.resolve_server_reference(server.into()).await?;
        self.bind_fixture_login_by_id(
            user_id,
            server_id,
            &server_url,
            authorization,
            jellyfin_token,
            original_user_id,
            expires_at,
        )
        .await
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn bind_fixture_login_by_id(
        &self,
        user_id: &str,
        server_id: ServerId,
        server_url: &str,
        authorization: &Authorization,
        jellyfin_token: String,
        original_user_id: String,
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<i64, sqlx::Error> {
        let mapping = self
            .get_server_mapping_by_server_id(user_id, server_id)
            .await?
            .ok_or(sqlx::Error::RowNotFound)?;
        let token = format!("fixture-login-{user_id}");
        let mut tx = self.pool.begin().await?;
        let login_id: i64 = sqlx::query_scalar(
            "INSERT INTO client_logins(user_id, token_hash, client, device, device_id, version)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(token_hash) DO UPDATE SET client=excluded.client, device=excluded.device,
                device_id=excluded.device_id, version=excluded.version
             RETURNING id",
        )
        .bind(user_id)
        .bind(Sha256::digest(token.as_bytes()).to_vec())
        .bind(&authorization.client)
        .bind(&authorization.device)
        .bind(&authorization.device_id)
        .bind(&authorization.version)
        .fetch_one(&mut *tx)
        .await?;
        let id = sqlx::query_scalar(
            "INSERT INTO login_sessions(user_id, login_id, mapping_id, server_url, upstream_device_id,
                 jellyfin_token, owned, original_user_id, expires_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(login_id,mapping_id) DO UPDATE SET jellyfin_token=excluded.jellyfin_token,
                 expires_at=excluded.expires_at, upstream_device_id=excluded.upstream_device_id
             RETURNING id",
        )
           .bind(user_id)
            .bind(login_id)
            .bind(mapping.id)
            .bind(server_url)
            .bind(&authorization.device_id)
           .bind(self.encrypt_session_token(jellyfin_token)?)
            .bind(matches!(mapping.auth, MappingAuth::Password { .. }))
           .bind(original_user_id)
            .bind(expires_at)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(id)
    }
    pub(super) fn encrypt_session_token(&self, token: String) -> Result<String, sqlx::Error> {
        self.session_key
            .as_ref()
            .ok_or_else(|| sqlx::Error::Protocol("missing session key".into()))?
            .encrypt(&Password::from(token))
            .map(|v| v.into_inner())
            .map_err(|e| sqlx::Error::Protocol(e.to_string()))
    }
    pub(crate) fn decrypt_session_token(&self, value: String) -> Result<String, sqlx::Error> {
        self.session_key
            .as_ref()
            .ok_or_else(|| sqlx::Error::Protocol("missing session key".into()))?
            .decrypt(&EncryptedPassword::from_raw(value))
            .map(|v| v.into_inner())
            .map_err(|e| sqlx::Error::Protocol(e.to_string()))
    }
    pub(crate) async fn reserve_retirement(
        &self,
        url: &str,
        token: &str,
    ) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(
            "INSERT INTO pending_upstream_logouts(server_url,encrypted_token,next_attempt)
             VALUES(?,?,unixepoch()+3600) RETURNING id",
        )
        .bind(url)
        .bind(self.encrypt_session_token(token.to_owned())?)
        .fetch_one(&self.pool)
        .await
    }
    pub(crate) async fn release_retirement(&self, id: i64) -> Result<(), sqlx::Error> {
        let released = sqlx::query(
            "UPDATE pending_upstream_logouts SET next_attempt=0 WHERE id=? AND attempts=0",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        if released.rows_affected() > 0 {
            self.session_changes.send_replace(());
        }
        Ok(())
    }
    /// Publish the bearer token and all successful backend bindings in one commit.
    pub(crate) async fn create_login(
        &self,
        user: &LocalUser,
        auth: &Authorization,
        upstream_device_id: &str,
        backends: &[BackendLogin],
    ) -> Result<(String, usize, i64), sqlx::Error> {
        if backends.is_empty() {
            return Err(sqlx::Error::RowNotFound);
        }
        let token = generate_token();
        let mut tx = self.pool.begin().await?;
        let login_id: i64 =sqlx::query_scalar("INSERT INTO client_logins(user_id,token_hash,client,device,device_id,version) VALUES(?,?,?,?,?,?) RETURNING id")
            .bind(&user.id)
            .bind(Sha256::digest(token.as_bytes()).to_vec())
            .bind(&auth.client)
            .bind(&auth.device)
            .bind(&auth.device_id)
            .bind(&auth.version)
            .fetch_one(&mut *tx)
            .await?;
        let mut first = None;
        for (index, backend) in backends.iter().enumerate() {
            if self
                .insert_backend(&mut tx, user, login_id, upstream_device_id, backend)
                .await?
            {
                first.get_or_insert(index);
            }
        }
        let first = first.ok_or(sqlx::Error::RowNotFound)?;
        let mut replaced = false;
        if Device::has_known_device_id(&normalize_device(&auth.device_id)) {
            // Android TV 0.20 renamed the app. Other clients can legitimately share a device ID.
            let android_tv = matches!(
                normalize_device(&auth.client).as_str(),
                "jellyfin android tv" | "jellyfin for android tv"
            );
            replaced = sqlx::query(
                "DELETE FROM client_logins
                 WHERE user_id=? AND device_id=? AND token_hash IS NOT NULL AND id<>?
                 AND (client=? OR (? AND lower(replace(trim(client), '+', ' ')) IN
                     ('jellyfin android tv', 'jellyfin for android tv')))",
            )
            .bind(&user.id)
            .bind(&auth.device_id)
            .bind(login_id)
            .bind(&auth.client)
            .bind(android_tv)
            .execute(&mut *tx)
            .await?
            .rows_affected()
                > 0;
        }
        tx.commit().await?;
        if replaced {
            self.session_changes.send_replace(());
        }
        Ok((token, first, login_id))
    }

    pub(crate) async fn insert_backend(
        &self,
        tx: &mut sqlx::SqliteConnection,
        user: &LocalUser,
        login_id: i64,
        device_id: &str,
        backend: &BackendLogin,
    ) -> Result<bool, sqlx::Error> {
        let Some(mapping_id) = self.validated_login_mapping(tx, user, backend).await? else {
            return Ok(false);
        };
        if let Some(guard) = &backend.login.guard {
            let claimed =
                sqlx::query("DELETE FROM pending_upstream_logouts WHERE id=? AND attempts=0")
                    .bind(guard.retirement_id)
                    .execute(&mut *tx)
                    .await?;
            if claimed.rows_affected() == 0 {
                return Ok(false);
            }
        }
        let owned = backend.login.password.is_some();
        let device_id = if owned {
            device_id.to_owned()
        } else {
            crate::auth::mapping_authentication::mapping_device_id(&backend.server, &user.id)
        };
        sqlx::query("INSERT INTO login_sessions(user_id,login_id,mapping_id,server_url,upstream_device_id,jellyfin_token,owned,original_user_id)
            VALUES (?,?,?,?,?,?,?,?)")
            .bind(&user.id)
            .bind(login_id)
            .bind(mapping_id)
            .bind(backend.server.url.as_str())
            .bind(device_id)
            .bind(self.encrypt_session_token(backend.login.response.access_token.clone())?)
            .bind(owned)
            .bind(&backend.login.response.user.id)
            .execute(tx)
            .await?;
        Ok(true)
    }
    /// Re-check the backend identity while holding the login transaction's writer lock.
    /// A relink or removal during upstream authentication invalidates that result.
    async fn validated_login_mapping(
        &self,
        tx: &mut sqlx::SqliteConnection,
        user: &LocalUser,
        backend: &BackendLogin,
    ) -> Result<Option<i64>, sqlx::Error> {
        let server = &backend.server;
        let login = &backend.login;
        let key = self
            .mapping_key
            .as_ref()
            .ok_or_else(|| sqlx::Error::Protocol("mapping encryption key is required".into()))?;
        let current_server: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM servers WHERE id=? AND url=?)")
                .bind(server.id.as_i64())
                .bind(server.url.as_str())
                .fetch_one(&mut *tx)
                .await?;
        if !current_server {
            return Ok(None);
        }
        if backend.mapping.is_none() {
            let password = login.password.as_ref().ok_or(sqlx::Error::RowNotFound)?;
            let encrypted = key
                .encrypt(password)
                .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
            sqlx::query("INSERT INTO server_mappings
                (user_id, server_id, server_url, mapped_username, mapped_password, auth_method, credential_format)
                VALUES (?, ?, ?, ?, ?, 'password', 'session_v1') ON CONFLICT(user_id, server_id) DO NOTHING")
                .bind(&user.id)
                .bind(server.id.as_i64())
                .bind(server.url.as_str())
                .bind(&login.username)
                .bind(encrypted.as_str())
                .execute(&mut *tx)
            .await?;
        }
        let current: Option<ServerMapping> =
            sqlx::query_as("SELECT * FROM server_mappings WHERE user_id = ? AND server_id = ?")
                .bind(&user.id)
                .bind(server.id.as_i64())
                .fetch_optional(&mut *tx)
                .await?;
        let Some(current) = current else {
            return Ok(None);
        };
        if backend
            .mapping
            .as_ref()
            .is_some_and(|mapping| mapping.id != current.id)
        {
            return Ok(None);
        }
        // A password-hash upgrade can re-encrypt an unchanged mapping while
        // the upstream login is in flight. Accept it if the credential still matches.
        if !backend
            .mapping
            .as_ref()
            .is_some_and(|original| original.auth == current.auth)
        {
            let user_key = user.local_credential.mapping_key();
            let matches = match (&current.auth, &login.password) {
                (MappingAuth::Password { .. }, Some(password)) => !self
                    .mapping_credentials_changed(
                        &current,
                        &login.username,
                        password,
                        Some(&user_key),
                    ),
                (
                    MappingAuth::QuickConnect {
                        backend_user_id, ..
                    },
                    None,
                ) => {
                    *backend_user_id == login.response.user.id
                        && self
                            .decrypt_mapping_token(
                                &current,
                                &user_key,
                                self.legacy_admin_key.as_ref().unwrap_or(&user_key),
                            )
                            .is_ok_and(|token| token == login.response.access_token)
                }
                _ => false,
            };
            if !matches {
                return Ok(None);
            }
        }
        if current.credential_format == CredentialFormat::Legacy {
            if let Some(password) = &login.password {
                let encrypted = key
                    .encrypt(password)
                    .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
                sqlx::query("UPDATE server_mappings SET mapped_password = ?, credential_format = 'session_v1', updated_at = CURRENT_TIMESTAMP WHERE id = ?")
                    .bind(encrypted.as_str())
            .bind(current.id)
            .execute(&mut *tx)
            .await?;
            }
        }
        Ok(Some(current.id))
    }

    pub(crate) async fn revoke_login(&self, token: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM client_logins WHERE token_hash=?")
            .bind(Sha256::digest(token.as_bytes()).to_vec())
            .execute(&self.pool)
            .await?;
        self.session_changes.send_replace(());
        Ok(())
    }
    pub(crate) async fn expire_idle_logins(&self, days: u32) -> Result<(), sqlx::Error> {
        if days > 0 {
            sqlx::query("DELETE FROM client_logins WHERE token_hash IS NOT NULL AND last_used_at<unixepoch()-?")
                .bind(i64::from(days)*86400)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    pub(crate) async fn login_exists(
        &self,
        user: &str,
        login: Option<i64>,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM client_logins WHERE user_id=? AND id=?)")
            .bind(user)
            .bind(login)
            .fetch_one(&self.pool)
            .await
    }

    /// Get authorization sessions and servers for a user by user ID
    pub(crate) async fn get_user_sessions_by_user_id(
        &self,
        user_id: &str,
    ) -> Result<Option<(LocalUser, Vec<(AuthorizationSession, Server)>)>, sqlx::Error> {
        let Some(user) = self.get_user_by_id(user_id).await? else {
            return Ok(None);
        };

        let sessions = self.get_user_sessions(&user.id).await?;
        Ok(Some((user, sessions)))
    }

    /// Get authorization sessions and servers for a user by proxy token.
    #[cfg(test)]
    pub(crate) async fn get_user_sessions_by_virtual_token(
        &self,
        virtual_token: &str,
    ) -> Result<Option<(LocalUser, Vec<(AuthorizationSession, Server)>)>, sqlx::Error> {
        let Some((user, credential)) = self.resolve_credential(virtual_token).await? else {
            return Ok(None);
        };
        let sessions = self.sessions_for_credential(&user.id, credential).await?;
        Ok(Some((user, sessions)))
    }

    /// Get authorization sessions with servers for a user
    pub(crate) async fn get_user_sessions(
        &self,
        user_id: &str,
    ) -> Result<Vec<(AuthorizationSession, Server)>, sqlx::Error> {
        self.get_user_sessions_with(user_id, None).await
    }

    pub(crate) async fn sessions_for_credential(
        &self,
        user_id: &str,
        credential: CallerCredential,
    ) -> Result<Vec<(AuthorizationSession, Server)>, sqlx::Error> {
        self.get_user_sessions_with(user_id, Some(credential)).await
    }

    async fn get_user_sessions_with(
        &self,
        user_id: &str,
        credential: Option<CallerCredential>,
    ) -> Result<Vec<(AuthorizationSession, Server)>, sqlx::Error> {
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
            r#"
    SELECT
        auth.id as auth_id,
        auth.user_id as auth_user_id,
        sm.server_url as auth_server_url,
        l.client,
        l.device,
        l.device_id,
        auth.upstream_device_id,
        l.version,
        auth.jellyfin_token,

        auth.original_user_id,

        s.id as server_id,
        s.name as server_name,
        s.url as server_url_full,
        s.priority,
        s.media_streaming_mode,
        s.created_at as server_created_at,
        s.updated_at as server_updated_at
    FROM login_sessions auth JOIN client_logins l ON l.id=auth.login_id
    JOIN server_mappings sm ON auth.mapping_id = sm.id
    JOIN servers s ON sm.server_id = s.id
    WHERE auth.user_id = "#,
        );
        query.push_bind(user_id);
        if let Some(CallerCredential::Login(id) | CallerCredential::ApplicationKey(id)) = credential
        {
            query.push(" AND auth.login_id = ").push_bind(id);
        }
        let rows = query
            .push(" AND (auth.expires_at IS NULL OR auth.expires_at > ")
            .push_bind(chrono::Utc::now())
            .push(") ORDER BY s.priority DESC, s.name ASC, auth.updated_at DESC, auth.id DESC")
            .build()
            .fetch_all(&self.pool)
            .await?;

        let sessions: Vec<(AuthorizationSession, Server)> = rows
            .into_iter()
            .map(|row| {
                let mut session = AuthorizationSession::from_user_sessions_row(&row)?;
                session.jellyfin_token = self.decrypt_session_token(session.jellyfin_token)?;
                Ok((session, Server::from_session_join_row(&row)?))
            })
            .collect::<Result<_, sqlx::Error>>()?;

        debug!("Found {} sessions for user_id: {}", sessions.len(), user_id);

        Ok(sessions)
    }

    /// Get counts of authorization sessions per server for a user.
    pub(crate) async fn session_counts_by_server(
        &self,
        user_id: &str,
    ) -> Result<Vec<(String, i64)>, sqlx::Error> {
        let rows = sqlx::query(
            r#"SELECT s.url as url_norm, COUNT(*) as cnt
                FROM login_sessions auth JOIN client_logins l ON l.id=auth.login_id
                JOIN server_mappings sm ON auth.mapping_id = sm.id
                JOIN servers s ON sm.server_id = s.id
                WHERE auth.user_id = ?
                GROUP BY sm.server_id"#,
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get::<String, _>("url_norm"), r.get::<i64, _>("cnt")))
            .collect())
    }

    /// Delete all authorization sessions for a given user.
    pub(crate) async fn delete_all_sessions_for_user(
        &self,
        user_id: &str,
    ) -> Result<u64, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let res = sqlx::query("DELETE FROM login_sessions WHERE user_id = ?")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM client_logins WHERE user_id = ? AND token_hash IS NOT NULL")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.session_changes.send_replace(());
        Ok(res.rows_affected())
    }

    /// Delete authorization sessions for a specific mapping.
    #[cfg(test)]
    pub(crate) async fn delete_sessions_for_mapping(
        &self,
        mapping_id: i64,
    ) -> Result<u64, sqlx::Error> {
        let res = sqlx::query("DELETE FROM login_sessions WHERE mapping_id = ?")
            .bind(mapping_id)
            .execute(&self.pool)
            .await?;
        self.session_changes.send_replace(());
        Ok(res.rows_affected())
    }
}
