//! Create, list, and enforce permissions for named virtual-user API keys.
use super::*;

impl UserAuthorizationService {
    pub(crate) async fn create_api_key(
        &self,
        issuer: &LocalUser,
        app_name: &str,
    ) -> Result<(VirtualUserApiKey, i64), sqlx::Error> {
        let access_token = generate_token();
        let mut tx = self.pool.begin().await?;
        let key = sqlx::query_as::<_, VirtualUserApiKey>(
            r#"
            INSERT INTO virtual_user_api_keys (user_id, app_name, access_token)
            SELECT user_id, ?, ? FROM client_logins
            WHERE id = ? AND user_id = ? AND token_hash IS NOT NULL
            RETURNING id, user_id, app_name, access_token, created_at
            "#,
        )
        .bind(app_name.trim())
        .bind(access_token)
        .bind(issuer.login_id)
        .bind(&issuer.id)
        .fetch_one(&mut *tx)
        .await?;
        let login_id = sqlx::query_scalar("SELECT id FROM client_logins WHERE api_key_id=?")
            .bind(key.id)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok((key, login_id))
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

    pub(crate) async fn delete_api_key(
        &self,
        user_id: &str,
        token: &str,
    ) -> Result<bool, sqlx::Error> {
        let deleted =
            sqlx::query("DELETE FROM virtual_user_api_keys WHERE user_id=? AND access_token=?")
                .bind(user_id)
                .bind(token)
                .execute(&self.pool)
                .await?
                .rows_affected()
                != 0;
        self.session_changes.send_replace(());
        Ok(deleted)
    }
}
