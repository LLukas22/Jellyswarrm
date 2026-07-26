use crate::{
    config::DebugUser, encryption::HashedPassword, server_id::ServerId,
    server_storage::ServerStorageService, user_authorization_service::UserAuthorizationService,
};

pub async fn initialize_debug_user(
    config: &DebugUser,
    server_ids: &[ServerId],
    user_authorization: &UserAuthorizationService,
    server_storage: &ServerStorageService,
) -> Result<usize, sqlx::Error> {
    let user = user_authorization
        .get_or_create_user(&config.username, &config.password)
        .await?;
    let encryption_key = HashedPassword::from(&config.password);

    for server_id in server_ids {
        let server = server_storage
            .get_server_by_id(*server_id)
            .await?
            .ok_or(sqlx::Error::RowNotFound)?;
        user_authorization
            .add_server_mapping(
                &user.id,
                &server,
                &config.username,
                &config.password,
                Some(&encryption_key),
            )
            .await?;
    }

    Ok(server_ids.len())
}

#[cfg(test)]
mod tests {
    use crate::{
        config::{DebugUser, MediaStreamingMode, MIGRATOR},
        encryption::{decrypt_password, Password},
        server_storage::ServerStorageService,
        user_authorization_service::UserAuthorizationService,
    };
    use sqlx::SqlitePool;

    use super::initialize_debug_user;

    async fn setup() -> (
        UserAuthorizationService,
        ServerStorageService,
        Vec<crate::server_id::ServerId>,
    ) {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let user_authorization = UserAuthorizationService::new(pool.clone());
        let server_storage = ServerStorageService::new(pool);
        let mut server_ids = Vec::new();

        for (name, url) in [
            ("Movies", "http://localhost:8096"),
            ("Shows", "http://localhost:8097"),
        ] {
            server_ids.push(
                server_storage
                    .add_server(name, url, 100, MediaStreamingMode::Proxy)
                    .await
                    .unwrap(),
            );
        }

        (user_authorization, server_storage, server_ids)
    }

    fn debug_user() -> DebugUser {
        DebugUser {
            username: "test".to_string(),
            password: Password::from("test"),
        }
    }

    #[tokio::test]
    async fn initialize_debug_user_creates_local_user_and_encrypted_mappings() {
        let (user_authorization, server_storage, server_ids) = setup().await;
        let config = debug_user();

        initialize_debug_user(&config, &server_ids, &user_authorization, &server_storage)
            .await
            .unwrap();

        let user = user_authorization
            .get_user_by_credentials("test", &Password::from("test"))
            .await
            .unwrap()
            .unwrap();
        let mappings = user_authorization
            .list_server_mappings(&user.id)
            .await
            .unwrap();
        let mapped_passwords = mappings
            .iter()
            .map(|mapping| {
                decrypt_password(&mapping.mapped_password, &user.original_password_hash)
                    .unwrap()
                    .into_inner()
            })
            .collect::<Vec<_>>();

        assert_eq!(mapped_passwords, vec!["test", "test"]);
    }

    #[tokio::test]
    async fn initialize_debug_user_is_idempotent() {
        let (user_authorization, server_storage, server_ids) = setup().await;
        let config = debug_user();

        for _ in 0..2 {
            initialize_debug_user(&config, &server_ids, &user_authorization, &server_storage)
                .await
                .unwrap();
        }

        let users = user_authorization.list_users().await.unwrap();
        let mappings = user_authorization
            .list_server_mappings(&users[0].id)
            .await
            .unwrap();

        assert_eq!((users.len(), mappings.len()), (1, 2));
    }
}
