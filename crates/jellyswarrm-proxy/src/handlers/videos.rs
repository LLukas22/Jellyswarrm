use axum::extract::{Request, State};
use hyper::StatusCode;
use regex::Regex;
use tracing::{error, info};

use crate::{request_preprocessing::preprocess_request, url_helper::join_server_url, AppState};

//http://localhost:3000/videos/71bda5a4-267a-1a6c-49ce-8536d36628d8/master.m3u8?DeviceId=TW96aWxsYS81LjAgKFgxMTsgTGludXggeDg2XzY0OyBydjoxNDEuMCkgR2Vja28vMjAxMDAxMDEgRmlyZWZveC8xNDEuMHwxNzUzNTM1MDA0NDk4&MediaSourceId=4984199da7b84d1d8ca640cafe041e20&VideoCodec=av1%2Ch264%2Cvp9&AudioCodec=aac%2Copus%2Cflac&AudioStreamIndex=1&VideoBitrate=2147099647&AudioBitrate=384000&MaxFramerate=24&PlaySessionId=f6f93680f3f345e1a90c8d73d8c56698&api_key=2fac9237707a4bfb8a6a601ba0c6b4a0&SubtitleMethod=Encode&TranscodingMaxAudioChannels=2&RequireAvc=false&EnableAudioVbrEncoding=true&Tag=dcfdf6b92443006121a95aaa46804a0a&SegmentContainer=mp4&MinSegments=1&BreakOnNonKeyFrames=True&h264-level=40&h264-videobitdepth=8&h264-profile=high&av1-profile=main&av1-rangetype=SDR&av1-level=19&vp9-rangetype=SDR&h264-rangetype=SDR&h264-deinterlace=true&TranscodeReasons=ContainerNotSupported%2C+AudioCodecNotSupported
//http://localhost:3000/Videos/82fe5aab-29ff-9630-05c2-da1a5a640428/82fe5aab29ff963005c2da1a5a640428/Attachments/5
//http://localhost:3000/Videos/71bda5a4-267a-1a6c-49ce-8536d36628d8/71bda5a4267a1a6c49ce8536d36628d8/Subtitles/3/0/Stream.js?api_key=4543ddacf7544d258444677c680d81a5
pub async fn get_video_resource(
    State(state): State<AppState>,
    req: Request,
) -> Result<axum::response::Redirect, StatusCode> {
    let preprocessed = preprocess_request(req, &state).await.map_err(|e| {
        error!("Failed to preprocess request: {}", e);
        StatusCode::BAD_REQUEST
    })?;

    let original_request = preprocessed
        .original_request
        .ok_or(StatusCode::BAD_REQUEST)?;

    let re = Regex::new(r"(?i)/videos/([^/]+)/").unwrap();

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

    info!("Redirecting HLS stream to: {}", new_url);

    Ok(axum::response::Redirect::temporary(new_url.as_ref()))
}

pub async fn get_stream(
    State(state): State<AppState>,
    req: Request,
) -> Result<axum::response::Redirect, StatusCode> {
    let preprocessed = preprocess_request(req, &state).await.map_err(|e| {
        error!("Failed to preprocess request: {}", e);
        StatusCode::BAD_REQUEST
    })?;

    let url = preprocessed.request.url().clone();

    info!("Redirecting MKV stream to: {}", url);

    Ok(axum::response::Redirect::temporary(url.as_ref()))
}
