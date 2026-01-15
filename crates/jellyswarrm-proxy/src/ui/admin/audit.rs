//! Audit log UI handlers

use askama::Template;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse},
};
use serde::Deserialize;
use tracing::error;

use crate::{
    audit_service::{AuditLogEntry, AuditLogFilter},
    AppState,
};

#[derive(Template)]
#[template(path = "admin/audit.html")]
pub struct AuditPageTemplate {
    pub ui_route: String,
}

#[derive(Template)]
#[template(path = "admin/audit_list.html")]
pub struct AuditListTemplate {
    pub logs: Vec<AuditLogEntry>,
    pub ui_route: String,
    pub page: i64,
    pub total_pages: i64,
    pub filter: AuditFilterParams,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AuditFilterParams {
    #[serde(default)]
    pub actor_type: Option<String>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub resource_type: Option<String>,
    #[serde(default = "default_page")]
    pub page: i64,
}

fn default_page() -> i64 {
    1
}

const PAGE_SIZE: i64 = 50;

/// Main audit log page
pub async fn audit_page(State(state): State<AppState>) -> impl IntoResponse {
    let template = AuditPageTemplate {
        ui_route: state.get_ui_route().await,
    };

    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render audit template: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

/// Get audit log list (with pagination and filtering)
pub async fn get_audit_list(
    State(state): State<AppState>,
    Query(params): Query<AuditFilterParams>,
) -> impl IntoResponse {
    let page = params.page.max(1);
    let offset = (page - 1) * PAGE_SIZE;

    let filter = AuditLogFilter {
        actor_type: params.actor_type.clone().filter(|s| !s.is_empty()),
        action: params.action.clone().filter(|s| !s.is_empty()),
        resource_type: params.resource_type.clone().filter(|s| !s.is_empty()),
        limit: Some(PAGE_SIZE),
        offset: Some(offset),
        ..Default::default()
    };

    let logs = match state.audit_service.query(filter).await {
        Ok(logs) => logs,
        Err(e) => {
            error!("Failed to query audit logs: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response();
        }
    };

    let total_count = match state.audit_service.count().await {
        Ok(count) => count,
        Err(e) => {
            error!("Failed to count audit logs: {}", e);
            0
        }
    };

    let total_pages = (total_count as f64 / PAGE_SIZE as f64).ceil() as i64;

    let template = AuditListTemplate {
        logs,
        ui_route: state.get_ui_route().await,
        page,
        total_pages: total_pages.max(1),
        filter: params,
    };

    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render audit list: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}
