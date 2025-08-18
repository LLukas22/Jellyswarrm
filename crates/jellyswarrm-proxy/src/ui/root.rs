use askama::Template;
use axum::{
    http::StatusCode,
    response::{Html, IntoResponse},
};
use tracing::error;

#[derive(Template)]
#[template(path = "index.html")]
pub struct IndexTemplate {
    pub version: Option<String>,
}

/// Root/home page
pub async fn index() -> impl IntoResponse {
    let template = IndexTemplate {
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
    };

    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render index template: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}
