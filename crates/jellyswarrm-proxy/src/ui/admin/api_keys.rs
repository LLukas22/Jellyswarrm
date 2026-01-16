use askama::Template;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse},
    Form,
};
use serde::Deserialize;
use tracing::{error, info};

use crate::{api_keys::ApiKey, AppState};

#[derive(Template)]
#[template(path = "admin/api_keys.html")]
pub struct ApiKeysTemplate {
    pub ui_route: String,
    pub keys: Vec<ApiKey>,
    pub new_key: Option<String>, // Only shown once after creation
    pub new_key_name: Option<String>,
}

#[derive(Template)]
#[template(path = "admin/api_keys_list.html")]
pub struct ApiKeysListTemplate {
    pub keys: Vec<ApiKey>,
    pub ui_route: String,
}

#[derive(Deserialize)]
pub struct CreateApiKeyForm {
    pub name: String,
    pub permissions: Option<String>, // Comma-separated list
    pub expires_in_days: Option<i64>,
}

/// GET /admin/api-keys - API keys management page
pub async fn get_api_keys_page(State(state): State<AppState>) -> impl IntoResponse {
    let ui_route = state.get_ui_route().await;

    let keys = match state.api_keys.list_keys().await {
        Ok(k) => k,
        Err(e) => {
            error!("Failed to list API keys: {}", e);
            Vec::new()
        }
    };

    let template = ApiKeysTemplate {
        ui_route,
        keys,
        new_key: None,
        new_key_name: None,
    };

    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

/// POST /admin/api-keys - Create new API key
pub async fn create_api_key(
    State(state): State<AppState>,
    Form(form): Form<CreateApiKeyForm>,
) -> impl IntoResponse {
    let ui_route = state.get_ui_route().await;

    // Parse permissions
    let permissions: Vec<String> = form
        .permissions
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let created_by = "admin"; // TODO: Get from session

    match state
        .api_keys
        .create_key(&form.name, permissions, created_by, form.expires_in_days)
        .await
    {
        Ok(key_with_secret) => {
            info!("Created API key: {}", form.name);

            // Return page with the new key visible (one time only!)
            let keys = state.api_keys.list_keys().await.unwrap_or_default();

            let template = ApiKeysTemplate {
                ui_route,
                keys,
                new_key: Some(key_with_secret.key),
                new_key_name: Some(form.name),
            };

            match template.render() {
                Ok(html) => Html(html).into_response(),
                Err(e) => {
                    error!("Template render error: {}", e);
                    (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
                }
            }
        }
        Err(e) => {
            error!("Failed to create API key: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Failed to create API key").into_response()
        }
    }
}

/// DELETE /admin/api-keys/:id - Delete an API key
pub async fn delete_api_key(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let ui_route = state.get_ui_route().await;

    match state.api_keys.delete_key(id).await {
        Ok(deleted) => {
            if deleted {
                info!("Deleted API key: {}", id);

                // Return updated list
                let keys = state.api_keys.list_keys().await.unwrap_or_default();
                let template = ApiKeysListTemplate { keys, ui_route };

                match template.render() {
                    Ok(html) => Html(html).into_response(),
                    Err(e) => {
                        error!("Template render error: {}", e);
                        (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
                    }
                }
            } else {
                (StatusCode::NOT_FOUND, "API key not found").into_response()
            }
        }
        Err(e) => {
            error!("Failed to delete API key: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response()
        }
    }
}
