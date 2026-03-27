use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use hyper::StatusCode;
use tracing::{debug, error, info};

use crate::{
    config::MediaStreamingMode,
    request_preprocessing::{apply_to_request, preprocess_request, remap_authorization},
    server_storage::Server,
    user_authorization_service::AuthorizationSession,
    AppState,
};

fn strip_hop_by_hop_headers(headers: &mut HeaderMap) {
    headers.remove(hyper::header::CONNECTION);
    headers.remove(HeaderName::from_static("keep-alive"));
    headers.remove(hyper::header::PROXY_AUTHENTICATE);
    headers.remove(hyper::header::PROXY_AUTHORIZATION);
    headers.remove(hyper::header::TE);
    headers.remove(hyper::header::TRAILER);
    headers.remove(hyper::header::TRANSFER_ENCODING);
    headers.remove(hyper::header::UPGRADE);
}

fn extract_video_id(path: &str) -> Option<&str> {
    let mut segments = path.trim_matches('/').split('/');

    while let Some(segment) = segments.next() {
        if segment.eq_ignore_ascii_case("Videos") {
            return segments.next().filter(|segment| !segment.is_empty());
        }
    }

    None
}

async fn proxy_request(
    client: &reqwest::Client,
    request: reqwest::Request,
) -> Result<Response, StatusCode> {
    debug!(
        "Video proxy request: method={} url={} range={:?}",
        request.method(),
        request.url(),
        request.headers().get(hyper::header::RANGE)
    );

    let resp = client.execute(request).await.map_err(|e| {
        error!("Proxy request failed: {}", e);
        StatusCode::BAD_GATEWAY
    })?;

    let status = resp.status();
    let mut headers = resp.headers().clone();

    debug!(
        "Video proxy response: status={} content-type={:?} content-length={:?} content-range={:?} accept-ranges={:?}",
        status,
        headers.get(hyper::header::CONTENT_TYPE),
        headers.get(hyper::header::CONTENT_LENGTH),
        headers.get(hyper::header::CONTENT_RANGE),
        headers.get(hyper::header::ACCEPT_RANGES),
    );

    // Drop hop-by-hop headers; Hyper will manage connection semantics downstream.
    strip_hop_by_hop_headers(&mut headers);

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

fn session_matches_server(session: &AuthorizationSession, server: &Server) -> bool {
    session.server_url.trim_end_matches('/') == server.url.as_str().trim_end_matches('/')
}

fn session_for_server(
    sessions: &Option<Vec<(AuthorizationSession, Server)>>,
    server: &Server,
) -> Option<AuthorizationSession> {
    sessions.as_ref().and_then(|sessions| {
        sessions
            .iter()
            .find(|(session, _)| session_matches_server(session, server))
            .map(|(session, _)| session.clone())
    })
}

//http://localhost:3000/Videos/82fe5aab-29ff-9630-05c2-da1a5a640428/82fe5aab29ff963005c2da1a5a640428/Attachments/5
//http://localhost:3000/Videos/71bda5a4-267a-1a6c-49ce-8536d36628d8/71bda5a4267a1a6c49ce8536d36628d8/Subtitles/3/0/Stream.js?api_key=4543ddacf7544d258444677c680d81a5
pub async fn get_video_resource(
    State(state): State<AppState>,
    req: Request,
) -> Result<Response, StatusCode> {
    let preprocessed = preprocess_request(req, &state).await.map_err(|e| {
        error!("Failed to preprocess request: {}", e);
        StatusCode::BAD_REQUEST
    })?;

    let original_request = preprocessed
        .original_request
        .ok_or(StatusCode::BAD_REQUEST)?;

    let id = extract_video_id(original_request.url().path()).ok_or(StatusCode::NOT_FOUND)?;

    let server = if let Some(session) = state.play_sessions.get_session_by_item_id(id).await {
        info!(
            "Found play session for resource: {}, server: {}",
            id, session.server.name
        );
        session.server
    } else {
        error!("No play session found for resource: {}", id);
        return Err(StatusCode::NOT_FOUND);
    };

    let mut upstream_request = original_request;
    let session = session_for_server(&preprocessed.sessions, &server).or_else(|| {
        preprocessed
            .session
            .clone()
            .filter(|session| session_matches_server(session, &server))
    });
    let new_auth = remap_authorization(&preprocessed.auth, &session)
        .await
        .map_err(|e| {
            error!("Failed to remap authorization for resource request: {}", e);
            StatusCode::BAD_REQUEST
        })?;

    apply_to_request(&mut upstream_request, &server, &session, &new_auth, &state).await;

    let url = upstream_request.url().clone();

    match server.media_streaming_mode {
        MediaStreamingMode::Redirect => {
            info!("Redirecting HLS stream to: {}", url);
            Ok(axum::response::Redirect::temporary(url.as_ref()).into_response())
        }
        MediaStreamingMode::Proxy => {
            info!("Proxying HLS stream from: {}", url);
            proxy_request(&state.streaming_reqwest_client, upstream_request).await
        }
    }
}

pub async fn get_stream(
    State(state): State<AppState>,
    req: Request,
) -> Result<Response, StatusCode> {
    let preprocessed = preprocess_request(req, &state).await.map_err(|e| {
        error!("Failed to preprocess request: {}", e);
        StatusCode::BAD_REQUEST
    })?;

    let server = preprocessed.server;
    let request = preprocessed.request;
    let url = request.url().clone();

    match server.media_streaming_mode {
        MediaStreamingMode::Redirect => {
            info!("Redirecting MKV stream to: {}", url);
            Ok(axum::response::Redirect::temporary(url.as_ref()).into_response())
        }
        MediaStreamingMode::Proxy => {
            info!("Proxying MKV stream from: {}", url);
            proxy_request(&state.streaming_reqwest_client, request).await
        }
    }
}
