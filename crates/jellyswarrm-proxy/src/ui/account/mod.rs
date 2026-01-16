//! User self-service account management

use askama::Template;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Redirect},
    Form,
};
use axum_login::AuthSession;
use serde::Deserialize;
use tracing::{error, info};

use crate::{
    encryption::Password,
    ui::auth::Backend,
    AppState,
};

#[derive(Template)]
#[template(path = "account/index.html")]
pub struct AccountTemplate {
    pub ui_route: String,
    pub username: String,
    pub sessions: Vec<SessionInfo>,
    pub message: Option<String>,
    pub error: Option<String>,
}

pub struct SessionInfo {
    pub id: i64,
    pub server_name: String,
    pub device: String,
    pub device_id: String,
    pub created_at: String,
    pub is_current: bool,
}

#[derive(Deserialize)]
pub struct ChangePasswordForm {
    pub current_password: String,
    pub new_password: String,
    pub confirm_password: String,
}

/// GET /account - User account page
pub async fn get_account_page(
    State(state): State<AppState>,
    auth_session: AuthSession<Backend>,
) -> impl IntoResponse {
    let user = match auth_session.user {
        Some(ref u) => u,
        None => {
            return Redirect::to("/login").into_response();
        }
    };

    let ui_route = state.get_ui_route().await;

    // Get user's sessions
    let sessions = get_user_sessions(&state, &user.id).await;

    let template = AccountTemplate {
        ui_route,
        username: user.username.clone(),
        sessions,
        message: None,
        error: None,
    };

    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

/// POST /account/password - Change password
pub async fn change_password(
    State(state): State<AppState>,
    auth_session: AuthSession<Backend>,
    Form(form): Form<ChangePasswordForm>,
) -> impl IntoResponse {
    let user = match auth_session.user {
        Some(ref u) => u,
        None => {
            return Redirect::to("/login").into_response();
        }
    };

    let ui_route = state.get_ui_route().await;
    let sessions = get_user_sessions(&state, &user.id).await;

    // Validate passwords match
    if form.new_password != form.confirm_password {
        let template = AccountTemplate {
            ui_route,
            username: user.username.clone(),
            sessions,
            message: None,
            error: Some("New passwords do not match".to_string()),
        };
        return match template.render() {
            Ok(html) => Html(html).into_response(),
            Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response(),
        };
    }

    // Validate password length
    if form.new_password.len() < 8 {
        let template = AccountTemplate {
            ui_route,
            username: user.username.clone(),
            sessions,
            message: None,
            error: Some("Password must be at least 8 characters".to_string()),
        };
        return match template.render() {
            Ok(html) => Html(html).into_response(),
            Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response(),
        };
    }

    // Verify current password
    let current_password: Password = form.current_password.clone().into();
    match state
        .user_authorization
        .verify_user_password(&user.id, &current_password)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            let template = AccountTemplate {
                ui_route,
                username: user.username.clone(),
                sessions,
                message: None,
                error: Some("Current password is incorrect".to_string()),
            };
            return match template.render() {
                Ok(html) => Html(html).into_response(),
                Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response(),
            };
        }
        Err(e) => {
            error!("Failed to verify password: {}", e);
            let template = AccountTemplate {
                ui_route,
                username: user.username.clone(),
                sessions,
                message: None,
                error: Some("An error occurred".to_string()),
            };
            return match template.render() {
                Ok(html) => Html(html).into_response(),
                Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response(),
            };
        }
    }

    // Update password
    // Note: current_password is used both for verification and as master password for re-encrypting
    // server mappings (the user's password is used to encrypt their stored server credentials)
    let new_password: Password = form.new_password.clone().into();
    match state
        .user_authorization
        .update_user_password(&user.id, &current_password, &new_password, &current_password)
        .await
    {
        Ok(_) => {
            info!("User {} changed password", user.username);

            let template = AccountTemplate {
                ui_route,
                username: user.username.clone(),
                sessions,
                message: Some("Password changed successfully".to_string()),
                error: None,
            };
            match template.render() {
                Ok(html) => Html(html).into_response(),
                Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response(),
            }
        }
        Err(e) => {
            error!("Failed to update password: {}", e);
            let template = AccountTemplate {
                ui_route,
                username: user.username.clone(),
                sessions,
                message: None,
                error: Some("Failed to update password".to_string()),
            };
            match template.render() {
                Ok(html) => Html(html).into_response(),
                Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response(),
            }
        }
    }
}

/// POST /account/sessions/:id/revoke - Revoke a specific session
pub async fn revoke_session(
    State(state): State<AppState>,
    auth_session: AuthSession<Backend>,
    axum::extract::Path(session_id): axum::extract::Path<i64>,
) -> impl IntoResponse {
    let user = match auth_session.user {
        Some(ref u) => u,
        None => {
            return Redirect::to("/login").into_response();
        }
    };

    // Verify this session belongs to the user
    match state
        .user_authorization
        .revoke_user_session(&user.id, session_id)
        .await
    {
        Ok(revoked) => {
            if revoked {
                info!(
                    "User {} revoked session {}",
                    user.username, session_id
                );
            }
        }
        Err(e) => {
            error!("Failed to revoke session: {}", e);
        }
    }

    Redirect::to("/account").into_response()
}

/// POST /account/sessions/revoke-all - Revoke all sessions except current
pub async fn revoke_all_sessions(
    State(state): State<AppState>,
    auth_session: AuthSession<Backend>,
) -> impl IntoResponse {
    let user = match auth_session.user {
        Some(ref u) => u,
        None => {
            return Redirect::to("/login").into_response();
        }
    };

    match state
        .user_authorization
        .revoke_all_user_sessions(&user.id)
        .await
    {
        Ok(count) => {
            info!(
                "User {} revoked {} sessions",
                user.username, count
            );
        }
        Err(e) => {
            error!("Failed to revoke all sessions: {}", e);
        }
    }

    Redirect::to("/account").into_response()
}

async fn get_user_sessions(state: &AppState, user_id: &str) -> Vec<SessionInfo> {
    let sessions = match state
        .user_authorization
        .get_user_sessions(user_id, None)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to get user sessions: {}", e);
            return Vec::new();
        }
    };

    sessions
        .into_iter()
        .map(|(session, server)| {
            SessionInfo {
                id: session.id,
                server_name: server.name.clone(),
                device: session.device.device.clone(),
                device_id: session.device.device_id.clone(),
                created_at: session.created_at.format("%Y-%m-%d %H:%M").to_string(),
                is_current: false, // TODO: Determine current session
            }
        })
        .collect()
}
