//! SyncPlay HTTP extraction, access checks, and response policy. State mutation
//! and ordered notifications belong to named commands in the SyncPlay feature.
use crate::{http::state::SessionState, sessions::SessionContext, syncplay::models::*};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use chrono::Utc;
use std::{collections::HashMap, str::FromStr};
use tracing::{debug, info};
use uuid::Uuid;

pub(crate) async fn create_group(
    State(state): State<SessionState>,
    session: SessionContext,
    Json(payload): Json<NewGroupRequestDto>,
) -> Result<Json<GroupInfoDto>, StatusCode> {
    state
        .syncplay
        .create_group(&session, payload.group_name)
        .await
        .map(Json)
        .ok_or(StatusCode::CONFLICT)
}

pub(crate) async fn join_group(
    State(state): State<SessionState>,
    session: SessionContext,
    Json(payload): Json<JoinGroupRequestDto>,
) -> Result<StatusCode, StatusCode> {
    let outcome = state
        .syncplay_operations()
        .join(&session, payload.group_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    use crate::application::syncplay::JoinOutcome;
    Ok(match outcome {
        JoinOutcome::Rejected => StatusCode::CONFLICT,
        JoinOutcome::Accepted | JoinOutcome::AccessDenied => StatusCode::NO_CONTENT,
    })
}

pub(crate) async fn leave_group(
    State(state): State<SessionState>,
    session: SessionContext,
) -> Result<StatusCode, StatusCode> {
    info!("SyncPlay leave requested");
    state.syncplay.leave_group(&session).await;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn list_groups(
    State(state): State<SessionState>,
    session: SessionContext,
) -> Result<Json<Vec<GroupInfoDto>>, StatusCode> {
    state
        .syncplay_operations()
        .list(&session)
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

pub(crate) async fn get_group(
    State(state): State<SessionState>,
    session: SessionContext,
    Path(group_id): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    let group_id = Uuid::from_str(&group_id).map_err(|_| StatusCode::NOT_FOUND)?;
    let Some(group) = state
        .syncplay_operations()
        .get(&session, group_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    Ok((StatusCode::OK, Json(group)).into_response())
}

pub(crate) async fn set_new_queue(
    State(state): State<SessionState>,
    session: SessionContext,
    Json(payload): Json<PlayRequestDto>,
) -> Result<StatusCode, StatusCode> {
    state
        .syncplay_operations()
        .set_queue(&session, payload)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn queue_items(
    State(state): State<SessionState>,
    session: SessionContext,
    Json(payload): Json<QueueRequestDto>,
) -> Result<StatusCode, StatusCode> {
    state
        .syncplay_operations()
        .queue_items(&session, payload)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn set_playlist_item(
    State(state): State<SessionState>,
    session: SessionContext,
    Json(payload): Json<SetPlaylistItemRequestDto>,
) -> Result<StatusCode, StatusCode> {
    state.syncplay.set_playlist_item(&session, payload).await;
    Ok(StatusCode::NO_CONTENT)
}
pub(crate) async fn remove_from_playlist(
    State(state): State<SessionState>,
    session: SessionContext,
    Json(payload): Json<RemoveFromPlaylistRequestDto>,
) -> Result<StatusCode, StatusCode> {
    state.syncplay.remove_from_playlist(&session, payload).await;
    Ok(StatusCode::NO_CONTENT)
}
pub(crate) async fn move_playlist_item(
    State(state): State<SessionState>,
    session: SessionContext,
    Json(payload): Json<MovePlaylistItemRequestDto>,
) -> Result<StatusCode, StatusCode> {
    state.syncplay.move_playlist_item(&session, payload).await;
    Ok(StatusCode::NO_CONTENT)
}
pub(crate) async fn unpause(
    State(state): State<SessionState>,
    session: SessionContext,
) -> Result<StatusCode, StatusCode> {
    state.syncplay.unpause(&session).await;
    Ok(StatusCode::NO_CONTENT)
}
pub(crate) async fn pause(
    State(state): State<SessionState>,
    session: SessionContext,
) -> Result<StatusCode, StatusCode> {
    state.syncplay.pause(&session).await;
    Ok(StatusCode::NO_CONTENT)
}
pub(crate) async fn stop(
    State(state): State<SessionState>,
    session: SessionContext,
) -> Result<StatusCode, StatusCode> {
    state.syncplay.stop(&session).await;
    Ok(StatusCode::NO_CONTENT)
}
pub(crate) async fn seek(
    State(state): State<SessionState>,
    session: SessionContext,
    Json(payload): Json<SeekRequestDto>,
) -> Result<StatusCode, StatusCode> {
    state.syncplay.seek(&session, payload).await;
    Ok(StatusCode::NO_CONTENT)
}
pub(crate) async fn buffering(
    State(state): State<SessionState>,
    session: SessionContext,
    Json(payload): Json<BufferRequestDto>,
) -> Result<StatusCode, StatusCode> {
    state.syncplay.buffering(&session, payload).await;
    Ok(StatusCode::NO_CONTENT)
}
pub(crate) async fn ready(
    State(state): State<SessionState>,
    session: SessionContext,
    Json(payload): Json<BufferRequestDto>,
) -> Result<StatusCode, StatusCode> {
    state.syncplay.ready(&session, payload).await;
    Ok(StatusCode::NO_CONTENT)
}
pub(crate) async fn set_ignore_wait(
    State(state): State<SessionState>,
    session: SessionContext,
    Json(payload): Json<IgnoreWaitRequestDto>,
) -> Result<StatusCode, StatusCode> {
    state.syncplay.set_ignore_wait(&session, payload).await;
    Ok(StatusCode::NO_CONTENT)
}
pub(crate) async fn next_item(
    State(state): State<SessionState>,
    session: SessionContext,
    payload: Option<Json<NextItemRequestDto>>,
) -> Result<StatusCode, StatusCode> {
    state
        .syncplay
        .next_item(
            &session,
            payload.and_then(|payload| payload.0.playlist_item_id),
        )
        .await;
    Ok(StatusCode::NO_CONTENT)
}
pub(crate) async fn previous_item(
    State(state): State<SessionState>,
    session: SessionContext,
    payload: Option<Json<NextItemRequestDto>>,
) -> Result<StatusCode, StatusCode> {
    state
        .syncplay
        .previous_item(
            &session,
            payload.and_then(|payload| payload.0.playlist_item_id),
        )
        .await;
    Ok(StatusCode::NO_CONTENT)
}
pub(crate) async fn set_repeat_mode(
    State(state): State<SessionState>,
    session: SessionContext,
    Json(payload): Json<SetRepeatModeRequestDto>,
) -> Result<StatusCode, StatusCode> {
    state.syncplay.set_repeat_mode(&session, payload).await;
    Ok(StatusCode::NO_CONTENT)
}
pub(crate) async fn set_shuffle_mode(
    State(state): State<SessionState>,
    session: SessionContext,
    Json(payload): Json<SetShuffleModeRequestDto>,
) -> Result<StatusCode, StatusCode> {
    state.syncplay.set_shuffle_mode(&session, payload).await;
    Ok(StatusCode::NO_CONTENT)
}
pub(crate) async fn ping(
    State(state): State<SessionState>,
    session: SessionContext,
    Json(payload): Json<PingRequestDto>,
) -> Result<StatusCode, StatusCode> {
    state.syncplay.ping(&session, payload).await;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn get_utc_time(
    Query(_query): Query<HashMap<String, String>>,
) -> Result<Json<UtcTimeResponse>, StatusCode> {
    let request_reception_time = Utc::now();
    let response_transmission_time = Utc::now();
    debug!("Handled local /GetUtcTime request");
    Ok(Json(UtcTimeResponse {
        request_reception_time,
        response_transmission_time,
    }))
}
