use askama::Template;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse},
    Json,
};
use tracing::error;

use crate::{
    statistics_service::{ServerStats, SystemStats, UserStats},
    AppState,
};

#[derive(Template)]
#[template(path = "admin/stats.html")]
pub struct StatsDashboardTemplate {
    pub ui_route: String,
    pub system_stats: SystemStats,
    pub server_stats: Vec<ServerStats>,
    pub top_users: Vec<UserStats>,
}

/// GET /admin/stats - Statistics dashboard
pub async fn get_stats_dashboard(State(state): State<AppState>) -> impl IntoResponse {
    let ui_route = state.get_ui_route().await;

    let system_stats = match state.statistics.get_system_stats().await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to get system stats: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response();
        }
    };

    let server_stats = match state.statistics.get_server_stats().await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to get server stats: {}", e);
            Vec::new()
        }
    };

    let top_users = match state.statistics.get_top_users(10).await {
        Ok(u) => u,
        Err(e) => {
            error!("Failed to get top users: {}", e);
            Vec::new()
        }
    };

    let template = StatsDashboardTemplate {
        ui_route,
        system_stats,
        server_stats,
        top_users,
    };

    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

/// GET /admin/api/stats - JSON statistics
pub async fn get_stats_json(State(state): State<AppState>) -> impl IntoResponse {
    match state.statistics.get_system_stats().await {
        Ok(stats) => Json(stats).into_response(),
        Err(e) => {
            error!("Failed to get system stats: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response()
        }
    }
}

/// GET /admin/api/stats/servers - JSON per-server statistics
pub async fn get_server_stats_json(State(state): State<AppState>) -> impl IntoResponse {
    match state.statistics.get_server_stats().await {
        Ok(stats) => Json(stats).into_response(),
        Err(e) => {
            error!("Failed to get server stats: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response()
        }
    }
}
