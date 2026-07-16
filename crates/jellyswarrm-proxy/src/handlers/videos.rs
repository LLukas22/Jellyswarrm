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
    request_preprocessing::{apply_to_request, remap_authorization},
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
            debug!("Redirecting {} to: {}", log_label, url);
            Ok(axum::response::Redirect::temporary(url.as_ref()).into_response())
        }
        MediaStreamingMode::Proxy => {
            debug!("Proxying {} from: {}", log_label, url);
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
        state
            .play_sessions
            .get_session_by_session_and_item_id(play_session_id, &id)
            .await
            .ok_or_else(|| {
                error!(
                    "No play session found for resource: {} and session: {}",
                    id, play_session_id
                );
                StatusCode::NOT_FOUND
            })?
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
                if state
                    .media_storage
                    .get_media_mapping_with_server(&id)
                    .await
                    .map_err(|e| {
                        error!(
                            "Failed to resolve media mapping for media resource {}: {}",
                            id, e
                        );
                        StatusCode::INTERNAL_SERVER_ERROR
                    })?
                    .is_some()
                {
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
                        "Media resource {} arrived without play session id; routing by virtual media mapping",
                        id
                    );
                    return forward_media_request(
                        &state,
                        &server,
                        preprocessed.request,
                        "media resource",
                    )
                    .await;
                }

                if count == 0 {
                    error!(
                        "No play session found for media resource without session id: {}",
                        id
                    );
                } else {
                    error!(
                        "Ambiguous media resource {} without play session id matched {} active sessions",
                        id, count
                    );
                }
                return Err(StatusCode::NOT_FOUND);
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
