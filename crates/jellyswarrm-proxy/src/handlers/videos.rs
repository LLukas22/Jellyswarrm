use axum::body::Body;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use hyper::StatusCode;
use tracing::{error, info, warn};

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

fn matching_play_session(
    mut candidates: Vec<PlaybackSession>,
    session_id: Option<&str>,
    user_id: Option<&str>,
) -> Result<PlaybackSession, usize> {
    if let Some(session_id) = session_id {
        candidates.retain(|session| session.session_id == session_id);
    }
    if let Some(user_id) = user_id {
        candidates.retain(|session| session.user_id == user_id);
    }

    match candidates.len() {
        1 => Ok(candidates.remove(0)),
        count => Err(count),
    }
}

async fn proxy_request(
    client: &reqwest::Client,
    request: reqwest::Request,
) -> Result<Response, StatusCode> {
    let resp = client.execute(request).await.map_err(|e| {
        error!("Proxy request failed: {}", e);
        StatusCode::BAD_GATEWAY
    })?;

    let status = resp.status();
    let mut headers = resp.headers().clone();

    // Drop hop-by-hop headers; Hyper will manage connection semantics downstream.
    remove_hop_by_hop_headers(&mut headers);

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
            info!("Redirecting {} to: {}", log_label, url);
            Ok(axum::response::Redirect::temporary(url.as_ref()).into_response())
        }
        MediaStreamingMode::Proxy => {
            info!("Proxying {} from: {}", log_label, url);
            proxy_request(&state.streaming_reqwest_client, request).await
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

fn request_user_id(preprocessed: &PreprocessedRequest) -> Option<&str> {
    preprocessed
        .user
        .as_ref()
        .map(|user| user.id.as_str())
        .or_else(|| {
            preprocessed
                .session
                .as_ref()
                .map(|session| session.user_id.as_str())
        })
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

async fn request_for_server(
    state: &AppState,
    preprocessed: PreprocessedRequest,
    server: &Server,
) -> Result<reqwest::Request, StatusCode> {
    if server.id == preprocessed.server.id {
        return Ok(preprocessed.request);
    }

    let session = session_for_server(&preprocessed.sessions, server);
    if preprocessed.auth.is_some() && session.is_none() {
        error!(
            "No backend authorization session for media stream server {}",
            server.name
        );
        return Err(StatusCode::UNAUTHORIZED);
    }
    let auth = remap_authorization(&preprocessed.auth, &session)
        .await
        .map_err(|e| {
            error!("Failed to remap authorization for media stream: {}", e);
            StatusCode::BAD_REQUEST
        })?;
    let mut request = preprocessed.original_request;

    apply_to_request(
        &mut request,
        server,
        &session,
        &auth,
        state,
        preprocessed.access_scope.as_ref(),
    )
    .await;

    Ok(request)
}

async fn forward_preprocessed_resource(
    state: &AppState,
    preprocessed: PreprocessedRequest,
    id: &str,
) -> Result<Response, StatusCode> {
    if !preprocessed.server_matched_request {
        error!("No play session or media mapping for resource {}", id);
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
            "Server {} for media resource {} is not healthy",
            server.name, id
        );
        return Err(StatusCode::NOT_FOUND);
    }

    warn!(
        "No matching play session for resource {}; using media mapping via {}",
        id, server.name
    );
    forward_media_request(state, &server, preprocessed.request, "media resource").await
}

pub async fn get_video_resource(
    State(state): State<AppState>,
    Preprocessed(preprocessed): Preprocessed,
) -> Result<Response, StatusCode> {
    let id = extract_stream_item_id(preprocessed.original_request.url().path())
        .ok_or(StatusCode::NOT_FOUND)?
        .to_string();
    let session_id = extract_play_session_id(preprocessed.original_request.url());
    let candidates = state.play_sessions.get_sessions_by_item_id(&id).await;
    let play_session = match matching_play_session(
        candidates,
        session_id.as_deref(),
        request_user_id(&preprocessed),
    ) {
        Ok(play_session) => play_session,
        Err(_) => return forward_preprocessed_resource(&state, preprocessed, &id).await,
    };

    let server = resolve_play_session_server(&state, &play_session).await?;
    info!(
        "Found play session for resource: {}, session: {}, server: {}",
        id, play_session.session_id, server.name
    );

    let request = request_for_server(&state, preprocessed, &server).await?;
    forward_media_request(&state, &server, request, "media resource").await
}

pub async fn get_stream(
    State(state): State<AppState>,
    Preprocessed(preprocessed): Preprocessed,
) -> Result<Response, StatusCode> {
    let item_id =
        extract_stream_item_id(preprocessed.original_request.url().path()).map(str::to_string);
    let session_id = extract_play_session_id(preprocessed.original_request.url());
    let user_id = request_user_id(&preprocessed).map(str::to_string);
    let play_session = if let Some(item_id) = item_id.as_deref() {
        let candidates = state.play_sessions.get_sessions_by_item_id(item_id).await;
        matching_play_session(
            candidates,
            session_id.as_deref(),
            request_user_id(&preprocessed),
        )
        .ok()
    } else {
        None
    };

    let Some(play_session) = play_session else {
        let server = preprocessed.server.clone();
        let response =
            forward_media_request(&state, &server, preprocessed.request, "media stream").await?;
        if response.status().is_success() || response.status().is_redirection() {
            if let (Some(session_id), Some(item_id), Some(user_id)) = (session_id, item_id, user_id)
            {
                state
                    .play_sessions
                    .add_session(PlaybackSession {
                        session_id,
                        item_id,
                        user_id,
                        server_id: server.id,
                    })
                    .await;
            }
        }
        return Ok(response);
    };
    if play_session.server_id == preprocessed.server.id {
        let server = preprocessed.server;
        return forward_media_request(&state, &server, preprocessed.request, "media stream").await;
    }

    let server = resolve_play_session_server(&state, &play_session).await?;
    let request = request_for_server(&state, preprocessed, &server).await?;
    forward_media_request(&state, &server, request, "media stream").await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server_id::ServerId;

    fn playback_session(session_id: &str, user_id: &str, server_id: i64) -> PlaybackSession {
        PlaybackSession {
            session_id: session_id.to_string(),
            item_id: "item-1".to_string(),
            user_id: user_id.to_string(),
            server_id: ServerId::new(server_id),
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
    fn matching_play_session_accepts_one_candidate() {
        let session = matching_play_session(
            vec![playback_session("session-1", "user-1", 1)],
            None,
            Some("user-1"),
        )
        .unwrap();

        assert_eq!(session.session_id, "session-1");
        assert_eq!(session.server_id, ServerId::new(1));
    }

    #[test]
    fn matching_play_session_filters_by_user() {
        let session = matching_play_session(
            vec![
                playback_session("session-1", "user-1", 1),
                playback_session("session-2", "user-2", 2),
            ],
            None,
            Some("user-2"),
        )
        .unwrap();

        assert_eq!(session.session_id, "session-2");
        assert_eq!(session.server_id, ServerId::new(2));
    }

    #[test]
    fn matching_play_session_rejects_ambiguous_matches() {
        let result = matching_play_session(
            vec![
                playback_session("session-1", "user-1", 1),
                playback_session("session-2", "user-1", 2),
            ],
            None,
            Some("user-1"),
        );

        assert!(matches!(result, Err(2)));
    }

    #[test]
    fn matching_play_session_rejects_missing_user_match() {
        let result = matching_play_session(
            vec![playback_session("session-1", "user-1", 1)],
            None,
            Some("user-2"),
        );

        assert!(matches!(result, Err(0)));
    }

    #[test]
    fn matching_play_session_does_not_replace_unknown_session() {
        let result = matching_play_session(
            vec![playback_session("session-1", "user-1", 1)],
            Some("unknown"),
            Some("user-1"),
        );

        assert!(matches!(result, Err(0)));
    }
}
