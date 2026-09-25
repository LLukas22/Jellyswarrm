use crate::{
    encryption::HashedPassword,
    models::User,
    server_storage::Server,
    user_authorization_service::{MappingAuth, ServerMapping, User as LocalUser},
    AppState,
};
use jellyfin_api::{ClientInfo, JellyfinClient};

pub fn mapping_device_id(server: &Server, user_id: &str) -> String {
    format!("jellyswarrm-{}-{}", server.id, user_id)
}

/// Check the saved credential against the server before using it for a new
/// client session. A Quick Connect token is a durable credential, not a code.
pub async fn validated_quick_connect_token(
    state: &AppState,
    user: &LocalUser,
    server: &Server,
    mapping: &ServerMapping,
) -> Result<(String, User), String> {
    let MappingAuth::QuickConnect {
        backend_user_id, ..
    } = &mapping.auth
    else {
        return Err("Not a Quick Connect mapping".into());
    };
    let admin_key: HashedPassword = (&state.get_admin_password().await).into();
    let token = state.user_authorization.decrypt_mapping_token(
        mapping,
        &user.local_credential.mapping_key(),
        &admin_key,
    )?;
    let client = JellyfinClient::new_with_client(
        server.url.as_str(),
        ClientInfo {
            client: "Jellyswarrm Proxy".into(),
            device: "Server".into(),
            device_id: mapping_device_id(server, &user.id),
            version: env!("CARGO_PKG_VERSION").into(),
        },
        state.reqwest_client.clone(),
    )
    .map_err(|e| e.to_string())?;
    client.with_token(token.clone()).await;
    let remote_user: User = client
        .get_me_typed()
        .await
        .map_err(|_| "Backend token invalid; reconnect with Quick Connect".to_string())?;
    if remote_user.id != *backend_user_id {
        return Err("Backend account changed; reconnect with Quick Connect".into());
    }
    Ok((token, remote_user))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{AppConfig, MIGRATOR},
        handlers::quick_connect::QuickConnectStorage,
        media_storage_service::MediaStorageService,
        server_storage::ServerStorageService,
        session_storage::SessionStorage,
        user_authorization_service::UserAuthorizationService,
        virtual_library_service::VirtualLibraryService,
        DataContext, ProxyProcessors,
    };
    use std::sync::Arc;
    use wiremock::{
        matchers::{header_regex, method, path},
        Mock, MockServer, ResponseTemplate,
    };

    #[tokio::test]
    async fn validates_token_and_original_backend_account() {
        let upstream = MockServer::start().await;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let server_storage = ServerStorageService::new(pool.clone());
        let media_storage = MediaStorageService::new(pool.clone());
        let context = DataContext {
            user_authorization: Arc::new(UserAuthorizationService::new(pool.clone())),
            server_storage: Arc::new(server_storage.clone()),
            media_storage: Arc::new(media_storage.clone()),
            virtual_library_service: Arc::new(VirtualLibraryService::new(
                pool.clone(),
                server_storage,
                media_storage,
            )),
            play_sessions: Arc::new(SessionStorage::new()),
            config: Arc::new(tokio::sync::RwLock::new(AppConfig::default())),
        };
        let processors = ProxyProcessors::new(context.clone());
        let state = AppState::new(
            reqwest::Client::new(),
            reqwest::Client::new(),
            context,
            processors,
            QuickConnectStorage::new(),
        );
        let server = Server::from_row(
            sqlx::query(
                "INSERT INTO servers (name, url, priority) VALUES ('Backend', ?, 100) RETURNING *",
            )
            .bind(upstream.uri())
            .fetch_one(&pool)
            .await
            .unwrap(),
        )
        .unwrap();
        let local = state
            .user_authorization
            .create_user("local", &"password".into())
            .await
            .unwrap();
        state
            .user_authorization
            .add_quick_connect_mapping(
                &local.id,
                &server,
                "Remote",
                "backend-id",
                "saved-token",
                &local.local_credential.mapping_key(),
            )
            .await
            .unwrap();
        let mapping = state
            .user_authorization
            .get_server_mapping(&local.id, &server)
            .await
            .unwrap()
            .unwrap();

        Mock::given(method("GET"))
            .and(path("/Users/Me"))
            .and(header_regex("authorization", "Token=\\\"saved-token\\\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "Id": "backend-id", "Name": "Remote", "ServerId": "upstream",
                "Policy": { "IsAdministrator": false, "SyncPlayAccess": "None" }
            })))
            .mount(&upstream)
            .await;
        let (token, remote) = validated_quick_connect_token(&state, &local, &server, &mapping)
            .await
            .unwrap();
        assert_eq!(token, "saved-token");
        assert_eq!(remote.id, "backend-id");

        upstream.reset().await;
        Mock::given(method("GET"))
            .and(path("/Users/Me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "Id": "different-id", "Name": "Different", "ServerId": "upstream",
                "Policy": { "IsAdministrator": false, "SyncPlayAccess": "None" }
            })))
            .mount(&upstream)
            .await;
        assert!(
            validated_quick_connect_token(&state, &local, &server, &mapping)
                .await
                .is_err()
        );
    }
}
