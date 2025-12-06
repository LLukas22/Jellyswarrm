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
                Ok(_) => {
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
