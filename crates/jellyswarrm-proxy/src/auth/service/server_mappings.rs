//! Maintain backend account mappings and migrate or decrypt stored credentials.
use super::*;

#[derive(Debug, thiserror::Error)]
pub(crate) enum MappingCredentialError {
    #[error("mapping has the wrong credential kind")]
    WrongKind,
    #[error("mapping encryption key unavailable")]
    MissingKey,
    #[error("unable to decrypt saved mapping credential")]
    Encryption(#[from] crate::domain::security::EncryptionError),
}

impl UserAuthorizationService {
    pub(super) fn mapping_credentials_changed(
        &self,
        existing_mapping: &ServerMapping,
        mapped_username: &str,
        mapped_password: &Password,
        master_password: Option<&HashedPassword>,
    ) -> bool {
        let MappingAuth::Password { username, password } = &existing_mapping.auth else {
            return true;
        };
        if !username.trim().eq_ignore_ascii_case(mapped_username.trim()) {
            return true;
        }

        let Some(master_password) = master_password else {
            return existing_mapping.credential_format != CredentialFormat::Legacy
                || !is_legacy_plaintext(password)
                || password.as_str() != mapped_password.as_str();
        };
        self.decrypt_server_mapping_password(
            existing_mapping,
            master_password,
            self.legacy_admin_key.as_ref().unwrap_or(master_password),
            None,
            None,
        )
        .map_or(true, |existing| existing != *mapped_password)
    }

    #[cfg(test)]
    pub(super) async fn resolve_server_reference(
        &self,
        server: ServerReference<'_>,
    ) -> Result<(ServerId, String), sqlx::Error> {
        match server {
            ServerReference::Server(server) => Ok((server.id, server.url.as_str().to_string())),
            ServerReference::Url(server_url) => {
                let server_url = ServerUrl::canonicalize(server_url)
                    .unwrap_or_else(|_| server_url.trim().trim_end_matches('/').to_string());
                let server_id = sqlx::query_scalar::<_, i64>(
                    r#"
                    SELECT id
                    FROM servers
                    WHERE RTRIM(url, '/') = ?
                    ORDER BY id ASC
                    LIMIT 1
                    "#,
                )
                .bind(&server_url)
                .fetch_one(&self.pool)
                .await?;

                Ok((ServerId::new(server_id), server_url))
            }
        }
    }

    /// Add or update server mapping for a user
    #[cfg(not(test))]
    pub(crate) async fn add_server_mapping(
        &self,
        user_id: &str,
        server: &Server,
        mapped_username: &str,
        mapped_password: &Password,
        master_password: Option<&HashedPassword>,
    ) -> Result<i64, sqlx::Error> {
        self.add_server_mapping_by_id(
            user_id,
            server.id,
            server.url.as_str(),
            mapped_username,
            mapped_password,
            master_password,
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn add_server_mapping<'a, S>(
        &self,
        user_id: &str,
        server: S,
        mapped_username: &str,
        mapped_password: &Password,
        master_password: Option<&HashedPassword>,
    ) -> Result<i64, sqlx::Error>
    where
        S: Into<ServerReference<'a>>,
    {
        let (server_id, server_url) = self.resolve_server_reference(server.into()).await?;
        self.add_server_mapping_by_id(
            user_id,
            server_id,
            &server_url,
            mapped_username,
            mapped_password,
            master_password,
        )
        .await
    }

    pub(super) async fn add_server_mapping_by_id(
        &self,
        user_id: &str,
        server_id: ServerId,
        server_url: &str,
        mapped_username: &str,
        mapped_password: &Password,
        master_password: Option<&HashedPassword>,
    ) -> Result<i64, sqlx::Error> {
        let now = chrono::Utc::now();

        let existing_mapping = self
            .get_server_mapping_by_server_id(user_id, server_id)
            .await?;
        let credentials_changed = existing_mapping.as_ref().is_some_and(|existing_mapping| {
            self.mapping_credentials_changed(
                existing_mapping,
                mapped_username,
                mapped_password,
                master_password,
            )
        });

        let (final_password, credential_format) = if let Some(key) = &self.mapping_key {
            (
                key.encrypt(mapped_password)
                    .map_err(|e| sqlx::Error::Protocol(e.to_string()))?,
                CredentialFormat::SessionV1,
            )
        } else if let Some(master) = master_password {
            (
                encrypt_password(mapped_password, master)
                    .map_err(|e| sqlx::Error::Protocol(e.to_string()))?,
                CredentialFormat::Legacy,
            )
        } else {
            // Only test fixtures without a configured key can write plaintext.
            #[cfg(not(test))]
            return Err(sqlx::Error::Protocol(
                "mapping encryption key is required".into(),
            ));
            #[cfg(test)]
            (
                EncryptedPassword::from_raw(mapped_password.as_str().into()),
                CredentialFormat::Legacy,
            )
        };

        let mut tx = self.pool.begin().await?;

        let mapping_id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO server_mappings
            (user_id, server_id, server_url, mapped_username, mapped_password, auth_method, backend_user_id, encrypted_token, credential_format, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, 'password', NULL, NULL, ?, ?, ?)
            ON CONFLICT(user_id, server_id) DO UPDATE SET
                server_url = excluded.server_url,
                mapped_username = excluded.mapped_username,
                mapped_password = excluded.mapped_password,
                auth_method = 'password',
                backend_user_id = NULL,
                encrypted_token = NULL,
                credential_format = excluded.credential_format,
                updated_at = excluded.updated_at
            RETURNING id
            "#,
        )
        .bind(user_id)
        .bind(server_id.as_i64())
        .bind(server_url)
        .bind(mapped_username)
        .bind(final_password)
        .bind(credential_format.as_str())
        .bind(now)
        .bind(now)
        .fetch_one(&mut *tx)
        .await?;

        if credentials_changed {
            let deleted = sqlx::query("DELETE FROM authorization_sessions WHERE mapping_id = ?")
                .bind(mapping_id)
                .execute(&mut *tx)
                .await?
                .rows_affected();

            info!(
                "Mapped credentials changed for user {} on server {}. Deleted {} affected session(s).",
                user_id, server_url, deleted
            );
        }

        tx.commit().await?;

        info!(
            "Added or updated server mapping for user {} to server {}",
            user_id, server_url
        );
        Ok(mapping_id)
    }

    /// Persist a Quick Connect credential independently of client-device sessions.
    /// Re-linking invalidates sessions that may refer to another upstream account.
    pub(crate) async fn add_quick_connect_mapping(
        &self,
        user_id: &str,
        server: &Server,
        username: &str,
        backend_user_id: &str,
        token: &str,
        encryption_key: &HashedPassword,
    ) -> Result<i64, sqlx::Error> {
        let (encrypted, credential_format) = if let Some(key) = &self.mapping_key {
            (
                key.encrypt(&Password::from(token)),
                CredentialFormat::SessionV1,
            )
        } else {
            (
                encrypt_password(&Password::from(token), encryption_key),
                CredentialFormat::Legacy,
            )
        };
        let encrypted = encrypted.map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
        let mut tx = self.pool.begin().await?;
        let id = sqlx::query_scalar::<_, i64>(
            r#"INSERT INTO server_mappings
            (user_id, server_id, server_url, mapped_username, mapped_password, auth_method, backend_user_id, encrypted_token, credential_format)
            VALUES (?, ?, ?, ?, '', 'quick_connect', ?, ?, ?)
            ON CONFLICT(user_id, server_id) DO UPDATE SET
                server_url = excluded.server_url,
                mapped_username = excluded.mapped_username,
                mapped_password = '',
                auth_method = 'quick_connect',
                backend_user_id = excluded.backend_user_id,
                encrypted_token = excluded.encrypted_token,
                credential_format = excluded.credential_format,
                updated_at = CURRENT_TIMESTAMP
            RETURNING id"#,
        )
        .bind(user_id)
        .bind(server.id.as_i64())
        .bind(server.url.as_str())
        .bind(username)
        .bind(backend_user_id)
        .bind(encrypted.as_str())
        .bind(credential_format.as_str())
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM authorization_sessions WHERE mapping_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(id)
    }

    pub(crate) fn decrypt_mapping_token(
        &self,
        mapping: &ServerMapping,
        user_key: &HashedPassword,
        admin_key: &HashedPassword,
    ) -> Result<String, MappingCredentialError> {
        let MappingAuth::QuickConnect { token, .. } = &mapping.auth else {
            return Err(MappingCredentialError::WrongKind);
        };
        if mapping.credential_format == CredentialFormat::SessionV1 {
            return self
                .mapping_key
                .as_ref()
                .ok_or(MappingCredentialError::MissingKey)?
                .decrypt(token)
                .map(Password::into_inner)
                .map_err(MappingCredentialError::Encryption);
        }
        // New Quick Connect links use the admin key. Try that first so routine
        // validation does not log an expected failed user-key decryption.
        decrypt_password(token, admin_key)
            .or_else(|_| decrypt_password(token, user_key))
            .map(Password::into_inner)
            .map_err(MappingCredentialError::Encryption)
    }

    /// Decrypt a server mapping password
    pub(crate) fn decrypt_server_mapping_password(
        &self,
        mapping: &ServerMapping,
        user_password: &HashedPassword,
        admin_password: &HashedPassword,
        user_password_plain: Option<&Password>,
        admin_password_plain: Option<&Password>,
    ) -> Result<Password, MappingCredentialError> {
        let MappingAuth::Password { password, .. } = &mapping.auth else {
            return Err(MappingCredentialError::WrongKind);
        };
        if mapping.credential_format == CredentialFormat::SessionV1 {
            return self
                .mapping_key
                .as_ref()
                .ok_or(MappingCredentialError::MissingKey)?
                .decrypt(password)
                .map_err(MappingCredentialError::Encryption);
        }
        // Try user password first
        if let Ok(decrypted) = decrypt_password(password, user_password) {
            return Ok(decrypted);
        }

        // Try admin password
        if let Ok(decrypted) = decrypt_password(password, admin_password) {
            return Ok(decrypted);
        }

        // Backward compatibility: try raw user password key material if available
        if let Some(user_password_plain) = user_password_plain {
            if let Ok(decrypted) =
                decrypt_password_with_key_material(password, user_password_plain.as_str())
            {
                return Ok(decrypted);
            }
        }

        // Backward compatibility: try raw admin password key material if available
        if let Some(admin_password_plain) = admin_password_plain {
            if let Ok(decrypted) =
                decrypt_password_with_key_material(password, admin_password_plain.as_str())
            {
                return Ok(decrypted);
            }
        }

        // Legacy plaintext can itself be valid Base64 and have AEAD-sized
        // decoded bytes. Return it for upstream validation, but never migrate
        // an ambiguous value just because legacy decryption failed.
        Ok(password.clone().into_inner().into())
    }

    pub(super) async fn upgrade_legacy_mapping(
        &self,
        mut mapping: ServerMapping,
        user_password_plain: Option<&Password>,
        admin_password_plain: Option<&Password>,
    ) -> Result<ServerMapping, sqlx::Error> {
        let Some(key) = &self.mapping_key else {
            return Ok(mapping);
        };
        if mapping.credential_format != CredentialFormat::Legacy {
            return Ok(mapping);
        }
        let user = self
            .get_user_by_id(&mapping.user_id)
            .await?
            .ok_or(sqlx::Error::RowNotFound)?;
        let user_key = user.local_credential.mapping_key();
        let admin_key = self.legacy_admin_key.as_ref().unwrap_or(&user_key);
        let plaintext = match &mapping.auth {
            MappingAuth::Password { password, .. }
                if !is_legacy_plaintext(password)
                    && decrypt_password(password, &user_key).is_err()
                    && decrypt_password(password, admin_key).is_err()
                    && !user_password_plain.is_some_and(|raw| {
                        decrypt_password_with_key_material(password, raw.as_str()).is_ok()
                    })
                    && !admin_password_plain.is_some_and(|raw| {
                        decrypt_password_with_key_material(password, raw.as_str()).is_ok()
                    }) =>
            {
                None
            }
            MappingAuth::Password { .. } => self
                .decrypt_server_mapping_password(
                    &mapping,
                    &user_key,
                    admin_key,
                    user_password_plain,
                    admin_password_plain,
                )
                .ok(),
            MappingAuth::QuickConnect { .. } => self
                .decrypt_mapping_token(&mapping, &user_key, admin_key)
                .ok()
                .map(Password::from),
        };
        let Some(plaintext) = plaintext else {
            // Raw-password-key records need the original password (available
            // when changing it); never rewrite ciphertext we cannot decrypt.
            return Ok(mapping);
        };
        let ciphertext = key
            .encrypt(&plaintext)
            .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
        let (column, old_value) = match &mapping.auth {
            MappingAuth::Password { password, .. } => ("mapped_password", password.as_str()),
            MappingAuth::QuickConnect { token, .. } => ("encrypted_token", token.as_str()),
        };
        let updated = sqlx::query(&format!(
            "UPDATE server_mappings SET {column} = ?, credential_format = 'session_v1', updated_at = CURRENT_TIMESTAMP \
             WHERE id = ? AND credential_format = 'legacy' AND {column} = ?"
        ))
        .bind(ciphertext.as_str()).bind(mapping.id).bind(old_value)
        .execute(&self.pool).await?;
        if updated.rows_affected() == 0 {
            // Another writer won; never hand out a stale credential.
            return sqlx::query_as::<_, ServerMapping>(
                "SELECT * FROM server_mappings WHERE id = ?",
            )
            .bind(mapping.id)
            .fetch_one(&self.pool)
            .await;
        }
        match &mut mapping.auth {
            MappingAuth::Password { password, .. } => *password = ciphertext,
            MappingAuth::QuickConnect { token, .. } => *token = ciphertext,
        }
        mapping.credential_format = CredentialFormat::SessionV1;
        Ok(mapping)
    }

    /// Get server mapping
    pub(crate) async fn get_server_mapping(
        &self,
        user_id: &str,
        server: &Server,
    ) -> Result<Option<ServerMapping>, sqlx::Error> {
        self.get_server_mapping_by_server_id(user_id, server.id)
            .await
    }

    pub(crate) async fn get_server_mapping_by_server_id(
        &self,
        user_id: &str,
        server_id: ServerId,
    ) -> Result<Option<ServerMapping>, sqlx::Error> {
        let mapping = sqlx::query_as::<_, ServerMapping>(
            r#"
            SELECT id, user_id, server_id, server_url, mapped_username, mapped_password, auth_method, backend_user_id, encrypted_token, credential_format, created_at, updated_at
            FROM server_mappings
            WHERE user_id = ? AND server_id = ?
            "#,
        )
        .bind(user_id)
        .bind(server_id.as_i64())
        .fetch_optional(&self.pool)
        .await?;

        match mapping {
            Some(mapping) => self
                .upgrade_legacy_mapping(mapping, None, None)
                .await
                .map(Some),
            None => Ok(None),
        }
    }

    /// List all server mappings for a user
    pub(crate) async fn list_server_mappings(
        &self,
        user_id: &str,
    ) -> Result<Vec<ServerMapping>, sqlx::Error> {
        let mappings = sqlx::query_as::<_, ServerMapping>(
            r#"
            SELECT id, user_id, server_id, server_url, mapped_username, mapped_password, auth_method, backend_user_id, encrypted_token, credential_format, created_at, updated_at
            FROM server_mappings
            WHERE user_id = ?
            ORDER BY server_url
            "#,
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;

        let mut upgraded = Vec::with_capacity(mappings.len());
        for mapping in mappings {
            upgraded.push(self.upgrade_legacy_mapping(mapping, None, None).await?);
        }
        Ok(upgraded)
    }

    /// Call only after verifying the local password. Raw-password-key legacy
    /// mappings can be migrated without contacting the backend because AEAD
    /// decryption authenticates their contents. Ambiguous values remain legacy.
    pub(crate) async fn upgrade_mappings_with_verified_password(
        &self,
        user_id: &str,
        password: &Password,
        admin_password: Option<&Password>,
    ) -> Result<Vec<ServerMapping>, sqlx::Error> {
        let mappings = self.list_server_mappings(user_id).await?;
        let mut upgraded = Vec::with_capacity(mappings.len());
        for mapping in mappings {
            upgraded.push(
                self.upgrade_legacy_mapping(mapping, Some(password), admin_password)
                    .await?,
            );
        }
        Ok(upgraded)
    }

    /// Persist a legacy password only after successful upstream authentication.
    /// Compare against the original row so a concurrent reconnect is not lost.
    pub(crate) async fn upgrade_validated_password_mapping(
        &self,
        mapping: &ServerMapping,
        plaintext: &Password,
    ) -> Result<(), sqlx::Error> {
        let MappingAuth::Password { username, password } = &mapping.auth else {
            return Err(sqlx::Error::Protocol("not a password mapping".into()));
        };
        let key = self
            .mapping_key
            .as_ref()
            .ok_or_else(|| sqlx::Error::Protocol("mapping encryption key is required".into()))?;
        let encrypted = key
            .encrypt(plaintext)
            .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
        let changed = sqlx::query("UPDATE server_mappings SET mapped_password = ?, credential_format = 'session_v1', updated_at = CURRENT_TIMESTAMP WHERE id = ? AND user_id = ? AND credential_format = 'legacy' AND auth_method = 'password' AND mapped_username = ? AND mapped_password = ?")
            .bind(encrypted.as_str()).bind(mapping.id).bind(&mapping.user_id)
            .bind(username).bind(password.as_str()).execute(&self.pool).await?;
        if changed.rows_affected() != 1 {
            return Err(sqlx::Error::Protocol(
                "mapping changed during validation; please retry".into(),
            ));
        }
        Ok(())
    }

    /// Resolve a backend account only when it identifies one virtual user on
    /// this server. Multiple devices are fine; ambiguous account aliases aren't.
    pub(crate) async fn virtual_user_id_for_backend(
        &self,
        server_id: crate::domain::server_id::ServerId,
        original_user_id: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT auth.user_id FROM authorization_sessions auth \
             JOIN server_mappings sm ON auth.mapping_id = sm.id \
             JOIN users u ON auth.user_id = u.id \
             WHERE sm.server_id = ? AND auth.original_user_id = ? \
             AND (auth.expires_at IS NULL OR auth.expires_at > ?) LIMIT 2",
        )
        .bind(server_id.as_i64())
        .bind(original_user_id)
        .bind(chrono::Utc::now())
        .fetch_all(&self.pool)
        .await?;
        Ok((ids.len() == 1).then(|| ids[0].clone()))
    }

    /// Delete a server mapping
    pub(crate) async fn delete_server_mapping(&self, mapping_id: i64) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;

        sqlx::query("DELETE FROM authorization_sessions WHERE mapping_id = ?")
            .bind(mapping_id)
            .execute(&mut *tx)
            .await?;

        let res = sqlx::query("DELETE FROM server_mappings WHERE id = ?")
            .bind(mapping_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        Ok(res.rows_affected() > 0)
    }

    /// Get all servers mapped to a user, sorted by priority
    pub(crate) async fn get_mapped_servers(
        &self,
        user_id: &str,
    ) -> Result<Vec<Server>, sqlx::Error> {
        let rows = sqlx::query(
            r#"
            SELECT s.id, s.name, s.url, s.priority, s.media_streaming_mode, s.created_at, s.updated_at
            FROM servers s
            JOIN server_mappings sm ON s.id = sm.server_id
            WHERE sm.user_id = ?
            ORDER BY s.priority DESC, s.name ASC
            "#,
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;

        let servers = rows
            .into_iter()
            .map(Server::from_row)
            .collect::<Result<Vec<_>, sqlx::Error>>()?;

        Ok(servers)
    }
}
