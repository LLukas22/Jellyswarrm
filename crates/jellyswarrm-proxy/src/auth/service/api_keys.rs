//! Create, list, and enforce permissions for named virtual-user API keys.
use super::*;

impl UserAuthorizationService {
    pub async fn create_api_key(
        &self,
        user_id: &str,
        app_name: &str,
    ) -> Result<VirtualUserApiKey, sqlx::Error> {
        let access_token = generate_token();
        sqlx::query_as::<_, VirtualUserApiKey>(
            r#"
            INSERT INTO virtual_user_api_keys (user_id, app_name, access_token)
            VALUES (?, ?, ?)
            RETURNING id, user_id, app_name, access_token, created_at
            "#,
        )
        .bind(user_id)
        .bind(app_name.trim())
        .bind(access_token)
        .fetch_one(&self.pool)
        .await
    }

    pub async fn list_api_keys(
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

    pub async fn is_read_only_api_key(&self, token: &str) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM virtual_user_api_keys \
             WHERE access_token = ?)",
        )
        .bind(token)
        .fetch_one(&self.pool)
        .await
    }
}
