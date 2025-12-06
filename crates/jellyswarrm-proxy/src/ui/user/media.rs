use askama::Template;
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
};
use jellyfin_api::models::BaseItem;
use tracing::error;

use crate::{
    ui::{auth::AuthenticatedUser, user::common::authenticate_user_on_server},
    AppState,
};

pub struct ServerInfo {
    pub id: i64,
    pub name: String,
}

pub struct LibraryWithCount {
    pub id: String,
    pub name: String,
    pub count: i32,
}

#[derive(Template)]
#[template(path = "user/user_media.html")]
pub struct UserMediaTemplate {
    pub servers: Vec<ServerInfo>,
    pub ui_route: String,
}

#[derive(Template)]
#[template(path = "user/server_libraries.html")]
pub struct ServerLibrariesTemplate {
    pub server_id: i64,
    pub libraries: Vec<LibraryWithCount>,
    pub ui_route: String,
}

#[derive(Template)]
#[template(path = "user/library_items.html")]
pub struct LibraryItemsTemplate {
    pub server_id: i64,
    pub library_id: String,
    pub items: Vec<BaseItem>,
    pub next_page: Option<i32>,
    pub ui_route: String,
}

#[derive(serde::Deserialize)]
pub struct Pagination {
    pub page: Option<i32>,
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

    let server_infos = servers
        .into_iter()
        .map(|s| ServerInfo {
            id: s.id,
            name: s.name,
        })
        .collect();

    let template = UserMediaTemplate {
        servers: server_infos,
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

pub async fn get_server_libraries(
    State(state): State<AppState>,
    AuthenticatedUser(user): AuthenticatedUser,
    Path(server_id): Path<i64>,
) -> impl IntoResponse {
    let server = match state.server_storage.get_server_by_id(server_id).await {
        Ok(Some(s)) => s,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    if let Ok((client, jellyfin_user, _)) =
        authenticate_user_on_server(&state, &user, &server).await
    {
        match client.get_media_folders(Some(&jellyfin_user.id)).await {
            Ok(folders) => {
                let mut libraries = Vec::new();
                for folder in folders {
                    if folder.collection_type.as_deref() == Some("playlists")
                        || folder.name.to_lowercase() == "playlists"
                    {
                        continue;
                    }

                    let count = match client
                        .get_items(
                            &jellyfin_user.id,
                            Some(&folder.id),
                            true,
                            Some(vec!["Movie".to_string(), "Series".to_string()]),
                            Some(0),
                            None,
                            None,
                            None,
                        )
                        .await
                    {
                        Ok(resp) => resp.total_record_count,
                        Err(_) => 0,
                    };

                    libraries.push(LibraryWithCount {
                        id: folder.id,
                        name: folder.name,
                        count,
                    });
                }

                let template = ServerLibrariesTemplate {
                    server_id,
                    libraries,
                    ui_route: state.get_ui_route().await,
                };
                match template.render() {
                    Ok(html) => Html(html).into_response(),
                    Err(e) => {
                        error!("Failed to render server libraries template: {}", e);
                        (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
                    }
                }
            }
            Err(e) => {
                error!("Failed to get media folders: {}", e);
                (StatusCode::BAD_GATEWAY, "Failed to fetch libraries").into_response()
            }
        }
    } else {
        (StatusCode::UNAUTHORIZED, "Failed to authenticate").into_response()
    }
}

pub async fn get_library_items(
    State(state): State<AppState>,
    AuthenticatedUser(user): AuthenticatedUser,
    Path((server_id, library_id)): Path<(i64, String)>,
    Query(pagination): Query<Pagination>,
) -> impl IntoResponse {
    let server = match state.server_storage.get_server_by_id(server_id).await {
        Ok(Some(s)) => s,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let page = pagination.page.unwrap_or(0);
    let limit = 20;
    let start_index = page * limit;

    if let Ok((client, jellyfin_user, _)) =
        authenticate_user_on_server(&state, &user, &server).await
    {
        match client
            .get_items(
                &jellyfin_user.id,
                Some(&library_id),
                true,
                Some(vec!["Movie".to_string(), "Series".to_string()]),
                Some(limit),
                Some(start_index),
                Some("DateCreated".to_string()),
                Some("Descending".to_string()),
            )
            .await
        {
            Ok(response) => {
                let total_items = response.total_record_count;
                let total_pages = (total_items as f64 / limit as f64).ceil() as i32;

                let next_page = if (page + 1) < total_pages {
                    Some(page + 1)
                } else {
                    None
                };

                let template = LibraryItemsTemplate {
                    server_id,
                    library_id,
                    items: response.items,
                    next_page,
                    ui_route: state.get_ui_route().await,
                };
                match template.render() {
                    Ok(html) => Html(html).into_response(),
                    Err(e) => {
                        error!("Failed to render library items template: {}", e);
                        (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
                    }
                }
            }
            Err(e) => {
                error!("Failed to get items: {}", e);
                (StatusCode::BAD_GATEWAY, "Failed to fetch items").into_response()
            }
        }
    } else {
        (StatusCode::UNAUTHORIZED, "Failed to authenticate").into_response()
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
