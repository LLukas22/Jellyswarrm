use askama::Template;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse},
    Json,
};
use tracing::error;

use crate::{filters, health_monitor::ServerHealthStats, AppState};

#[derive(Template)]
#[template(path = "admin/health.html")]
pub struct HealthDashboardTemplate {
    pub ui_route: String,
    pub stats: Vec<ServerHealthStats>,
}

#[derive(Template)]
#[template(path = "admin/health_cards.html")]
pub struct HealthCardsTemplate {
    pub stats: Vec<ServerHealthStats>,
}

/// GET /admin/health - Server health dashboard
pub async fn get_health_dashboard(State(state): State<AppState>) -> impl IntoResponse {
    let ui_route = state.get_ui_route().await;

    let stats = match state.health_monitor.get_health_stats().await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to get health stats: {}", e);
            Vec::new()
        }
    };

    let template = HealthDashboardTemplate { ui_route, stats };

    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

/// GET /admin/health/cards - Partial update for health cards (HTMX)
pub async fn get_health_cards(State(state): State<AppState>) -> impl IntoResponse {
    let stats = match state.health_monitor.get_health_stats().await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to get health stats: {}", e);
            Vec::new()
        }
    };

    let template = HealthCardsTemplate { stats };

    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

/// GET /admin/api/health - JSON health status
pub async fn get_health_json(State(state): State<AppState>) -> impl IntoResponse {
    match state.health_monitor.get_health_stats().await {
        Ok(stats) => Json(stats).into_response(),
        Err(e) => {
            error!("Failed to get health stats: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response()
        }
    }
}
