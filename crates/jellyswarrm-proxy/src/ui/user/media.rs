use askama::Template;
use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
};
use jellyfin_api::models::BaseItem;
use tracing::error;

use crate::{
    ui::{auth::AuthenticatedUser, user::common::authenticate_user_on_server},
    AppState,
};

pub struct ServerItems {
    pub server_name: String,
    pub server_id: i64,
    pub items: Vec<BaseItem>,
    pub error: Option<String>,
}

#[derive(Template)]
#[template(path = "user/user_media.html")]
pub struct UserMediaTemplate {
    pub servers: Vec<ServerItems>,
    pub ui_route: String,
}

pub async fn get_user_media(
    State(state): State<AppState>,
    AuthenticatedUser(user): AuthenticatedUser,
) -> impl IntoResponse {
    let servers = match state.user_authorization.get_mapped_servers(&user.id).await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to list mapped servers: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response();
        }
    };

    let mut server_items: Vec<ServerItems> = Vec::new();

    for server in servers {
        let mut items = Vec::new();
        let mut error_msg = None;

        // Authenticate user on the server
        if let Ok((client, jellyfin_user, _)) =
            authenticate_user_on_server(&state, &user, &server).await
        {
            match client
                .get_items(
                    &jellyfin_user.id,
                    None,
                    true,
                    Some(vec!["Movie".to_string(), "Series".to_string()]),
                    Some(20),
                    Some("DateCreated".to_string()),
                    Some("Descending".to_string()),
                )
                .await
            {
                Ok(response) => {
                    items = response.items;
                    error_msg = None;
                }
                Err(jellyfin_api::error::Error::Unauthorized) => {
                    error!(
                        "Failed to get media items from server {}: Unauthorized after retry",
                        server.name
                    );
                    error_msg = Some("Unauthorized".to_string());
                }
                Err(e) => {
                    error!(
                        "Failed to get media items from server {}: {}",
                        server.name, e
                    );
                    error_msg = Some(format!("Error fetching media: {}", e));
                }
            }
        } else {
            error_msg = Some("Failed to authenticate on server".to_string());
        }

        server_items.push(ServerItems {
            server_name: server.name,
            server_id: server.id,
            items,
            error: error_msg,
        });
    }

    let template = UserMediaTemplate {
        servers: server_items,
        ui_route: state.get_ui_route().await,
    };
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render user media template: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

pub async fn proxy_media_image(
    State(state): State<AppState>,
    AuthenticatedUser(user): AuthenticatedUser,
    Path((server_id, item_id)): Path<(i64, String)>,
) -> impl IntoResponse {
    // Get server
    let server = match state.server_storage.get_server_by_id(server_id).await {
        Ok(Some(s)) => s,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    // Authenticate
    let (client, _, _) = match authenticate_user_on_server(&state, &user, &server).await {
        Ok(res) => res,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };

    // Construct image URL
    // We need to access the base_url from client, but it's private.
    // However, we have server.url.
    let image_url = format!(
        "{}/Items/{}/Images/Primary",
        server.url.as_str().trim_end_matches('/'),
        item_id
    );

    // Fetch image using the client's internal http client would be best, but we can't access it.
    // We can use state.reqwest_client but we need the token.
    let token = client.get_token().unwrap_or_default();

    // Build auth header manually since we are using a raw request
    // Or we can add a method to JellyfinClient to fetch raw resource.
    // For now, let's use state.reqwest_client

    let auth_header = format!(
        "MediaBrowser Client=\"Jellyswarrm Proxy\", Device=\"Server\", DeviceId=\"jellyswarrm-proxy\", Version=\"{}\", Token=\"{}\"",
        env!("CARGO_PKG_VERSION"),
        token
    );

    match state
        .reqwest_client
        .get(&image_url)
        .header(header::AUTHORIZATION, auth_header)
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            let headers = resp.headers().clone();
            let body = resp.bytes().await.unwrap_or_default();

            let mut response = Response::builder().status(status);
            if let Some(ct) = headers.get(header::CONTENT_TYPE) {
                response = response.header(header::CONTENT_TYPE, ct);
            }
            // Cache control
            response = response.header(header::CACHE_CONTROL, "public, max-age=3600");

            response
                .body(Body::from(body))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(_) => StatusCode::BAD_GATEWAY.into_response(),
    }
}
