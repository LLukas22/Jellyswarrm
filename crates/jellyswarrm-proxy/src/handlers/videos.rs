use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use hyper::StatusCode;
use regex::Regex;
use reqwest::Client;
use tracing::{error, info};

use crate::{
    config::MediaStreamingMode, request_preprocessing::preprocess_request,
    url_helper::join_server_url, AppState,
};

async fn proxy_request(client: &Client, url: url::Url) -> Result<Response, StatusCode> {
    let resp = client.get(url).send().await.map_err(|e| {
        error!("Proxy request failed: {}", e);
        StatusCode::BAD_GATEWAY
    })?;

    let status = resp.status();
    let mut headers = resp.headers().clone();

    // Remove headers that might conflict or are connection-specific
    // We let Axum/Hyper handle the transfer encoding for the outgoing response
    headers.remove(hyper::header::TRANSFER_ENCODING);
    headers.remove(hyper::header::CONNECTION);

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

    let re = Regex::new(r"/(?i)Videos/([^/]+)/").unwrap();

    let captures = re
        .captures(original_request.url().path())
        .ok_or(StatusCode::NOT_FOUND)?;

    let id = captures.get(1).map_or("", |m| m.as_str());

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

    // Get the original path and query
    let orig_url = original_request.url().clone();
    let path = state.remove_prefix_from_path(orig_url.path()).await;
    let mut new_url = join_server_url(&server.url, path);
    new_url.set_query(orig_url.query());

    let mode = state.config.read().await.media_streaming_mode;
    match mode {
        MediaStreamingMode::Redirect => {
            info!("Redirecting HLS stream to: {}", new_url);
            Ok(axum::response::Redirect::temporary(new_url.as_ref()).into_response())
        }
        MediaStreamingMode::Proxy => {
            info!("Proxying HLS stream from: {}", new_url);
            proxy_request(&state.reqwest_client, new_url).await
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

    let url = preprocessed.request.url().clone();

    let mode = state.config.read().await.media_streaming_mode;
    match mode {
        MediaStreamingMode::Redirect => {
            info!("Redirecting MKV stream to: {}", url);
            Ok(axum::response::Redirect::temporary(url.as_ref()).into_response())
        }
        MediaStreamingMode::Proxy => {
            info!("Proxying MKV stream from: {}", url);
            proxy_request(&state.reqwest_client, url).await
        }
    }
}
