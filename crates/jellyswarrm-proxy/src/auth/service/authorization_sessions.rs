//! Persist, encrypt, rebind, and revoke backend authorization sessions.
use super::*;
use crate::auth::mapping_authentication::BackendLogin;
use sha2::{Digest, Sha256};

impl UserAuthorizationService {
    /// Encrypt old session tokens before accepting requests. Legacy rows cannot
    /// be left in the DB indefinitely: a Quick Connect session holds the same
    /// durable backend token as its mapping.
    pub(crate) async fn enable_session_encryption(
        &mut self,
        secret: &[u8],
    ) -> Result<(), sqlx::Error> {
        let key = MappingEncryptionKey::for_authorization_sessions(secret)
            .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query(
            "SELECT id, jellyfin_token FROM authorization_sessions WHERE token_format = 'legacy'",
        )
        .fetch_all(&mut *tx)
        .await?;
        for row in rows {
            let id: i64 = row.try_get("id")?;
            let Some(token): Option<String> = row.try_get("jellyfin_token")? else {
                // Very old rows may have no upstream session at all.
                continue;
            };
            let encrypted = key
                .encrypt(&Password::from(token))
                .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
            sqlx::query("UPDATE authorization_sessions SET jellyfin_token = ?, token_format = 'session_v1' WHERE id = ?")
                .bind(encrypted.as_str()).bind(id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        self.session_key = Some(key);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn store_authorization_session<'a, S>(
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
        self.store_authorization_session_by_id(
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
    pub(super) async fn store_authorization_session_by_id(
        &self,
        user_id: &str,
        server_id: ServerId,
        server_url: &str,
        authorization: &Authorization,
        jellyfin_token: String,
        original_user_id: String,
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<i64, sqlx::Error> {
        let now = chrono::Utc::now();
        let (jellyfin_token, token_format) = self.encrypt_session_token(jellyfin_token)?;
        // Find mapping to obtain mapping_id (required for referential integrity & cascade deletes)
        let mapping = self
            .get_server_mapping_by_server_id(user_id, server_id)
            .await?
            .ok_or(sqlx::Error::RowNotFound)?;

        let session_id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO authorization_sessions
            (user_id, mapping_id, server_url, client, device, device_id, version, jellyfin_token, token_format, original_user_id, expires_at, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(user_id, mapping_id, device_id) WHERE login_id IS NULL DO UPDATE SET
                server_url = excluded.server_url,
                client = excluded.client,
                device = excluded.device,
                version = excluded.version,
                jellyfin_token = excluded.jellyfin_token,
                token_format = excluded.token_format,
                original_user_id = excluded.original_user_id,
                expires_at = excluded.expires_at,
                updated_at = excluded.updated_at
            RETURNING id
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
        .bind(token_format)
        .bind(original_user_id)
        .bind(expires_at)
        .bind(now)
        .bind(now)
        .fetch_one(&self.pool)
        .await?;

        info!(
            "Stored authorization session for user {} on server {}",
            user_id, server_url
        );
        Ok(session_id)
    }

    fn encrypt_session_token(
        &self,
        jellyfin_token: String,
    ) -> Result<(String, &'static str), sqlx::Error> {
        #[cfg(not(test))]
        if self.session_key.is_none() {
            return Err(sqlx::Error::Protocol(
                "session encryption key is required".into(),
            ));
        }
        Ok(if let Some(key) = &self.session_key {
            (
                key.encrypt(&Password::from(jellyfin_token))
                    .map_err(|e| sqlx::Error::Protocol(e.to_string()))?
                    .into_inner(),
                "session_v1",
            )
        } else {
            (jellyfin_token, "legacy")
        })
    }

    /// Publish the bearer token and all successful backend bindings in one commit.
    pub(crate) async fn create_login(
        &self,
        user: &LocalUser,
        auth: &Authorization,
        upstream_device_id: &str,
        backends: &[BackendLogin],
    ) -> Result<String, sqlx::Error> {
        if backends.is_empty() {
            return Err(sqlx::Error::RowNotFound);
        }
        let token = generate_token();
        let mut tx = self.pool.begin().await?;
        let login_id: i64 = sqlx::query_scalar(
            "INSERT INTO client_logins(user_id, token_hash) VALUES (?, ?) RETURNING id",
        )
        .bind(&user.id)
        .bind(Sha256::digest(token.as_bytes()).to_vec())
        .fetch_one(&mut *tx)
        .await?;
        for backend in backends {
            let mapping_id = self.validated_login_mapping(&mut tx, user, backend).await?;
            let response = &backend.login.response;
            let (encrypted, format) = self.encrypt_session_token(response.access_token.clone())?;
            sqlx::query(
                "INSERT INTO authorization_sessions
                 (user_id, login_id, mapping_id, server_url, client, device, device_id, upstream_device_id, version, jellyfin_token, token_format, original_user_id)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
            )
            .bind(&user.id)
            .bind(login_id)
            .bind(mapping_id)
            .bind(backend.server.url.as_str())
            .bind(&auth.client)
            .bind(&auth.device)
            .bind(&auth.device_id)
            .bind((auth.device_id != upstream_device_id).then_some(upstream_device_id))
            .bind(&auth.version)
            .bind(encrypted)
            .bind(format)
            .bind(&response.user.id)
            .execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(token)
    }

    /// Re-check the backend identity while holding the login transaction's writer lock.
    /// A relink or removal during upstream authentication invalidates that result.
    async fn validated_login_mapping(
        &self,
        tx: &mut sqlx::SqliteConnection,
        user: &LocalUser,
        backend: &BackendLogin,
    ) -> Result<i64, sqlx::Error> {
        let server = &backend.server;
        let login = &backend.login;
        let key = self
            .mapping_key
            .as_ref()
            .ok_or_else(|| sqlx::Error::Protocol("mapping encryption key is required".into()))?;
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
                .execute(&mut *tx).await?;
        }
        let current: ServerMapping =
            sqlx::query_as("SELECT * FROM server_mappings WHERE user_id = ? AND server_id = ?")
                .bind(&user.id)
                .bind(server.id.as_i64())
                .fetch_one(&mut *tx)
                .await?;
        if backend
            .mapping
            .as_ref()
            .is_some_and(|mapping| mapping.id != current.id)
        {
            return Err(sqlx::Error::Protocol(
                "mapping replaced during login".into(),
            ));
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
                return Err(sqlx::Error::Protocol("mapping changed during login".into()));
            }
        }
        if current.credential_format == CredentialFormat::Legacy {
            if let Some(password) = &login.password {
                let encrypted = key
                    .encrypt(password)
                    .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
                sqlx::query("UPDATE server_mappings SET mapped_password = ?, credential_format = 'session_v1', updated_at = CURRENT_TIMESTAMP WHERE id = ?")
                    .bind(encrypted.as_str()).bind(current.id).execute(&mut *tx).await?;
            }
        }
        Ok(current.id)
    }

    /// Revoke locally first. Return only disposable upstream tokens with no
    /// surviving bindings. Quick Connect mappings own their upstream token.
    pub(crate) async fn revoke_login(
        &self,
        token: &str,
    ) -> Result<Vec<(String, String)>, sqlx::Error> {
        let digest = Sha256::digest(token.as_bytes());
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT s.url, a.jellyfin_token FROM client_logins l
             JOIN authorization_sessions a ON a.login_id = l.id
             JOIN server_mappings m ON m.id = a.mapping_id
             JOIN servers s ON s.id = m.server_id
             WHERE l.token_hash = ? AND a.upstream_device_id IS NOT NULL AND m.auth_method = 'password'
               AND a.token_format = 'session_v1'
               AND NOT EXISTS (SELECT 1 FROM authorization_sessions other
                               WHERE other.mapping_id = a.mapping_id AND other.upstream_device_id = a.upstream_device_id
                                 AND other.id <> a.id)"
        ).bind(digest.as_slice()).fetch_all(&mut *tx).await?;
        sqlx::query("DELETE FROM client_logins WHERE token_hash = ?")
            .bind(digest.as_slice())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        let mut retired = Vec::new();
        if let Some(key) = &self.session_key {
            for (url, encrypted) in rows {
                match key.decrypt(&EncryptedPassword::from_raw(encrypted)) {
                    Ok(token) => retired.push((url, token.into_inner())),
                    Err(_) => warn!("Could not decrypt retired upstream session for {url}"),
                }
            }
        }
        Ok(retired)
    }

    /// Get authorization sessions and servers for a user by user ID
    pub(crate) async fn get_user_sessions_by_user_id(
        &self,
        user_id: &str,
    ) -> Result<Option<(LocalUser, Vec<(AuthorizationSession, Server)>)>, sqlx::Error> {
        let Some(user) = self.get_user_by_id(user_id).await? else {
            return Ok(None);
        };

        let sessions = self.get_user_sessions(&user.id, None).await?;
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
        let sessions = self
            .sessions_for_credential(&user.id, None, credential)
            .await?;
        Ok(Some((user, sessions)))
    }

    /// Get authorization sessions with servers for a user
    pub(crate) async fn get_user_sessions(
        &self,
        user_id: &str,
        device: Option<Device>,
    ) -> Result<Vec<(AuthorizationSession, Server)>, sqlx::Error> {
        self.get_user_sessions_with(&self.pool, user_id, device, None)
            .await
    }

    pub(crate) async fn sessions_for_credential(
        &self,
        user_id: &str,
        device: Option<Device>,
        credential: CallerCredential,
    ) -> Result<Vec<(AuthorizationSession, Server)>, sqlx::Error> {
        self.get_user_sessions_with(&self.pool, user_id, device, Some(credential))
            .await
    }

    async fn get_user_sessions_with<'e>(
        &self,
        executor: impl sqlx::Executor<'e, Database = sqlx::Sqlite>,
        user_id: &str,
        device: Option<Device>,
        credential: Option<CallerCredential>,
    ) -> Result<Vec<(AuthorizationSession, Server)>, sqlx::Error> {
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
            r#"
    SELECT
        auth.id as auth_id,
        auth.user_id as auth_user_id,
        auth.mapping_id as auth_mapping_id,
        sm.server_url as auth_server_url,
        auth.client,
        auth.device,
        auth.device_id,
        auth.upstream_device_id,
        auth.version,
        auth.jellyfin_token,
        auth.token_format,
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
    JOIN server_mappings sm ON auth.mapping_id = sm.id
    JOIN servers s ON sm.server_id = s.id
    WHERE auth.user_id = "#,
        );
        query.push_bind(user_id);
        match credential {
            Some(CallerCredential::Legacy) => {
                query.push(" AND auth.login_id IS NULL");
            }
            Some(CallerCredential::Login(id) | CallerCredential::ApplicationKey(Some(id))) => {
                query.push(" AND auth.login_id = ").push_bind(id);
            }
            None | Some(CallerCredential::ApplicationKey(None)) => {}
        }
        let rows = query
            .push(" AND (auth.expires_at IS NULL OR auth.expires_at > ")
            .push_bind(chrono::Utc::now())
            .push(") ORDER BY s.priority DESC, s.name ASC, auth.updated_at DESC, auth.id DESC")
            .build()
            .fetch_all(executor)
            .await?;

        let sessions: Vec<(AuthorizationSession, Server)> = rows
            .into_iter()
            .map(|row| {
                let mut session = AuthorizationSession::from_user_sessions_row(&row)?;
                if row.try_get::<String, _>("token_format")? == "session_v1" {
                    let key = self.session_key.as_ref().ok_or_else(|| {
                        sqlx::Error::Protocol("session encryption key unavailable".into())
                    })?;
                    session.jellyfin_token = key
                        .decrypt(&EncryptedPassword::from_raw(session.jellyfin_token))
                        .map_err(|e| sqlx::Error::Protocol(e.to_string()))?
                        .into_inner();
                }
                Ok((session, Server::from_session_join_row(&row)?))
            })
            .collect::<Result<_, sqlx::Error>>()?;

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

    /// Resolve abbreviated authenticated headers without weakening normal device matching.
    pub(crate) async fn get_user_sessions_for_partial_device(
        &self,
        user_id: &str,
        device: Option<&Device>,
        credential: CallerCredential,
    ) -> Result<Vec<(AuthorizationSession, Server)>, sqlx::Error> {
        let sessions = self
            .sessions_for_credential(user_id, None, credential)
            .await?;
        let Some(device) = device.filter(|d| !d.device_id.is_empty()) else {
            return Ok(sessions);
        };
        if !Device::has_known_device_id(&normalize_device(&device.device_id)) {
            return Ok(Vec::new());
        }
        Ok(sessions
            .into_iter()
            .filter(|(session, _)| {
                session.device.device_id == device.device_id
                    && (device.client.is_empty()
                        || normalize_device(&session.device.client)
                            == normalize_device(&device.client))
            })
            .collect())
    }

    /// Rebind Android TV authorization sessions to a new device ID when the client rotates
    /// from username-derived to user-id-derived device IDs after login.
    ///
    /// This is intentionally scoped to Android TV clients and is only meant for a one-time
    /// reconciliation path when strict device matching would otherwise miss existing sessions.
    /// Returns true when the caller should retry its strict lookup.
    pub(crate) async fn rebind_android_tv_device_sessions_if_needed(
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

        // Reserve the writer before reading candidates so a concurrent rebind or
        // login cannot replace the selected row between the read and the rename.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let sessions = self
            .get_user_sessions_with(&mut *tx, user_id, None, Some(CallerCredential::Legacy))
            .await?;

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
            tx.commit().await?;
            // Another request may have completed the rebind after strict lookup.
            return Ok(!incoming_session_exists_by_mapping.is_empty());
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

        let now = chrono::Utc::now();

        let mut changed = false;
        let mut replaced_conflicts = 0;

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

            let Some(canonical_session) = stale_sessions.pop() else {
                continue;
            };

            let conflicts = sqlx::query(
                r#"
                DELETE FROM authorization_sessions
                WHERE login_id IS NULL AND user_id = ? AND mapping_id = ? AND device_id = ? AND id <> ?
                "#,
            )
            .bind(user_id)
            .bind(mapping_id)
            .bind(&incoming_device.device_id)
            .bind(canonical_session.id)
            .execute(&mut *tx)
            .await?;
            replaced_conflicts += conflicts.rows_affected();

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

        warn!(
            "Collapsed Android TV device IDs for user {} on device '{}' (client '{}') to '{}': {}; replaced {} conflicting session(s)",
            user_id,
            incoming_device.device,
            incoming_device.client,
            incoming_device.device_id,
            collapsed_device_ids.join("; "),
            replaced_conflicts
        );

        Ok(changed)
    }

    /// Get counts of authorization sessions per server for a user.
    pub(crate) async fn session_counts_by_server(
        &self,
        user_id: &str,
    ) -> Result<Vec<(String, i64)>, sqlx::Error> {
        let rows = sqlx::query(
            r#"SELECT s.url as url_norm, COUNT(*) as cnt
                FROM authorization_sessions auth
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
        let res = sqlx::query("DELETE FROM authorization_sessions WHERE user_id = ?")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM client_logins WHERE user_id = ? AND token_hash IS NOT NULL")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(res.rows_affected())
    }

    /// Delete authorization sessions for a specific mapping.
    #[cfg(test)]
    pub(crate) async fn delete_sessions_for_mapping(
        &self,
        mapping_id: i64,
    ) -> Result<u64, sqlx::Error> {
        let res = sqlx::query("DELETE FROM authorization_sessions WHERE mapping_id = ?")
            .bind(mapping_id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }
}
