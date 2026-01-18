//! Admin user management handlers

use askama::Template;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    Form,
};
use serde::Deserialize;
use tracing::{error, info};

use crate::{
    admin_storage::AdminUser,
    audit_service::{AuditAction, ResourceType},
    encryption::HashedPassword,
    ui::auth::AuthenticatedUser,
    AppState,
};

#[derive(Template)]
#[template(path = "admin/admins.html")]
pub struct AdminsPageTemplate {
    pub ui_route: String,
    pub is_super_admin: bool,
}

pub struct AdminDisplay {
    pub admin: AdminUser,
    pub can_delete: bool,
}

#[derive(Template)]
#[template(path = "admin/admin_list.html")]
pub struct AdminListTemplate {
    pub admins: Vec<AdminDisplay>,
    pub ui_route: String,
    pub is_super_admin: bool,
    pub current_admin_id: String,
}

#[derive(Deserialize)]
pub struct AddAdminForm {
    pub username: String,
    pub password: String,
    pub is_super_admin: Option<String>,
}

#[derive(Deserialize)]
pub struct ChangePasswordForm {
    pub new_password: String,
}

async fn render_admin_list(state: &AppState, current_user: &AuthenticatedUser) -> Result<String, String> {
    match state.admin_storage.list_admins().await {
        Ok(admins) => {
            let super_admin_count = admins.iter().filter(|a| a.is_super_admin).count();
            let admins_display: Vec<AdminDisplay> = admins
                .into_iter()
                .map(|admin| {
                    // Can delete if: not the last super admin, and not self
                    let is_self = current_user.0.id == format!("admin-{}", admin.id);
                    let is_last_super = admin.is_super_admin && super_admin_count <= 1;
                    AdminDisplay {
                        can_delete: !is_self && !is_last_super,
                        admin,
                    }
                })
                .collect();

            let template = AdminListTemplate {
                admins: admins_display,
                ui_route: state.get_ui_route().await,
                is_super_admin: current_user.0.is_super_admin,
                current_admin_id: current_user.0.id.clone(),
            };

            template.render().map_err(|e| e.to_string())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Main admins management page
pub async fn admins_page(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> impl IntoResponse {
    let template = AdminsPageTemplate {
        ui_route: state.get_ui_route().await,
        is_super_admin: user.0.is_super_admin,
    };

    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render admins template: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

/// Get admin list partial (for HTMX)
pub async fn get_admin_list(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> impl IntoResponse {
    match render_admin_list(&state, &user).await {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render admin list: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Error").into_response()
        }
    }
}

/// Add a new admin (super admin only)
pub async fn add_admin(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Form(form): Form<AddAdminForm>,
) -> Response {
    // Only super admins can create new admins
    if !user.0.is_super_admin {
        return (
            StatusCode::FORBIDDEN,
            Html("<div class=\"alert alert-error\">Only super admins can create new admins</div>"),
        )
            .into_response();
    }

    // Validate the form data
    if form.username.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Username cannot be empty</div>"),
        )
            .into_response();
    }

    if form.password.len() < 4 {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Password must be at least 4 characters</div>"),
        )
            .into_response();
    }

    let password_hash = HashedPassword::from_password(&form.password);
    let is_super_admin = form.is_super_admin.is_some();

    // Try to add the admin
    match state
        .admin_storage
        .create_admin(form.username.trim(), &password_hash, is_super_admin)
        .await
    {
        Ok(admin_id) => {
            info!(
                "Created new admin: {} (ID: {}, super_admin: {})",
                form.username, admin_id, is_super_admin
            );

            // Log the action
            let _ = state.audit.log_admin_action(
                &user.0.id,
                &user.0.username,
                AuditAction::Create,
                ResourceType::Admin,
                Some(&admin_id.to_string()),
                Some(form.username.trim()),
                Some(&format!("Created admin (super_admin: {})", is_super_admin)),
                None,
            ).await;

            // Return updated admin list
            get_admin_list(State(state), user).await.into_response()
        }
        Err(e) => {
            error!("Failed to add admin: {}", e);

            let error_message = if e.to_string().contains("UNIQUE constraint failed") {
                "An admin with that username already exists"
            } else {
                "Failed to create admin"
            };

            (
                StatusCode::BAD_REQUEST,
                Html(format!(
                    "<div class=\"alert alert-error\">{error_message}</div>"
                )),
            )
                .into_response()
        }
    }
}

/// Delete an admin (super admin only)
pub async fn delete_admin(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(admin_id): Path<i64>,
) -> Response {
    // Only super admins can delete admins
    if !user.0.is_super_admin {
        return (
            StatusCode::FORBIDDEN,
            Html("<div class=\"alert alert-error\">Only super admins can delete admins</div>"),
        )
            .into_response();
    }

    // Cannot delete self
    if user.0.id == format!("admin-{}", admin_id) {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Cannot delete your own account</div>"),
        )
            .into_response();
    }

    match state.admin_storage.delete_admin(admin_id).await {
        Ok(true) => {
            info!("Deleted admin with ID: {}", admin_id);

            // Log the action
            let _ = state.audit.log_admin_action(
                &user.0.id,
                &user.0.username,
                AuditAction::Delete,
                ResourceType::Admin,
                Some(&admin_id.to_string()),
                None,
                Some("Deleted admin account"),
                None,
            ).await;

            get_admin_list(State(state), user).await.into_response()
        }
        Ok(false) => (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Cannot delete the last super admin</div>"),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to delete admin: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to delete admin</div>"),
            )
                .into_response()
        }
    }
}

/// Change admin password
pub async fn change_admin_password(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(admin_id): Path<i64>,
    Form(form): Form<ChangePasswordForm>,
) -> Response {
    // Super admins can change any password, regular admins can only change their own
    let is_self = user.0.id == format!("admin-{}", admin_id);
    if !user.0.is_super_admin && !is_self {
        return (
            StatusCode::FORBIDDEN,
            Html("<div class=\"alert alert-error\">You can only change your own password</div>"),
        )
            .into_response();
    }

    if form.new_password.len() < 4 {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Password must be at least 4 characters</div>"),
        )
            .into_response();
    }

    let password_hash = HashedPassword::from_password(&form.new_password);

    match state
        .admin_storage
        .update_password(admin_id, &password_hash)
        .await
    {
        Ok(true) => {
            info!("Updated password for admin ID: {}", admin_id);

            // Log the action
            let _ = state.audit.log_admin_action(
                &user.0.id,
                &user.0.username,
                AuditAction::PasswordChange,
                ResourceType::Admin,
                Some(&admin_id.to_string()),
                None,
                Some("Password changed"),
                None,
            ).await;

            (
                StatusCode::OK,
                Html("<div class=\"alert alert-success\">Password updated successfully</div>"),
            )
                .into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Html("<div class=\"alert alert-error\">Admin not found</div>"),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to update admin password: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to update password</div>"),
            )
                .into_response()
        }
    }
}

/// Promote admin to super admin
pub async fn promote_admin(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(admin_id): Path<i64>,
) -> Response {
    if !user.0.is_super_admin {
        return (
            StatusCode::FORBIDDEN,
            Html("<div class=\"alert alert-error\">Only super admins can promote admins</div>"),
        )
            .into_response();
    }

    match state.admin_storage.promote_to_super_admin(admin_id).await {
        Ok(true) => {
            info!("Promoted admin ID: {} to super admin", admin_id);

            // Log the action
            let _ = state.audit.log_admin_action(
                &user.0.id,
                &user.0.username,
                AuditAction::Update,
                ResourceType::Admin,
                Some(&admin_id.to_string()),
                None,
                Some("Promoted to super admin"),
                None,
            ).await;

            get_admin_list(State(state), user).await.into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Html("<div class=\"alert alert-error\">Admin not found</div>"),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to promote admin: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to promote admin</div>"),
            )
                .into_response()
        }
    }
}

/// Demote super admin
pub async fn demote_admin(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(admin_id): Path<i64>,
) -> Response {
    if !user.0.is_super_admin {
        return (
            StatusCode::FORBIDDEN,
            Html("<div class=\"alert alert-error\">Only super admins can demote admins</div>"),
        )
            .into_response();
    }

    // Cannot demote self
    if user.0.id == format!("admin-{}", admin_id) {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Cannot demote yourself</div>"),
        )
            .into_response();
    }

    match state.admin_storage.demote_from_super_admin(admin_id).await {
        Ok(true) => {
            info!("Demoted admin ID: {} from super admin", admin_id);

            // Log the action
            let _ = state.audit.log_admin_action(
                &user.0.id,
                &user.0.username,
                AuditAction::Update,
                ResourceType::Admin,
                Some(&admin_id.to_string()),
                None,
                Some("Demoted from super admin"),
                None,
            ).await;

            get_admin_list(State(state), user).await.into_response()
        }
        Ok(false) => (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Cannot demote the last super admin</div>"),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to demote admin: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to demote admin</div>"),
            )
                .into_response()
        }
    }
}
