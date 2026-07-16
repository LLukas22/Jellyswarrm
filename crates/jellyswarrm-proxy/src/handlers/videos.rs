use axum::body::Body;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use hyper::StatusCode;
use tracing::{debug, error, info, warn};

use crate::{
    config::MediaStreamingMode,
    extractors::Preprocessed,
    proxy_headers::remove_hop_by_hop_headers,
    request_preprocessing::{apply_to_request, remap_authorization, PreprocessedRequest},
    server_storage::Server,
    session_storage::PlaybackSession,
    user_authorization_service::AuthorizationSession,
    AppState,
};

/// Extract the media item id from `/Videos/{id}/...` or `/Audio/{id}/...` paths.
fn extract_stream_item_id(path: &str) -> Option<&str> {
    let mut segments = path.trim_matches('/').split('/');

    while let Some(segment) = segments.next() {
        if segment.eq_ignore_ascii_case("Videos") || segment.eq_ignore_ascii_case("Audio") {
            return segments.next().filter(|segment| !segment.is_empty());
        }
    }

    None
}

fn extract_play_session_id(url: &url::Url) -> Option<String> {
    url.query_pairs().find_map(|(key, value)| {
        (key.eq_ignore_ascii_case("PlaySessionId") || key.eq_ignore_ascii_case("SessionId"))
            .then(|| value.to_string())
            .filter(|value| !value.is_empty())
    })
}

fn single_matching_play_session(
    mut candidates: Vec<PlaybackSession>,
    user_id: Option<&str>,
) -> Result<PlaybackSession, usize> {
    if let Some(user_id) = user_id {
        candidates.retain(|session| session.user_id == user_id);
    }

    match candidates.len() {
        1 => Ok(candidates.remove(0)),
        count => Err(count),
    }
}

fn looks_like_hls_playlist(url: &url::Url, headers: &reqwest::header::HeaderMap) -> bool {
    let path = url.path().to_ascii_lowercase();
    // /Audio/{id}/universal often returns an HLS master playlist when the client
    // requests TranscodingProtocol=hls — treat that as a playlist too.
    if path.ends_with(".m3u8")
        || path.ends_with(".m3u")
        || path.ends_with("/universal")
        || path.contains("/universal?")
    {
        // Still require playlist-ish content-type for /universal to avoid buffering
        // progressive audio streams into memory.
        if path.ends_with("/universal") || path.contains("/universal?") {
            return headers
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|content_type| {
                    let content_type = content_type.to_ascii_lowercase();
                    content_type.contains("mpegurl")
                        || content_type.contains("m3u8")
                        || content_type.contains("application/vnd.apple")
                        || content_type.starts_with("text/")
                });
        }
        return true;
    }

    headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content_type| {
            let content_type = content_type.to_ascii_lowercase();
            content_type.contains("mpegurl") || content_type.contains("m3u8")
        })
}

fn url_belongs_to_server(candidate: &url::Url, server: &Server) -> bool {
    let server_url = server.url.as_url();
    if candidate.scheme() != server_url.scheme() {
        return false;
    }
    if candidate.host_str() != server_url.host_str() {
        return false;
    }
    if candidate.port_or_known_default() != server_url.port_or_known_default() {
        return false;
    }

    let server_path = server_url.path().trim_end_matches('/');
    if server_path.is_empty() || server_path == "/" {
        return true;
    }

    let candidate_path = candidate.path();
    candidate_path == server_path || candidate_path.starts_with(&format!("{server_path}/"))
}

fn absolute_url_to_proxy_relative(candidate: &url::Url) -> String {
    let mut relative = candidate.path().to_string();
    if let Some(query) = candidate.query() {
        relative.push('?');
        relative.push_str(query);
    }
    if let Some(fragment) = candidate.fragment() {
        relative.push('#');
        relative.push_str(fragment);
    }
    relative
}

/// Rewrite absolute upstream URLs in HLS playlists to root-relative paths so clients
/// keep talking to Jellyswarrm instead of unreachable backend hosts.
/// Only rewrites URLs that clearly belong to the selected upstream server.
fn rewrite_hls_playlist_absolute_urls(body: &str, server: &Server) -> Option<String> {
    let mut changed = false;
    let mut out = String::with_capacity(body.len());
    let ends_with_newline = body.ends_with('\n');

    for (index, line) in body.lines().enumerate() {
        if index > 0 {
            out.push('\n');
        }

        let trimmed = line.trim();
        if let Some(rewritten) = rewrite_hls_line_absolute_urls(trimmed, server) {
            // Preserve original indentation if present.
            let leading_ws_len = line.len() - line.trim_start().len();
            out.push_str(&line[..leading_ws_len]);
            out.push_str(&rewritten);
            changed = true;
        } else {
            out.push_str(line);
        }
    }

    if ends_with_newline {
        out.push('\n');
    }

    changed.then_some(out)
}

fn rewrite_hls_line_absolute_urls(line: &str, server: &Server) -> Option<String> {
    // Full-line absolute URI (variant/segment entry).
    if line.starts_with("http://") || line.starts_with("https://") {
        if let Ok(url) = url::Url::parse(line) {
            if url_belongs_to_server(&url, server) {
                return Some(absolute_url_to_proxy_relative(&url));
            }
        }
        return None;
    }

    // Attribute form: URI="https://upstream/..."
    let Some(uri_pos) = line.find("URI=\"") else {
        return None;
    };
    let value_start = uri_pos + 5;
    let rest = &line[value_start..];
    let Some(value_end) = rest.find('"') else {
        return None;
    };
    let value = &rest[..value_end];
    if !(value.starts_with("http://") || value.starts_with("https://")) {
        return None;
    }
    let Ok(url) = url::Url::parse(value) else {
        return None;
    };
    if !url_belongs_to_server(&url, server) {
        return None;
    }

    let relative = absolute_url_to_proxy_relative(&url);
    let mut rewritten = String::with_capacity(line.len());
    rewritten.push_str(&line[..value_start]);
    rewritten.push_str(&relative);
    rewritten.push_str(&rest[value_end..]);
    Some(rewritten)
}

async fn proxy_request(
    client: &reqwest::Client,
    request: reqwest::Request,
    server: &Server,
) -> Result<Response, StatusCode> {
    let request_url = request.url().clone();
    let resp = client.execute(request).await.map_err(|e| {
        error!("Proxy request failed: {}", e);
        StatusCode::BAD_GATEWAY
    })?;

    let status = resp.status();
    let mut headers = resp.headers().clone();

    // Drop hop-by-hop headers; Hyper will manage connection semantics downstream.
    remove_hop_by_hop_headers(&mut headers);

    // HLS playlists are small; buffer only those so we can rewrite absolute upstream URLs.
    // Everything else streams through chunk-by-chunk with no extra buffering.
    if looks_like_hls_playlist(&request_url, &headers) {
        let bytes = resp.bytes().await.map_err(|e| {
            error!("Failed to read HLS playlist body: {}", e);
            StatusCode::BAD_GATEWAY
        })?;

        let body_bytes = if let Ok(text) = std::str::from_utf8(&bytes) {
            if let Some(rewritten) = rewrite_hls_playlist_absolute_urls(text, server) {
                debug!(
                    "Rewrote absolute upstream URLs in HLS playlist for server {}",
                    server.name
                );
                headers.remove(reqwest::header::CONTENT_LENGTH);
                headers.insert(
                    reqwest::header::CONTENT_LENGTH,
                    reqwest::header::HeaderValue::from_str(&rewritten.len().to_string())
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
                );
                rewritten.into_bytes()
            } else {
                bytes.to_vec()
            }
        } else {
            bytes.to_vec()
        };

        let mut response = Response::builder()
            .status(status)
            .body(Body::from(body_bytes))
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        *response.headers_mut() = headers;
        return Ok(response);
    }

    // Create a stream that yields chunks as they are received from the upstream server
    let stream = resp
        .bytes_stream()
        .map(|result| result.map_err(std::io::Error::other));

    let body = Body::from_stream(stream);

    let mut response = Response::builder()
        .status(status)
        .body(body)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    *response.headers_mut() = headers;

    Ok(response)
}

async fn forward_media_request(
    state: &AppState,
    server: &Server,
    request: reqwest::Request,
    log_label: &str,
) -> Result<Response, StatusCode> {
    let url = request.url().clone();

    match server.media_streaming_mode {
        MediaStreamingMode::Redirect => {
            debug!("Redirecting {} to: {}", log_label, url);
            Ok(axum::response::Redirect::temporary(url.as_ref()).into_response())
        }
        MediaStreamingMode::Proxy => {
            debug!("Proxying {} from: {}", log_label, url);
            proxy_request(&state.streaming_reqwest_client, request, server).await
        }
    }
}

fn session_for_server(
    sessions: &Option<Vec<(AuthorizationSession, Server)>>,
    server: &Server,
) -> Option<AuthorizationSession> {
    sessions.as_ref().and_then(|sessions| {
        sessions
            .iter()
            .find(|(_, session_server)| session_server.id == server.id)
            .map(|(session, _)| session.clone())
    })
}

async fn resolve_server_for_stream_item(
    state: &AppState,
    item_id: &str,
    play_session_id: Option<&str>,
    user_id: Option<&str>,
    fallback_server: &Server,
) -> Result<Server, StatusCode> {
    if let Some(play_session_id) = play_session_id {
        if let Some(play_session) = state
            .play_sessions
            .get_session_by_session_and_item_id(play_session_id, item_id)
            .await
        {
            return resolve_play_session_server(state, &play_session).await;
        }
    }

    let candidates = state.play_sessions.get_sessions_by_item_id(item_id).await;
    if let Ok(play_session) = single_matching_play_session(candidates, user_id) {
        return resolve_play_session_server(state, &play_session).await;
    }

    // Prefer the virtual media mapping when available; preprocessing already selected a
    // server from the path/query id, so fall back to that.
    if state
        .media_storage
        .get_media_mapping_with_server(item_id)
        .await
        .map_err(|e| {
            error!(
                "Failed to resolve media mapping for stream item {}: {}",
                item_id, e
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .is_some()
    {
        return Ok(fallback_server.clone());
    }

    Ok(fallback_server.clone())
}

async fn resolve_play_session_server(
    state: &AppState,
    play_session: &PlaybackSession,
) -> Result<Server, StatusCode> {
    let server = match state
        .server_storage
        .get_server_by_id(play_session.server_id)
        .await
        .map_err(|e| {
            error!(
                "Failed to resolve server {} for play session {}: {}",
                play_session.server_id, play_session.session_id, e
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })? {
        Some(server) => server,
        None => {
            state
                .play_sessions
                .remove_sessions_for_server(play_session.server_id)
                .await;
            error!(
                "Server {} for play session {} no longer exists",
                play_session.server_id, play_session.session_id
            );
            return Err(StatusCode::NOT_FOUND);
        }
    };

    if !state
        .server_storage
        .server_status(server.id)
        .await
        .is_healthy()
    {
        error!(
            "Server {} for play session {} is not healthy",
            server.name, play_session.session_id
        );
        return Err(StatusCode::NOT_FOUND);
    }

    Ok(server)
}

async fn forward_resource_by_media_mapping(
    state: &AppState,
    preprocessed: PreprocessedRequest,
    id: &str,
    reason: &str,
) -> Result<Response, StatusCode> {
    if state
        .media_storage
        .get_media_mapping_with_server(id)
        .await
        .map_err(|e| {
            error!(
                "Failed to resolve media mapping for media resource {}: {}",
                id, e
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .is_none()
    {
        error!(
            "No play session or media mapping for media resource {} ({})",
            id, reason
        );
        return Err(StatusCode::NOT_FOUND);
    }

    let server = preprocessed.server;
    if !state
        .server_storage
        .server_status(server.id)
        .await
        .is_healthy()
    {
        error!(
            "Server {} for media resource {} is not available",
            server.name, id
        );
        return Err(StatusCode::NOT_FOUND);
    }

    warn!(
        "Media resource {} {}; routing by virtual media mapping via {}",
        id, reason, server.name
    );
    forward_media_request(state, &server, preprocessed.request, "media resource").await
}

//http://localhost:3000/Videos/82fe5aab-29ff-9630-05c2-da1a5a640428/82fe5aab29ff963005c2da1a5a640428/Attachments/5
//http://localhost:3000/Videos/71bda5a4-267a-1a6c-49ce-8536d36628d8/71bda5a4267a1a6c49ce8536d36628d8/Subtitles/3/0/Stream.js?api_key=...
//http://localhost:3000/Audio/{id}/master.m3u8?MediaSourceId=...&PlaySessionId=...
//http://localhost:3000/Audio/{id}/hls1/main/0.ts?...
pub async fn get_video_resource(
    State(state): State<AppState>,
    Preprocessed(preprocessed): Preprocessed,
) -> Result<Response, StatusCode> {
    let original_request = &preprocessed.original_request;

    let id = extract_stream_item_id(original_request.url().path())
        .ok_or(StatusCode::NOT_FOUND)?
        .to_string();
    let play_session_id = extract_play_session_id(original_request.url());
    let play_session = if let Some(play_session_id) = play_session_id.as_deref() {
        // Audio /universal often uses a client-generated PlaySessionId we never track
        // (no prior PlaybackInfo). Fall back to media mapping instead of hard 404.
        match state
            .play_sessions
            .get_session_by_session_and_item_id(play_session_id, &id)
            .await
        {
            Some(session) => session,
            None => {
                return forward_resource_by_media_mapping(
                    &state,
                    preprocessed,
                    &id,
                    &format!("unknown play session id {play_session_id}"),
                )
                .await;
            }
        }
    } else {
        let candidates = state.play_sessions.get_sessions_by_item_id(&id).await;
        let user_id = preprocessed
            .user
            .as_ref()
            .map(|user| user.id.as_str())
            .or_else(|| {
                preprocessed
                    .session
                    .as_ref()
                    .map(|session| session.user_id.as_str())
            });

        match single_matching_play_session(candidates, user_id) {
            Ok(play_session) => {
                warn!(
                    "Media resource {} arrived without play session id; using only matching active session {}",
                    id, play_session.session_id
                );
                play_session
            }
            Err(count) => {
                return forward_resource_by_media_mapping(
                    &state,
                    preprocessed,
                    &id,
                    &format!("no unique play session (matched {count})"),
                )
                .await;
            }
        }
    };

    let original_request = preprocessed.original_request;

    let server = resolve_play_session_server(&state, &play_session).await?;

    info!(
        "Found play session for resource: {}, session: {}, server: {}",
        id, play_session.session_id, server.name
    );

    let mut upstream_request = original_request;
    let session = session_for_server(&preprocessed.sessions, &server).or_else(|| {
        (preprocessed.server.id == server.id)
            .then(|| preprocessed.session.clone())
            .flatten()
    });
    let new_auth = remap_authorization(&preprocessed.auth, &session)
        .await
        .map_err(|e| {
            error!("Failed to remap authorization for resource request: {}", e);
            StatusCode::BAD_REQUEST
        })?;

    apply_to_request(&mut upstream_request, &server, &session, &new_auth, &state).await;

    forward_media_request(&state, &server, upstream_request, "media resource").await
}

/// Progressive and universal media streams under `/Videos/{id}/stream*` and
/// `/Audio/{id}/stream*|/universal`.
///
/// Prefer play-session / media-mapping routing so multi-server setups send audio and video
/// to the correct upstream even when preprocessing falls back to the "best" server.
pub async fn get_stream(
    State(state): State<AppState>,
    Preprocessed(preprocessed): Preprocessed,
) -> Result<Response, StatusCode> {
    let original_url = preprocessed.original_request.url().clone();
    let item_id = extract_stream_item_id(original_url.path()).map(str::to_string);
    let play_session_id = extract_play_session_id(&original_url);
    let user_id = preprocessed
        .user
        .as_ref()
        .map(|user| user.id.as_str())
        .or_else(|| {
            preprocessed
                .session
                .as_ref()
                .map(|session| session.user_id.as_str())
        });

    let server = if let Some(item_id) = item_id.as_deref() {
        resolve_server_for_stream_item(
            &state,
            item_id,
            play_session_id.as_deref(),
            user_id,
            &preprocessed.server,
        )
        .await?
    } else {
        preprocessed.server.clone()
    };

    if !state
        .server_storage
        .server_status(server.id)
        .await
        .is_healthy()
    {
        error!("Server {} for media stream is not healthy", server.name);
        return Err(StatusCode::NOT_FOUND);
    }

    // If play-session routing selected a different server than preprocessing, rebuild the
    // upstream request against that server with the matching session credentials.
    let request = if server.id != preprocessed.server.id {
        let mut upstream_request = preprocessed.original_request;
        let session = session_for_server(&preprocessed.sessions, &server).or_else(|| {
            (preprocessed.server.id == server.id)
                .then(|| preprocessed.session.clone())
                .flatten()
        });
        let new_auth = remap_authorization(&preprocessed.auth, &session)
            .await
            .map_err(|e| {
                error!("Failed to remap authorization for media stream: {}", e);
                StatusCode::BAD_REQUEST
            })?;
        apply_to_request(&mut upstream_request, &server, &session, &new_auth, &state).await;
        upstream_request
    } else {
        preprocessed.request
    };

    forward_media_request(&state, &server, request, "media stream").await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::MediaStreamingMode,
        server_id::ServerId,
        server_url::ServerUrl,
    };

    fn playback_session(session_id: &str, user_id: &str, server_id: i64) -> PlaybackSession {
        PlaybackSession {
            session_id: session_id.to_string(),
            item_id: "item-1".to_string(),
            user_id: user_id.to_string(),
            server_id: ServerId::new(server_id),
        }
    }

    fn test_server(url: &str) -> Server {
        let now = chrono::Utc::now();
        Server {
            id: ServerId::new(1),
            name: "Test".to_string(),
            url: ServerUrl::parse(url).unwrap(),
            priority: 100,
            media_streaming_mode: MediaStreamingMode::Proxy,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn extract_stream_item_id_from_videos_and_audio() {
        assert_eq!(
            extract_stream_item_id("/Videos/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee/master.m3u8"),
            Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
        );
        assert_eq!(
            extract_stream_item_id("/Audio/11111111111111111111111111111111/universal"),
            Some("11111111111111111111111111111111")
        );
        assert_eq!(
            extract_stream_item_id("/Audio/11111111111111111111111111111111/hls1/main/0.ts"),
            Some("11111111111111111111111111111111")
        );
        assert_eq!(extract_stream_item_id("/Items/abc/PlaybackInfo"), None);
    }

    #[test]
    fn rewrite_hls_playlist_rewrites_same_server_absolute_urls_only() {
        let server = test_server("http://jellyfin.local:8096/jellyfin");
        let playlist = "\
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-KEY:METHOD=AES-128,URI=\"http://jellyfin.local:8096/jellyfin/Keys/1\"
http://jellyfin.local:8096/jellyfin/Audio/item/main.m3u8?MediaSourceId=abc
http://other-host.example/Audio/item/main.m3u8
main.m3u8?MediaSourceId=local
";

        let rewritten = rewrite_hls_playlist_absolute_urls(playlist, &server).unwrap();
        assert!(rewritten.contains(
            "#EXT-X-KEY:METHOD=AES-128,URI=\"/jellyfin/Keys/1\""
        ));
        assert!(rewritten.contains("/jellyfin/Audio/item/main.m3u8?MediaSourceId=abc"));
        assert!(rewritten.contains("http://other-host.example/Audio/item/main.m3u8"));
        assert!(rewritten.contains("main.m3u8?MediaSourceId=local"));
        assert!(!rewritten.contains("http://jellyfin.local:8096/jellyfin/Audio"));
    }

    #[test]
    fn rewrite_hls_playlist_returns_none_when_unchanged() {
        let server = test_server("http://jellyfin.local:8096");
        let playlist = "#EXTM3U\nmain.m3u8?x=1\n";
        assert!(rewrite_hls_playlist_absolute_urls(playlist, &server).is_none());
    }

    #[test]
    fn single_matching_play_session_accepts_one_candidate() {
        let session = single_matching_play_session(
            vec![playback_session("session-1", "user-1", 1)],
            Some("user-1"),
        )
        .unwrap();

        assert_eq!(session.session_id, "session-1");
        assert_eq!(session.server_id, ServerId::new(1));
    }

    #[test]
    fn single_matching_play_session_filters_by_user() {
        let session = single_matching_play_session(
            vec![
                playback_session("session-1", "user-1", 1),
                playback_session("session-2", "user-2", 2),
            ],
            Some("user-2"),
        )
        .unwrap();

        assert_eq!(session.session_id, "session-2");
        assert_eq!(session.server_id, ServerId::new(2));
    }

    #[test]
    fn single_matching_play_session_rejects_ambiguous_matches() {
        let result = single_matching_play_session(
            vec![
                playback_session("session-1", "user-1", 1),
                playback_session("session-2", "user-1", 2),
            ],
            Some("user-1"),
        );

        assert!(matches!(result, Err(2)));
    }

    #[test]
    fn single_matching_play_session_rejects_missing_user_match() {
        let result = single_matching_play_session(
            vec![playback_session("session-1", "user-1", 1)],
            Some("user-2"),
        );

        assert!(matches!(result, Err(0)));
    }
}
