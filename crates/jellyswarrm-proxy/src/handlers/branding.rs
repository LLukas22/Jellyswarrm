use axum::{extract::State, Json};
use hyper::StatusCode;
use jellyfin_api::JellyfinClient;

use crate::{
    models::BrandingConfig,
    server_storage::{Server, ServerStorageService},
    AppState,
};

async fn fetch_custom_css(server_storage: &ServerStorageService, servers: &[Server]) -> String {
    for server in servers {
        if !server_storage.server_status(server.id).await.is_healthy() {
            continue;
        }

        let Ok(client) = JellyfinClient::new_with_client(
            server.url.as_ref(),
            server_storage.client_info.clone(),
            server_storage.http_client.clone(),
        ) else {
            continue;
        };
        let Ok(branding) = client.get_branding_configuration().await else {
            continue;
        };

        return branding.custom_css.unwrap_or_default();
    }

    String::new()
}

pub async fn handle_branding(
    State(state): State<AppState>,
) -> Result<Json<BrandingConfig>, StatusCode> {
    let servers = state
        .server_storage
        .list_servers()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut message = "Jellyswarrm proxying to the following servers: ".to_string();
    let custom_css = if !servers.is_empty() {
        let server_links: Vec<String> = servers
            .iter()
            .map(|s| {
                format!(
                    "<a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">{}</a>",
                    s.url, s.name
                )
            })
            .collect();
        message.push_str(&server_links.join(", "));

        fetch_custom_css(state.server_storage.as_ref(), &servers).await
    } else {
        message.push_str("No servers configured.");
        String::new()
    };

    let config = BrandingConfig {
        login_disclaimer: message,
        custom_css,
        splashscreen_enabled: false,
    };
    Ok(Json(config))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::SqlitePool;
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;
    use crate::config::{MediaStreamingMode, MIGRATOR};

    async fn test_storage() -> ServerStorageService {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        ServerStorageService::new(pool)
    }

    async fn mount_upstream(server: &MockServer, branding_response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path("/System/Info/Public"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ServerName": "Upstream"
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/Branding/Configuration"))
            .respond_with(branding_response)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn fetch_custom_css_uses_highest_priority_healthy_server() {
        let high_priority = MockServer::start().await;
        mount_upstream(
            &high_priority,
            ResponseTemplate::new(200).set_body_json(json!({
                "CustomCss": "body { color: high-priority; }"
            })),
        )
        .await;
        let low_priority = MockServer::start().await;
        mount_upstream(
            &low_priority,
            ResponseTemplate::new(200).set_body_json(json!({
                "CustomCss": "body { color: low-priority; }"
            })),
        )
        .await;

        let storage = test_storage().await;
        storage
            .add_server(
                "Low priority",
                &low_priority.uri(),
                100,
                MediaStreamingMode::Redirect,
            )
            .await
            .unwrap();
        storage
            .add_server(
                "High priority",
                &high_priority.uri(),
                200,
                MediaStreamingMode::Redirect,
            )
            .await
            .unwrap();
        storage.check_servers_health().await;

        let servers = storage.list_servers().await.unwrap();
        let custom_css = fetch_custom_css(&storage, &servers).await;

        assert_eq!(custom_css, "body { color: high-priority; }");
    }

    #[tokio::test]
    async fn fetch_custom_css_falls_back_when_higher_priority_branding_fails() {
        let high_priority = MockServer::start().await;
        mount_upstream(&high_priority, ResponseTemplate::new(500)).await;
        let low_priority = MockServer::start().await;
        mount_upstream(
            &low_priority,
            ResponseTemplate::new(200).set_body_json(json!({
                "CustomCss": "body { color: fallback; }"
            })),
        )
        .await;

        let storage = test_storage().await;
        storage
            .add_server(
                "Low priority",
                &low_priority.uri(),
                100,
                MediaStreamingMode::Redirect,
            )
            .await
            .unwrap();
        storage
            .add_server(
                "High priority",
                &high_priority.uri(),
                200,
                MediaStreamingMode::Redirect,
            )
            .await
            .unwrap();
        storage.check_servers_health().await;

        let servers = storage.list_servers().await.unwrap();
        let custom_css = fetch_custom_css(&storage, &servers).await;

        assert_eq!(custom_css, "body { color: fallback; }");
    }
}
