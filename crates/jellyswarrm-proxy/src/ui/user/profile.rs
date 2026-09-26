use askama::Template;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse},
    Form,
};
use serde::Deserialize;
use tracing::error;

use crate::{encryption::Password, ui::auth::AuthenticatedUser, AppState};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{AppConfig, MIGRATOR},
        encryption::{HashedPassword, MappingEncryptionKey},
        handlers::quick_connect::QuickConnectStorage,
        media_storage_service::MediaStorageService,
        server_storage::{Server, ServerStorageService},
        session_storage::SessionStorage,
        ui::auth::{User, UserRole},
        user_authorization_service::{CredentialFormat, UserAuthorizationService},
        virtual_library_service::VirtualLibraryService,
        DataContext, ProxyProcessors,
    };
    use sqlx::SqlitePool;
    use std::sync::Arc;
    use wiremock::{
        matchers::{body_json, method, path},
        Mock, MockServer, ResponseTemplate,
    };

    async fn password_change_with_legacy_mapping(
        backend_status: u16,
        ambiguous: &str,
        raw_admin_key: bool,
    ) {
        let upstream = MockServer::start().await;
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let legacy = UserAuthorizationService::new(pool.clone());
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
        let local = legacy.create_user("local", &"before".into()).await.unwrap();
        let config = AppConfig::default();
        let encryption_key =
            raw_admin_key.then(|| HashedPassword::from_hashed(config.password.as_str().into()));
        legacy
            .add_server_mapping(
                &local.id,
                &server,
                "remote",
                &ambiguous.into(),
                encryption_key.as_ref(),
            )
            .await
            .unwrap();
        let service = UserAuthorizationService::with_mapping_key(
            pool.clone(),
            MappingEncryptionKey::from_session_key(&config.session_key).unwrap(),
            (&config.password).into(),
        );
        let server_storage = ServerStorageService::new(pool.clone());
        let media_storage = MediaStorageService::new(pool.clone());
        let context = DataContext {
            user_authorization: Arc::new(service),
            server_storage: Arc::new(server_storage.clone()),
            media_storage: Arc::new(media_storage.clone()),
            virtual_library_service: Arc::new(VirtualLibraryService::new(
                pool,
                server_storage,
                media_storage,
            )),
            play_sessions: Arc::new(SessionStorage::new()),
            config: Arc::new(tokio::sync::RwLock::new(config)),
        };
        let processors = ProxyProcessors::new(context.clone());
        let state = AppState::new(
            reqwest::Client::new(),
            reqwest::Client::new(),
            context,
            processors,
            QuickConnectStorage::new(),
        );
        Mock::given(method("POST"))
            .and(path("/Users/AuthenticateByName"))
            .and(body_json(
                serde_json::json!({"Username": "remote", "Pw": ambiguous}),
            ))
            .respond_with(
                ResponseTemplate::new(backend_status).set_body_json(serde_json::json!({
                    "AccessToken": "validation-token", "User": {"Id": "remote-id", "Name": "remote"}
                })),
            )
            .expect(if raw_admin_key { 0 } else { 1 })
            .mount(&upstream)
            .await;
        Mock::given(method("POST"))
            .and(path("/Sessions/Logout"))
            .respond_with(ResponseTemplate::new(204))
            .expect(if backend_status == 200 && !raw_admin_key {
                1
            } else {
                0
            })
            .mount(&upstream)
            .await;
        let response = post_user_password(
            State(state.clone()),
            AuthenticatedUser(User {
                id: local.id.clone(),
                username: "local".into(),
                local_credential: local.local_credential.clone(),
                role: UserRole::User,
            }),
            Form(ChangePasswordForm {
                current_password: "before".into(),
                new_password: "after".into(),
                confirm_password: "after".into(),
            }),
        )
        .await
        .into_response();
        let body = axum::body::to_bytes(response.into_body(), 16 * 1024)
            .await
            .unwrap();
        let html = std::str::from_utf8(&body).unwrap();
        let mapping = state
            .user_authorization
            .get_server_mapping(&local.id, &server)
            .await
            .unwrap()
            .unwrap();
        if backend_status == 200 || raw_admin_key {
            assert!(html.contains("Password updated successfully"), "{html}");
            assert_eq!(mapping.credential_format, CredentialFormat::SessionV1);
            assert!(state
                .user_authorization
                .verify_user_password(&local.id, &"after".into())
                .await
                .unwrap());
            assert!(!state
                .user_authorization
                .verify_user_password(&local.id, &"before".into())
                .await
                .unwrap());
        } else {
            assert!(
                html.contains("reconnect them from Connected Servers"),
                "{html}"
            );
            assert!(html.contains("Your local password has not been changed"));
            assert_eq!(mapping.credential_format, CredentialFormat::Legacy);
            assert_eq!(
                state
                    .user_authorization
                    .get_user_by_id(&local.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .local_credential,
                local.local_credential
            );
        }
        assert_eq!(
            state
                .user_authorization
                .decrypt_server_mapping_password(
                    &mapping,
                    &local.local_credential.mapping_key(),
                    &HashedPassword::from_password("unused"),
                    None,
                    None
                )
                .unwrap()
                .as_str(),
            ambiguous
        );
    }

    #[tokio::test]
    async fn password_change_validates_and_upgrades_ambiguous_legacy_password() {
        password_change_with_legacy_mapping(200, "ABCDEFGHIJKLMNOPQRSTabcdefghijklmnopqrst", false)
            .await;
        password_change_with_legacy_mapping(200, "0123456789abcdef0123456789abcdef01234567", false)
            .await;
    }

    #[tokio::test]
    async fn password_change_preserves_credentials_and_explains_failed_validation() {
        password_change_with_legacy_mapping(401, "ABCDEFGHIJKLMNOPQRSTabcdefghijklmnopqrst", false)
            .await;
        password_change_with_legacy_mapping(503, "ABCDEFGHIJKLMNOPQRSTabcdefghijklmnopqrst", false)
            .await;
    }

    #[tokio::test]
    async fn password_change_rekeys_raw_admin_mapping_without_backend_validation() {
        password_change_with_legacy_mapping(503, "backend-password", true).await;
    }
}

#[derive(Template)]
#[template(path = "user/user_profile.html")]
pub struct UserProfileTemplate {
    pub username: String,
    pub ui_route: String,
}

#[derive(Deserialize)]
pub struct ChangePasswordForm {
    pub current_password: Password,
    pub new_password: Password,
    pub confirm_password: Password,
}

pub async fn get_user_profile(
    State(state): State<AppState>,
    AuthenticatedUser(user): AuthenticatedUser,
) -> impl IntoResponse {
    let template = UserProfileTemplate {
        username: user.username,
        ui_route: state.get_ui_route().await,
    };

    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render user profile template: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

pub async fn post_user_password(
    State(state): State<AppState>,
    AuthenticatedUser(user): AuthenticatedUser,
    Form(form): Form<ChangePasswordForm>,
) -> impl IntoResponse {
    if form.new_password != form.confirm_password {
        return (
            StatusCode::OK,
            Html(r#"
                <div role="alert" style="background-color: #c62828; color: white; padding: 0.75rem; border-radius: 0.25rem;">
                    <i class="fas fa-exclamation-circle" style="margin-right: 0.5rem;"></i> New passwords do not match
                </div>
            "#),
        )
            .into_response();
    }

    match state
        .user_authorization
        .verify_user_password(&user.id, &form.current_password)
        .await
    {
        Ok(true) => {
            if let Err(error) = super::common::prepare_mappings_for_password_change(
                &state, &user, &form.current_password,
            ).await {
                error!("Failed to validate legacy mappings before password change: {error}");
                return (StatusCode::OK, Html(
                    "<div role=\"alert\">Could not validate your legacy server connections. Check that the servers are reachable, or reconnect them from Connected Servers, then retry. Your local password has not been changed.</div>".to_string()
                )).into_response();
            }
            let admin_password = {
                let config = state.config.read().await;
                config.password.clone()
            };

            match state
                .user_authorization
                .update_user_password(
                    &user.id,
                    &form.current_password,
                    &form.new_password,
                    &admin_password,
                )
                .await
            {
                Ok(true) => {
                    let logout_url = format!("/{}/logout", state.get_ui_route().await);
                    (
                        StatusCode::OK,
                        Html(format!(r#"
                            <div role="alert" style="background-color: #2e7d32; color: white; padding: 0.75rem; border-radius: 0.25rem;">
                                <i class="fas fa-check-circle" style="margin-right: 0.5rem;"></i> Password updated successfully
                            </div>
                            <script>
                                document.getElementById("password_form").reset();
                                setTimeout(function() {{
                                    alert("Password changed successfully. You will be logged out.");
                                    window.location.href = "{}";
                                }}, 100);
                            </script>
                        "#, logout_url)),
                    )
                        .into_response()
                },
                Ok(false) => (
                    StatusCode::OK,
                    Html("<div role=\"alert\">Your credentials changed while this request was in progress. Your requested password change was not applied. Reload the page and retry with your current password.</div>".to_string()),
                ).into_response(),
                Err(e) => {
                    error!("Failed to update password: {}", e);
                    (
                        StatusCode::OK,
                        Html(r#"
                            <div role="alert" style="background-color: #c62828; color: white; padding: 0.75rem; border-radius: 0.25rem;">
                                <i class="fas fa-exclamation-circle" style="margin-right: 0.5rem;"></i> Database error
                            </div>
                        "#.to_string()),
                    )
                        .into_response()
                }
            }
        }
        Ok(false) => (
            StatusCode::OK,
            Html(r#"
                <div role="alert" style="background-color: #c62828; color: white; padding: 0.75rem; border-radius: 0.25rem;">
                    <i class="fas fa-exclamation-circle" style="margin-right: 0.5rem;"></i> Incorrect current password
                </div>
            "#),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to verify password: {}", e);
            (
                StatusCode::OK,
                Html(r#"
                    <div role="alert" style="background-color: #c62828; color: white; padding: 0.75rem; border-radius: 0.25rem;">
                        <i class="fas fa-exclamation-circle" style="margin-right: 0.5rem;"></i> Database error
                    </div>
                "#),
            )
                .into_response()
        }
    }
}
