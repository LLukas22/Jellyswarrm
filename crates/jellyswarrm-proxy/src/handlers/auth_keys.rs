use axum::{extract::Query, extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use tracing::error;

use crate::{extractors::RequirePrimaryUser, AppState};

#[derive(Deserialize)]
pub struct CreateApiKeyQuery {
    #[serde(rename = "App")]
    app: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ApiKeyItem {
    app_name: String,
    access_token: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ApiKeyList {
    items: Vec<ApiKeyItem>,
    total_record_count: usize,
    start_index: usize,
}

pub async fn create_api_key(
    State(state): State<AppState>,
    Query(query): Query<CreateApiKeyQuery>,
    RequirePrimaryUser(user): RequirePrimaryUser,
) -> Result<StatusCode, StatusCode> {
    if query.app.trim().is_empty() || query.app.trim().len() > 128 {
        return Err(StatusCode::BAD_REQUEST);
    }

    state
        .user_authorization
        .create_api_key(&user.id, &query.app)
        .await
        .map_err(|error| {
            error!("Failed to create virtual API key: {error}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_api_keys(
    State(state): State<AppState>,
    RequirePrimaryUser(user): RequirePrimaryUser,
) -> Result<Json<ApiKeyList>, StatusCode> {
    let keys = state
        .user_authorization
        .list_api_keys(&user.id)
        .await
        .map_err(|error| {
            error!("Failed to list virtual API keys: {error}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let items = keys
        .into_iter()
        .map(|key| ApiKeyItem {
            app_name: key.app_name,
            access_token: key.access_token,
        })
        .collect::<Vec<_>>();

    Ok(Json(ApiKeyList {
        total_record_count: items.len(),
        items,
        start_index: 0,
    }))
}
