//! Persist, encrypt, rebind, and revoke backend authorization sessions.
use super::*;

impl UserAuthorizationService {
    /// Encrypt old session tokens before accepting requests. Legacy rows cannot
    /// be left in the DB indefinitely: a Quick Connect session holds the same
    /// durable backend token as its mapping.
    pub async fn enable_session_encryption(&mut self, secret: &[u8]) -> Result<(), sqlx::Error> {
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

    /// Store authorization session
    #[cfg(not(test))]
    pub async fn store_authorization_session(
        &self,
        user_id: &str,
        server: &Server,
        authorization: &Authorization,
        jellyfin_token: String,
        original_user_id: String,
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<i64, sqlx::Error> {
        self.store_authorization_session_by_id(
            user_id,
            server.id,
            server.url.as_str(),
            authorization,
            jellyfin_token,
            original_user_id,
            expires_at,
        )
        .await
    }

    #[cfg(test)]
    pub async fn store_authorization_session<'a, S>(
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
        #[cfg(not(test))]
        if self.session_key.is_none() {
            return Err(sqlx::Error::Protocol(
                "session encryption key is required".into(),
            ));
        }
        let (jellyfin_token, token_format) = if let Some(key) = &self.session_key {
            (
                key.encrypt(&Password::from(jellyfin_token))
                    .map_err(|e| sqlx::Error::Protocol(e.to_string()))?
                    .into_inner(),
                "session_v1",
            )
        } else {
            (jellyfin_token, "legacy")
        };

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
            ON CONFLICT(user_id, mapping_id, device_id) DO UPDATE SET
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

    /// Get authorization sessions and servers for a user by user ID
    pub async fn get_user_sessions_by_user_id(
        &self,
        user_id: &str,
    ) -> Result<Option<(User, Vec<(AuthorizationSession, Server)>)>, sqlx::Error> {
        // First, find the user by their ID
        let user = sqlx::query_as::<_, User>(
            r#"
            SELECT id, virtual_key, original_username, original_password_hash,
                   local_credential_kind, created_at, updated_at
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

    /// Get authorization sessions and servers for a user by proxy token.
    pub async fn get_user_sessions_by_virtual_token(
        &self,
        virtual_token: &str,
    ) -> Result<Option<(User, Vec<(AuthorizationSession, Server)>)>, sqlx::Error> {
        let user = match self.get_user_by_token(virtual_token).await? {
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
        sm.server_url as auth_server_url,
        auth.client,
        auth.device,
        auth.device_id,
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

            let Some(canonical_session) = stale_sessions.pop() else {
                continue;
            };

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

    /// Get counts of authorization sessions per server for a user.
    pub async fn session_counts_by_server(
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

    /// Aggregate session counts for all users (user_id, canonical server URL, count).
    pub async fn all_session_counts(&self) -> Result<Vec<(String, String, i64)>, sqlx::Error> {
        let rows = sqlx::query(
            r#"SELECT auth.user_id, s.url as url_norm, COUNT(*) as cnt
                FROM authorization_sessions auth
                JOIN server_mappings sm ON auth.mapping_id = sm.id
                JOIN servers s ON sm.server_id = s.id
                GROUP BY auth.user_id, sm.server_id"#,
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
}
