//! Only playlists are buffered; media segments retain the streaming byte path.
use super::*;
use std::{sync::LazyLock, time::Duration};

const MAX_PLAYLIST_BYTES: usize = 8 * 1024 * 1024;
// Consume every quoted attribute, so a comma/URI= inside another quoted value
// cannot be mistaken for a new attribute.
static QUOTED_ATTRIBUTE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"(?:^|[:,])\s*([A-Z0-9-]+)="([^"]*)""#).unwrap());

pub(super) fn is_playlist_path(url: &url::Url) -> bool {
    url.path().to_ascii_lowercase().ends_with(".m3u8")
}

pub(super) fn is_playlist_type(headers: &hyper::HeaderMap) -> bool {
    headers
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            matches!(
                value
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase()
                    .as_str(),
                "application/vnd.apple.mpegurl"
                    | "application/x-mpegurl"
                    | "audio/mpegurl"
                    | "audio/x-mpegurl"
            )
        })
}

pub(super) async fn response(
    state: &AppState,
    server: &Server,
    upstream: reqwest::Response,
    proxy_api_key: Option<&str>,
    head: bool,
) -> Result<Response, StatusCode> {
    // Do not leak partial, cached, compressed, or error-page playlist bodies.
    if upstream.status() != StatusCode::OK {
        return Err(
            if upstream.status().is_success() || upstream.status().is_redirection() {
                StatusCode::BAD_GATEWAY
            } else {
                upstream.status()
            },
        );
    }
    if upstream
        .headers()
        .get(hyper::header::CONTENT_ENCODING)
        .is_some_and(|value| value != "identity")
    {
        return Err(StatusCode::BAD_GATEWAY);
    }
    let playlist = upstream.url().clone();
    let mut headers = upstream.headers().clone();
    remove_hop_by_hop_headers(&mut headers);
    for header in [
        "content-length",
        "content-encoding",
        "content-range",
        "accept-ranges",
        "etag",
        "last-modified",
        "content-md5",
        "digest",
    ] {
        headers.remove(header);
    }
    headers.insert(
        hyper::header::CACHE_CONTROL,
        hyper::header::HeaderValue::from_static("private, no-store"),
    );
    let body = if head {
        Body::empty()
    } else {
        let timeout = Duration::from_secs(state.config.read().await.timeout.clamp(1, 300));
        let text = tokio::time::timeout(timeout, async {
            let mut bytes = Vec::new();
            let mut stream = upstream.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| StatusCode::BAD_GATEWAY)?;
                if chunk.len() > MAX_PLAYLIST_BYTES.saturating_sub(bytes.len()) {
                    return Err(StatusCode::BAD_GATEWAY);
                }
                bytes.extend_from_slice(&chunk);
            }
            String::from_utf8(bytes).map_err(|_| StatusCode::BAD_GATEWAY)
        })
        .await
        .map_err(|_| StatusCode::GATEWAY_TIMEOUT)??;
        Body::from(rewrite(state, server, &playlist, &text, proxy_api_key).await?)
    };
    let mut response = Response::new(body);
    *response.headers_mut() = headers;
    Ok(response)
}

async fn rewrite(
    state: &AppState,
    server: &Server,
    playlist: &url::Url,
    text: &str,
    proxy_api_key: Option<&str>,
) -> Result<String, StatusCode> {
    if !text.trim_start_matches('\u{feff}').starts_with("#EXTM3U") {
        return Err(StatusCode::BAD_GATEWAY);
    }
    let viewer = match proxy_api_key {
        Some(key) => state
            .user_authorization
            .get_user_by_token(key)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .map(|user| user.id),
        None => None,
    };
    let mut output = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        let mut end = 0;
        let ranges = if trimmed.starts_with("#EXT") {
            QUOTED_ATTRIBUTE
                .captures_iter(line)
                .filter(|capture| &capture[1] == "URI")
                .map(|capture| {
                    let uri = capture.get(2).unwrap();
                    uri.start()..uri.end()
                })
                .collect::<Vec<_>>()
        } else if !trimmed.is_empty() && !trimmed.starts_with('#') {
            let start = line.len() - line.trim_start().len();
            std::iter::once(start..start + trimmed.len()).collect()
        } else {
            Vec::new()
        };
        for range in ranges {
            output.push_str(&line[end..range.start]);
            output.push_str(
                &state
                    .processors
                    .url_processor
                    .server_to_client_hls_url(
                        &line[range.clone()],
                        playlist,
                        server,
                        proxy_api_key,
                        viewer.as_deref(),
                    )
                    .await
                    .map_err(|_| StatusCode::BAD_GATEWAY)?,
            );
            end = range.end;
        }
        output.push_str(&line[end..]);
    }
    Ok(output)
}
