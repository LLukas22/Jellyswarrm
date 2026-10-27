//! Serve embedded web assets and the root index, preserving exact `/web` lookup semantics.
use crate::AppState;
use axum::{
    body::Body,
    extract::{Request, State},
    http::StatusCode,
    response::Response,
};
use percent_encoding::percent_decode_str;
use rust_embed::RustEmbed;
use tracing::{debug, error};
#[derive(RustEmbed)]
#[folder = "static/"]
pub(crate) struct Asset;

pub(super) async fn index_handler(
    State(state): State<AppState>,
    _req: Request,
) -> Result<Response<Body>, StatusCode> {
    let servers = state.server_storage.list_servers().await.map_err(|e| {
        error!("Failed to list servers: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    if servers.is_empty() {
        // No servers configured, redirect to UI management
        Ok(Response::builder()
            .status(StatusCode::TEMPORARY_REDIRECT)
            .header("Location", "/ui")
            .body(Body::empty())
            .map_err(|e| {
                error!("Failed to build redirect response: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?)
    } else {
        // Servers exist, return the index.html page
        if let Some(content) = Asset::get("index.html") {
            Ok(Response::builder()
                .header("Content-Type", "text/html")
                .body(Body::from(content.data.into_owned()))
                .map_err(|e| {
                    error!("Failed to build index response: {}", e);
                    StatusCode::INTERNAL_SERVER_ERROR
                })?)
        } else {
            // Fallback if index.html is not found in assets
            error!("index.html not found in static assets");
            Err(StatusCode::NOT_FOUND)
        }
    }
}

/// Map a request path to the embedded asset key. Clients like the official WebOS
/// app load the UI from `<server>/web/index.html`, but the dist is embedded with
/// root-relative keys, so strip the `/web/` prefix (and map bare `/web` to
/// `index.html`). Unprefixed paths pass through unchanged.
fn normalize_asset_lookup_path(path: &str) -> String {
    if path == "web" || path == "web/" {
        // Bare /web (or /web/) serves the UI entry point just like /.
        return "index.html".to_string();
    }

    match path.strip_prefix("web/") {
        Some(rest) => {
            if rest.is_empty() {
                "index.html".to_string()
            } else {
                rest.to_string()
            }
        }
        None => path.to_string(),
    }
}

pub(super) fn serve(req: &Request) -> Option<Result<Response<Body>, StatusCode>> {
    // check if a resource was requested
    let path = req.uri().path();
    debug!("Using generic processing for path: {}", path);
    let path = if let Some(path) = path.strip_prefix('/') {
        path
    } else {
        path
    };
    let path = if path.is_empty() { "index.html" } else { path };
    let decoded_path = normalize_asset_lookup_path(&percent_decode_str(path).decode_utf8_lossy());
    if let Some(content) = Asset::get(&decoded_path) {
        let mime = mime_guess::from_path(&decoded_path).first_or_octet_stream();
        return Some(
            Response::builder()
                .header("Content-Type", mime.as_ref())
                .body(Body::from(content.data.into_owned()))
                .map_err(|e| {
                    error!("Failed to build static asset response: {}", e);
                    StatusCode::INTERNAL_SERVER_ERROR
                }),
        );
    }

    None
}

#[cfg(test)]
mod web_asset_path_tests {
    use super::normalize_asset_lookup_path;

    #[test]
    fn keeps_root_level_paths_unchanged() {
        assert_eq!(normalize_asset_lookup_path("index.html"), "index.html");
        assert_eq!(
            normalize_asset_lookup_path("main.jellyfin.bundle.js"),
            "main.jellyfin.bundle.js"
        );
        assert_eq!(
            normalize_asset_lookup_path("fonts/noto-sans-latin-400-normal.woff2"),
            "fonts/noto-sans-latin-400-normal.woff2"
        );
    }

    #[test]
    fn maps_web_root_to_index_html() {
        assert_eq!(normalize_asset_lookup_path("web"), "index.html");
        assert_eq!(normalize_asset_lookup_path("web/"), "index.html");
    }

    #[test]
    fn strips_web_prefix_for_embedded_assets() {
        assert_eq!(normalize_asset_lookup_path("web/index.html"), "index.html");
        assert_eq!(
            normalize_asset_lookup_path("web/manifest.json"),
            "manifest.json"
        );
        // Query strings never reach this function (path only), but hashed names must
        // survive the strip.
        assert_eq!(
            normalize_asset_lookup_path(
                "web/fonts/noto-sans-latin-400-normal.f5cd7b617bcb047bfaa4.woff2"
            ),
            "fonts/noto-sans-latin-400-normal.f5cd7b617bcb047bfaa4.woff2"
        );
    }

    #[test]
    fn does_not_strip_when_prefix_is_not_exactly_web_slash() {
        // "webfonts" starts with "web" but not with the "web/" directory prefix.
        assert_eq!(
            normalize_asset_lookup_path("webfonts/some-font.ttf"),
            "webfonts/some-font.ttf"
        );
        // Upstream API paths are untouched and keep falling through to proxying.
        assert_eq!(
            normalize_asset_lookup_path("Users/abc/Items"),
            "Users/abc/Items"
        );
    }
}
