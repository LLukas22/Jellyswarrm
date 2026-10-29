//! Production authorization composition, shared with full HTTP fixtures.
use crate::{
    auth::UserAuthorizationService, config::AppConfig, domain::security::MappingEncryptionKey,
};

pub(crate) async fn build_authorization(
    pool: sqlx::SqlitePool,
    config: &AppConfig,
) -> anyhow::Result<UserAuthorizationService> {
    let mapping_key = MappingEncryptionKey::from_session_key(&config.session_key)?;
    let mut authorization =
        UserAuthorizationService::with_mapping_key(pool, mapping_key, (&config.password).into());
    authorization
        .enable_session_encryption(&config.session_key)
        .await?;
    Ok(authorization)
}
