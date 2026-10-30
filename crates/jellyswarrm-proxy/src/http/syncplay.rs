//! SyncPlay HTTP extraction, access checks, and response policy. State mutation
//! and ordered notifications belong to named commands in the SyncPlay feature.
use crate::{
    application::{
        error::ApplicationError,
        syncplay::{JoinOutcome, SyncPlayOperations},
    },
    sessions::SessionContext,
    syncplay::models::*,
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use chrono::Utc;
use std::str::FromStr;
use tracing::{debug, info};
use uuid::Uuid;

/// Group commands are fire-and-forget: the SyncPlay state machine decides the
/// effect and notifies members, so the caller always receives 204.
macro_rules! commands {
    (
        without_body: $($name:ident),* ;
        with_body: $($with_body:ident($payload:ty)),* $(,)?
    ) => {
        $(pub(crate) async fn $name(
            State(state): State<SyncPlayOperations>,
            session: SessionContext,
        ) -> StatusCode {
            state.groups.$name(&session).await;
            StatusCode::NO_CONTENT
        })*
        $(pub(crate) async fn $with_body(
            State(state): State<SyncPlayOperations>,
            session: SessionContext,
            Json(payload): Json<$payload>,
        ) -> StatusCode {
            state.groups.$with_body(&session, payload).await;
            StatusCode::NO_CONTENT
        })*
    };
}

commands! {
    without_body: unpause, pause, stop;
    with_body:
        set_playlist_item(SetPlaylistItemRequestDto),
        remove_from_playlist(RemoveFromPlaylistRequestDto),
        move_playlist_item(MovePlaylistItemRequestDto),
        seek(SeekRequestDto),
        buffering(BufferRequestDto),
        ready(BufferRequestDto),
        set_ignore_wait(IgnoreWaitRequestDto),
        set_repeat_mode(SetRepeatModeRequestDto),
        set_shuffle_mode(SetShuffleModeRequestDto),
        ping(PingRequestDto),
}

pub(crate) async fn create_group(
    State(state): State<SyncPlayOperations>,
    session: SessionContext,
    Json(payload): Json<NewGroupRequestDto>,
) -> Result<Json<GroupInfoDto>, StatusCode> {
    state
        .groups
        .create_group(&session, payload.group_name)
        .await
        .map(Json)
        .ok_or(StatusCode::CONFLICT)
}

pub(crate) async fn join_group(
    State(state): State<SyncPlayOperations>,
    session: SessionContext,
    Json(payload): Json<JoinGroupRequestDto>,
) -> Result<StatusCode, ApplicationError> {
    let outcome = state
        .join(&session, payload.group_id)
        .await
        .map_err(ApplicationError::Storage)?;
    Ok(match outcome {
        JoinOutcome::Rejected => StatusCode::CONFLICT,
        JoinOutcome::Accepted | JoinOutcome::AccessDenied => StatusCode::NO_CONTENT,
    })
}

pub(crate) async fn leave_group(
    State(state): State<SyncPlayOperations>,
    session: SessionContext,
) -> StatusCode {
    info!("SyncPlay leave requested");
    state.groups.leave_group(&session).await;
    StatusCode::NO_CONTENT
}

pub(crate) async fn list_groups(
    State(state): State<SyncPlayOperations>,
    session: SessionContext,
) -> Result<Json<Vec<GroupInfoDto>>, ApplicationError> {
    state
        .list(&session)
        .await
        .map(Json)
        .map_err(ApplicationError::Storage)
}

pub(crate) async fn get_group(
    State(state): State<SyncPlayOperations>,
    session: SessionContext,
    Path(group_id): Path<String>,
) -> Result<Json<GroupInfoDto>, StatusCode> {
    let group_id = Uuid::from_str(&group_id).map_err(|_| StatusCode::NOT_FOUND)?;
    state
        .get(&session, group_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

pub(crate) async fn set_new_queue(
    State(state): State<SyncPlayOperations>,
    session: SessionContext,
    Json(payload): Json<PlayRequestDto>,
) -> Result<StatusCode, ApplicationError> {
    state
        .set_queue(&session, payload)
        .await
        .map_err(ApplicationError::Storage)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn queue_items(
    State(state): State<SyncPlayOperations>,
    session: SessionContext,
    Json(payload): Json<QueueRequestDto>,
) -> Result<StatusCode, ApplicationError> {
    state
        .queue_items(&session, payload)
        .await
        .map_err(ApplicationError::Storage)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn next_item(
    State(state): State<SyncPlayOperations>,
    session: SessionContext,
    payload: Option<Json<NextItemRequestDto>>,
) -> StatusCode {
    let item = payload.and_then(|Json(payload)| payload.playlist_item_id);
    state.groups.next_item(&session, item).await;
    StatusCode::NO_CONTENT
}

pub(crate) async fn previous_item(
    State(state): State<SyncPlayOperations>,
    session: SessionContext,
    payload: Option<Json<NextItemRequestDto>>,
) -> StatusCode {
    let item = payload.and_then(|Json(payload)| payload.playlist_item_id);
    state.groups.previous_item(&session, item).await;
    StatusCode::NO_CONTENT
}

pub(crate) async fn get_utc_time() -> Json<UtcTimeResponse> {
    let request_reception_time = Utc::now();
    let response_transmission_time = Utc::now();
    debug!("Handled local /GetUtcTime request");
    Json(UtcTimeResponse {
        request_reception_time,
        response_transmission_time,
    })
}
