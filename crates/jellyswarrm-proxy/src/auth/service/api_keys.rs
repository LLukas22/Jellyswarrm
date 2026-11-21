//! Create, list, and enforce permissions for named virtual-user API keys.
use super::*;

impl UserAuthorizationService {
    pub(crate) async fn create_api_key(
        &self,
        user_id: &str,
        app_name: &str,
        login_id: Option<i64>,
    ) -> Result<VirtualUserApiKey, sqlx::Error> {
        let access_token = generate_token();
        let mut tx = self.pool.begin().await?;
        let key = sqlx::query_as::<_, VirtualUserApiKey>(
            r#"
            INSERT INTO virtual_user_api_keys (user_id, app_name, access_token)
            VALUES (?, ?, ?)
            RETURNING id, user_id, app_name, access_token, created_at
            "#,
        )
        .bind(user_id)
        .bind(app_name.trim())
        .bind(access_token)
        .fetch_one(&mut *tx)
        .await?;
        if let Some(login_id) = login_id {
            // Give the read-only integration its own bindings so signing out of the
            // issuing client does not interrupt a scanner or media integration.
            let binding_id: i64 = sqlx::query_scalar(
                "INSERT INTO client_logins(user_id, api_key_id)
                 SELECT user_id, ? FROM client_logins WHERE id = ? AND user_id = ? AND token_hash IS NOT NULL RETURNING id"
            )
            .bind(key.id)
            .bind(login_id)
            .bind(user_id)
            .fetch_one(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO authorization_sessions
                 (user_id, login_id, mapping_id, server_url, client, device, device_id, upstream_device_id, version, jellyfin_token, token_format, original_user_id, expires_at)
                 SELECT user_id, ?, mapping_id, server_url, client, device, device_id, upstream_device_id, version, jellyfin_token, token_format, original_user_id, expires_at
                 FROM authorization_sessions WHERE login_id = ? AND user_id = ?"
            )
            .bind(binding_id)
            .bind(login_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(key)
    }

    pub(crate) async fn list_api_keys(
        &self,
        user_id: &str,
    ) -> Result<Vec<VirtualUserApiKey>, sqlx::Error> {
        sqlx::query_as::<_, VirtualUserApiKey>(
            r#"
            SELECT id, user_id, app_name, access_token, created_at
            FROM virtual_user_api_keys
            WHERE user_id = ?
            ORDER BY id ASC
            "#,
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
    }

    pub(crate) async fn is_read_only_api_key(&self, token: &str) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM virtual_user_api_keys \
             WHERE access_token = ?)",
        )
        .bind(token)
        .fetch_one(&self.pool)
        .await
    }
}
