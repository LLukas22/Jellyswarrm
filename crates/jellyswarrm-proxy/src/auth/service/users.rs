//! Local user creation, login lookup, password changes, and credential upgrades.
use super::*;
use crate::auth::user_id::UserId;

impl UserAuthorizationService {
    pub(super) fn normalized_username_key(username: &str) -> String {
        username.trim().to_string()
    }

    /// Create or get a user by username without changing an existing credential.
    pub async fn get_or_create_user(
        &self,
        username: &str,
        password: &Password,
    ) -> Result<User, sqlx::Error> {
        let username_key = Self::normalized_username_key(username);

        if let Some(user) = self.get_user_by_username(username).await? {
            return Ok(user);
        }

        // Create new user
        let local_credential = LocalCredential::from_password(password).await?;
        let virtual_key = generate_token();
        let user_id = UserId::from(generate_token());
        let now = chrono::Utc::now();

        sqlx::query(
            r#"
            INSERT INTO users
                (id, virtual_key, original_username, original_password_hash, local_credential_kind, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&user_id)
        .bind(&virtual_key)
        .bind(&username_key)
        .bind(local_credential.stored_hash())
        .bind(local_credential.kind())
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        info!("Created new user for: {}", username);

        Ok(User {
            id: user_id,
            virtual_key,
            original_username: username_key,
            local_credential,
            created_at: now,
            updated_at: now,
        })
    }

    /// Resolve an existing login identity or create it after upstream authentication.
    /// A concurrent creator is accepted only when it used the same local credential.
    pub async fn resolve_or_create_login_user(
        &self,
        username: &str,
        password: &Password,
    ) -> Result<Option<User>, sqlx::Error> {
        if let Some(user) = self.get_user_by_username(username).await? {
            return if user.local_credential.verify_async(password).await? {
                self.upgrade_local_password_hash(user, password)
                    .await
                    .map(Some)
            } else {
                Ok(None)
            };
        }

        match self.create_user(username, password).await {
            Ok(user) => Ok(Some(user)),
            Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
                self.get_user_by_credentials(username, password).await
            }
            Err(error) => Err(error),
        }
    }

    /// Create a new user. Fails if a user with the same normalized username already exists.
    pub async fn create_user(
        &self,
        username: &str,
        password: &Password,
    ) -> Result<User, sqlx::Error> {
        let local_credential = LocalCredential::from_password(password).await?;
        let username_key = Self::normalized_username_key(username);
        let virtual_key = generate_token();
        let user_id = UserId::from(generate_token());
        let now = chrono::Utc::now();

        sqlx::query(
            r#"
            INSERT INTO users
                (id, virtual_key, original_username, original_password_hash, local_credential_kind, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&user_id)
        .bind(&virtual_key)
        .bind(&username_key)
        .bind(local_credential.stored_hash())
        .bind(local_credential.kind())
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        Ok(User {
            id: user_id,
            virtual_key,
            original_username: username_key,
            local_credential,
            created_at: now,
            updated_at: now,
        })
    }

    /// Get user by username (case-insensitive, trimmed)
    pub async fn get_user_by_username(&self, username: &str) -> Result<Option<User>, sqlx::Error> {
        let user = sqlx::query_as::<_, User>(
            r#"
            SELECT id, virtual_key, original_username, original_password_hash,
                   local_credential_kind, created_at, updated_at
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
            SELECT id, virtual_key, original_username, original_password_hash,
                   local_credential_kind, created_at, updated_at
            FROM users
            WHERE virtual_key = ?
            "#,
        )
        .bind(virtual_key)
        .fetch_optional(&self.pool)
        .await?;

        Ok(user)
    }

    /// Resolve either a login token or an application API key to its virtual user.
    pub async fn get_user_by_token(&self, token: &str) -> Result<Option<User>, sqlx::Error> {
        if let Some(user) = self.get_user_by_virtual_key(token).await? {
            return Ok(Some(user));
        }

        sqlx::query_as::<_, User>(
            r#"
            SELECT u.id, u.virtual_key, u.original_username, u.original_password_hash,
                   u.local_credential_kind, u.created_at, u.updated_at
            FROM users u
            JOIN virtual_user_api_keys key ON key.user_id = u.id
            WHERE key.access_token = ?
            "#,
        )
        .bind(token)
        .fetch_optional(&self.pool)
        .await
    }

    /// Get user by virtual ID, accepting equivalent UUID formats.
    pub async fn get_user_by_id(&self, id: &str) -> Result<Option<User>, sqlx::Error> {
        let user = sqlx::query_as::<_, User>(
            r#"
            SELECT id, virtual_key, original_username, original_password_hash,
                   local_credential_kind, created_at, updated_at
            FROM users
            WHERE id = ?
            "#,
        )
        .bind(UserId::new(id))
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
        let user = self.get_user_by_username(username).await?;
        if let Some(user) = user {
            if user.local_credential.verify_async(password).await? {
                return self
                    .upgrade_local_password_hash(user, password)
                    .await
                    .map(Some);
            }
        }
        Ok(None)
    }

    pub(super) async fn upgrade_local_password_hash(
        &self,
        mut user: User,
        password: &Password,
    ) -> Result<User, sqlx::Error> {
        let LocalCredential::Password(old_hash) = &user.local_credential else {
            return Ok(user);
        };
        // Legacy mapping ciphertext may depend on this exact SHA-256 verifier.
        // Upgrade mappings first, and retain the old verifier if any mapping
        // cannot yet be decrypted or validated against the backend.
        if self.mapping_key.is_none()
            || self
                .upgrade_mappings_with_verified_password(&user.id, password, None)
                .await?
                .iter()
                .any(|mapping| mapping.credential_format == CredentialFormat::Legacy)
        {
            return Ok(user);
        }
        let updated = LocalCredential::from_password(password).await?;
        let changed = sqlx::query("UPDATE users SET original_password_hash = ?, local_credential_kind = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ? AND local_credential_kind = 'password' AND original_password_hash = ?")
            .bind(updated.stored_hash()).bind(updated.kind()).bind(&user.id).bind(old_hash)
            .execute(&self.pool).await?;
        if changed.rows_affected() > 0 {
            user.local_credential = updated;
        } else {
            let current = self
                .get_user_by_id(&user.id)
                .await?
                .ok_or(sqlx::Error::RowNotFound)?;
            if !current.local_credential.verify_async(password).await? {
                return Err(sqlx::Error::Protocol(
                    "local credentials changed during login".into(),
                ));
            }
            return Ok(current);
        }
        Ok(user)
    }

    /// List all users
    pub async fn list_users(&self) -> Result<Vec<User>, sqlx::Error> {
        let users = sqlx::query_as::<_, User>(
            r#"
            SELECT id, virtual_key, original_username, original_password_hash,
                   local_credential_kind, created_at, updated_at
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

    /// Update user password and re-encrypt server mappings
    pub async fn update_user_password(
        &self,
        user_id: &str,
        old_password: &Password,
        new_password: &Password,
        admin_password: &Password,
    ) -> Result<bool, sqlx::Error> {
        // Verification in the UI may predate slow backend validation. Capture
        // and verify a fresh credential here, then compare it atomically when
        // committing so neither that wait nor Argon2 work permits a stale write.
        let Some(user) = self.get_user_by_id(user_id).await? else {
            return Ok(false);
        };
        if !user.local_credential.verify_async(old_password).await? {
            return Ok(false);
        }
        let local_credential = LocalCredential::from_password(new_password).await?;
        self.update_user_password_if_current(&user, old_password, &local_credential, admin_password)
            .await
    }

    pub(super) async fn update_user_password_if_current(
        &self,
        expected_user: &User,
        old_password: &Password,
        local_credential: &LocalCredential,
        admin_password: &Password,
    ) -> Result<bool, sqlx::Error> {
        let user_id = &expected_user.id;
        let prior_key = expected_user.local_credential.mapping_key();
        let mut transaction = self.pool.begin().await?;

        // 1. Update user password hash
        let now = chrono::Utc::now();

        let res = sqlx::query(
            r#"
            UPDATE users
            SET original_password_hash = ?, local_credential_kind = ?, updated_at = ?
            WHERE id = ? AND original_password_hash = ? AND local_credential_kind = ?
            "#,
        )
        .bind(local_credential.stored_hash())
        .bind(local_credential.kind())
        .bind(now)
        .bind(user_id)
        .bind(expected_user.local_credential.stored_hash())
        .bind(expected_user.local_credential.kind())
        .execute(&mut *transaction)
        .await?;

        if res.rows_affected() == 0 {
            return Ok(false);
        }

        // 2. Re-encrypt all server mappings
        let mappings = sqlx::query_as::<_, ServerMapping>(
            r#"
            SELECT id, user_id, server_id, server_url, mapped_username, mapped_password, auth_method, backend_user_id, encrypted_token, credential_format, created_at, updated_at
            FROM server_mappings
            WHERE user_id = ?
            "#,
        )
        .bind(user_id)
        .fetch_all(&mut *transaction)
        .await?;

        let old_password_hash: HashedPassword = old_password.into();
        let admin_password_hash = admin_password.into();

        for mapping in mappings {
            if mapping.credential_format == CredentialFormat::SessionV1 {
                // No credential re-keying is needed for a local password change.
                continue;
            }
            if let MappingAuth::QuickConnect { token, .. } = &mapping.auth {
                let plaintext = decrypt_password(token, &admin_password_hash)
                    .or_else(|_| decrypt_password(token, &prior_key))
                    .or_else(|_| decrypt_password(token, &old_password_hash))
                    .map_err(|_| {
                        sqlx::Error::Protocol(format!(
                            "Unable to decrypt legacy token for mapping {}",
                            mapping.id
                        ))
                    })?;
                let (encrypted, format) = if let Some(key) = &self.mapping_key {
                    (key.encrypt(&plaintext), CredentialFormat::SessionV1)
                } else {
                    (
                        encrypt_password(&plaintext, &local_credential.mapping_key()),
                        CredentialFormat::Legacy,
                    )
                };
                let encrypted = encrypted.map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
                sqlx::query("UPDATE server_mappings SET encrypted_token = ?, credential_format = ?, updated_at = ? WHERE id = ?")
                    .bind(encrypted.as_str()).bind(format.as_str()).bind(now).bind(mapping.id)
                    .execute(&mut *transaction).await?;
                continue;
            }
            // Decrypt with old credentials
            if let MappingAuth::Password { password, .. } = &mapping.auth {
                if !is_legacy_plaintext(password)
                    && decrypt_password(password, &prior_key).is_err()
                    && decrypt_password(password, &admin_password_hash).is_err()
                    && decrypt_password_with_key_material(password, old_password.as_str()).is_err()
                    && decrypt_password_with_key_material(password, admin_password.as_str())
                        .is_err()
                {
                    return Err(sqlx::Error::Protocol(format!(
                        "Ambiguous legacy mapping {}; reconnect before changing password",
                        mapping.id
                    )));
                }
            }
            let decrypted_password = self
                .decrypt_server_mapping_password(
                    &mapping,
                    &prior_key,
                    &admin_password_hash,
                    Some(old_password),
                    Some(admin_password),
                )
                .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;

            // Encrypt with new password
            let (new_encrypted_password, format) = if let Some(key) = &self.mapping_key {
                (
                    key.encrypt(&decrypted_password),
                    CredentialFormat::SessionV1,
                )
            } else {
                (
                    encrypt_password(&decrypted_password, &local_credential.mapping_key()),
                    CredentialFormat::Legacy,
                )
            };
            let new_encrypted_password =
                new_encrypted_password.map_err(|e| sqlx::Error::Protocol(e.to_string()))?;

            // Update mapping in DB
            sqlx::query(
                r#"
                UPDATE server_mappings
                SET mapped_password = ?, credential_format = ?, updated_at = ?
                WHERE id = ?
                "#,
            )
            .bind(new_encrypted_password)
            .bind(format.as_str())
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
            user.local_credential.verify_async(password).await
        } else {
            Ok(false)
        }
    }
}
