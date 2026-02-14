//! Axum handlers for SyncPlay HTTP and websocket endpoints.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{models::Authorization, AppState};

use super::models::*;
use super::service::{SessionContext, SyncPlayService, DEFAULT_PING_MS};

async fn session_context_from_request(
    state: &AppState,
    headers: &HeaderMap,
    uri: &Uri,
) -> Result<SessionContext, StatusCode> {
    let mut token = None;
    let mut device_id = None;

    if let Some(raw_auth) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
    {
        if let Ok(auth) = Authorization::parse(raw_auth) {
            token = auth.token.clone();
            device_id = Some(auth.device_id);
        }
    }

    if token.is_none() {
        if let Some(raw_auth) = headers
            .get("x-emby-authorization")
            .and_then(|value| value.to_str().ok())
        {
            if let Ok(auth) = Authorization::parse_with_legacy(raw_auth, true) {
                token = auth.token.clone();
                device_id = Some(auth.device_id);
            }
        }
    }

    if token.is_none() {
        token = headers
            .get("x-mediabrowser-token")
            .or_else(|| headers.get("x-emby-token"))
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string);
    }

    if token.is_none() {
        if let Some(query) = uri.query() {
            let pairs = url::form_urlencoded::parse(query.as_bytes());
            for (k, v) in pairs {
                if k.eq_ignore_ascii_case("apikey") || k.eq_ignore_ascii_case("api_key") {
                    token = Some(v.to_string());
                    break;
                }
            }
        }
    }

    let Some(token) = token else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    let user = state
        .user_authorization
        .get_user_by_virtual_key(&token)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let session_id = match device_id {
        Some(d) if !d.is_empty() => format!("{}:{}", user.id, d),
        _ => format!("{}:token:{}", user.id, token),
    };

    Ok(SessionContext { user, session_id })
}

fn user_id_from_session_id(session_id: &str) -> Option<&str> {
    session_id.split(':').next().filter(|s| !s.is_empty())
}

fn normalize_server_url(input: &str) -> &str {
    input.trim_end_matches('/')
}

async fn users_have_library_access_to_items(
    state: &AppState,
    user_ids: &[String],
    item_ids: &[String],
) -> Result<bool, StatusCode> {
    if item_ids.is_empty() {
        return Ok(true);
    }

    let mut user_server_urls: HashMap<String, HashSet<String>> = HashMap::new();
    for user_id in user_ids {
        if user_server_urls.contains_key(user_id) {
            continue;
        }

        let sessions = state
            .user_authorization
            .get_user_sessions_by_user_id(user_id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        let Some((_user, sessions)) = sessions else {
            return Ok(false);
        };

        let urls = sessions
            .into_iter()
            .map(|(auth_session, _)| normalize_server_url(&auth_session.server_url).to_string())
            .collect::<HashSet<_>>();
        user_server_urls.insert(user_id.clone(), urls);
    }

    for item_id in item_ids {
        let Some((mapping, _)) = state
            .media_storage
            .get_media_mapping_with_server(item_id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        else {
            return Ok(false);
        };

        let needed_server = normalize_server_url(&mapping.server_url);
        for urls in user_server_urls.values() {
            if !urls.contains(needed_server) {
                return Ok(false);
            }
        }
    }

    Ok(true)
}

async fn group_has_library_access(
    state: &AppState,
    group: &SyncPlayGroup,
    item_ids: &[String],
) -> Result<bool, StatusCode> {
    let user_ids = group
        .participants
        .keys()
        .filter_map(|session_id| user_id_from_session_id(session_id).map(ToString::to_string))
        .collect::<Vec<_>>();
    users_have_library_access_to_items(state, &user_ids, item_ids).await
}

async fn user_has_library_access(
    state: &AppState,
    user_id: &str,
    item_ids: &[String],
) -> Result<bool, StatusCode> {
    users_have_library_access_to_items(state, &[user_id.to_string()], item_ids).await
}

async fn handle_ws(state: AppState, session: SessionContext, socket: WebSocket) {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    state
        .syncplay
        .register_websocket(session.session_id.clone(), tx)
        .await;

    loop {
        tokio::select! {
            outbound = rx.recv() => {
                let Some(outbound) = outbound else { break; };
                if ws_sender.send(Message::Text(outbound.into())).await.is_err() {
                    break;
                }
            }
            inbound = ws_receiver.next() => {
                let Some(inbound) = inbound else { break; };
                let Ok(inbound) = inbound else { break; };

                match inbound {
                    Message::Text(text) => {
                        if let Ok(msg) = serde_json::from_str::<InboundWebSocketMessage>(&text) {
                            let _ = &msg.data;
                            if msg.message_type.eq_ignore_ascii_case("KeepAlive") {
                                state.syncplay.send_keepalive(&session.session_id).await;
                            }
                        }
                    }
                    Message::Ping(payload) => {
                        if ws_sender.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
    }

    state
        .syncplay
        .unregister_websocket_and_leave(&session.session_id)
        .await;
}

pub async fn websocket(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    ws: WebSocketUpgrade,
) -> Result<Response, StatusCode> {
    let session = session_context_from_request(&state, &headers, &uri).await?;
    Ok(ws
        .on_upgrade(move |socket| handle_ws(state, session, socket))
        .into_response())
}

pub async fn create_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Json(payload): Json<NewGroupRequestDto>,
) -> Result<Json<GroupInfoDto>, StatusCode> {
    let session = session_context_from_request(&state, &headers, &uri).await?;
    let group = state
        .syncplay
        .create_group(&session, payload.group_name)
        .await;
    Ok(Json(group))
}

pub async fn join_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Json(payload): Json<JoinGroupRequestDto>,
) -> Result<StatusCode, StatusCode> {
    let session = session_context_from_request(&state, &headers, &uri).await?;

    if let Some(group) = state.syncplay.get_group_snapshot_by_id(payload.group_id).await {
        let queue_item_ids = group
            .playlist
            .iter()
            .map(|item| item.item_id.clone())
            .collect::<Vec<_>>();
        if !user_has_library_access(&state, &session.user.id, &queue_item_ids).await? {
            warn!(
                group_id = %payload.group_id,
                "SyncPlay join denied due to library access"
            );
            state
                .syncplay
                .send_library_access_denied_to_session(&session.session_id)
                .await;
            return Ok(StatusCode::NO_CONTENT);
        }
    }

    state.syncplay.join_group(&session, payload.group_id).await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn leave_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<StatusCode, StatusCode> {
    let session = session_context_from_request(&state, &headers, &uri).await?;
    info!(
        "SyncPlay leave requested"
    );
    state.syncplay.leave_group(&session).await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_groups(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Json<Vec<GroupInfoDto>>, StatusCode> {
    let _ = session_context_from_request(&state, &headers, &uri).await?;
    Ok(Json(state.syncplay.list_groups().await))
}

pub async fn get_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Path(group_id): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    let _ = session_context_from_request(&state, &headers, &uri).await?;
    let group_id = Uuid::from_str(&group_id).map_err(|_| StatusCode::NOT_FOUND)?;
    if let Some(group) = state.syncplay.get_group(group_id).await {
        Ok((StatusCode::OK, Json(group)).into_response())
    } else {
        Ok(StatusCode::NOT_FOUND.into_response())
    }
}

pub async fn set_new_queue(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Json(payload): Json<PlayRequestDto>,
) -> Result<StatusCode, StatusCode> {
    let session = session_context_from_request(&state, &headers, &uri).await?;

    if let Some(group) = state
        .syncplay
        .get_group_snapshot_for_session(&session.session_id)
        .await
    {
        if !group_has_library_access(&state, &group, &payload.playing_queue).await? {
            warn!(
                item_count = payload.playing_queue.len(),
                "SyncPlay SetNewQueue denied due to library access"
            );
            state
                .syncplay
                .send_library_access_denied_to_session(&session.session_id)
                .await;
            return Ok(StatusCode::NO_CONTENT);
        }
    }

    state
        .syncplay
        .with_group_for_session(&session, move |group, sync_state| {
            info!(
                group_id = %group.group_id,
                item_count = payload.playing_queue.len(),
                start_position_ticks = payload.start_position_ticks,
                "SyncPlay set new queue"
            );
            group.playlist = payload
                .playing_queue
                .into_iter()
                .map(|item_id| SyncPlayQueueItem {
                    item_id,
                    playlist_item_id: Uuid::new_v4(),
                })
                .collect();
            group.playing_item_index = if group.playlist.is_empty() {
                None
            } else {
                Some(payload.playing_item_position.min(group.playlist.len() - 1))
            };
            group.start_position_ticks = payload.start_position_ticks;
            group.is_playing = false;
            group.state = GroupStateType::Waiting;
            group.waiting_resume_playing = false;
            for member in group.participants.values_mut() {
                member.is_buffering = true;
            }
            group.touch();

            SyncPlayService::send_play_queue_update_locked(
                sync_state,
                group,
                PlayQueueUpdateReason::NewPlaylist,
            );
            SyncPlayService::send_state_update_locked(sync_state, group, "Play");
        })
        .await;

    Ok(StatusCode::NO_CONTENT)
}

pub async fn set_playlist_item(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Json(payload): Json<SetPlaylistItemRequestDto>,
) -> Result<StatusCode, StatusCode> {
    let session = session_context_from_request(&state, &headers, &uri).await?;
    state
        .syncplay
        .with_group_for_session(&session, move |group, sync_state| {
            if let Some(index) = group
                .playlist
                .iter()
                .position(|item| item.playlist_item_id == payload.playlist_item_id)
            {
                info!(
                    group_id = %group.group_id,
                    playlist_item_id = %payload.playlist_item_id,
                    new_index = index,
                    "SyncPlay set current playlist item"
                );
                group.playing_item_index = Some(index);
                group.start_position_ticks = 0;
                group.state = GroupStateType::Waiting;
                group.waiting_resume_playing = group.is_playing;
                for member in group.participants.values_mut() {
                    member.is_buffering = true;
                }
                group.touch();
                SyncPlayService::send_play_queue_update_locked(
                    sync_state,
                    group,
                    PlayQueueUpdateReason::SetCurrentItem,
                );
                SyncPlayService::send_state_update_locked(sync_state, group, "SetPlaylistItem");
            }
        })
        .await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn remove_from_playlist(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Json(payload): Json<RemoveFromPlaylistRequestDto>,
) -> Result<StatusCode, StatusCode> {
    let session = session_context_from_request(&state, &headers, &uri).await?;
    state
        .syncplay
        .with_group_for_session(&session, move |group, sync_state| {
            if payload.clear_playlist {
                info!(
                    group_id = %group.group_id,
                    "SyncPlay clear playlist"
                );
                group.playlist.clear();
                group.playing_item_index = None;
                group.state = GroupStateType::Idle;
                group.is_playing = false;
                group.waiting_resume_playing = false;
                group.touch();
                SyncPlayService::send_play_queue_update_locked(
                    sync_state,
                    group,
                    PlayQueueUpdateReason::RemoveItems,
                );
                SyncPlayService::send_state_update_locked(sync_state, group, "RemoveFromPlaylist");
                return;
            }

            let to_remove: HashSet<Uuid> = payload.playlist_item_ids.into_iter().collect();
            info!(
                group_id = %group.group_id,
                removed_count = to_remove.len(),
                "SyncPlay remove items from playlist"
            );
            let old_current = group.playing_item_index.and_then(|i| group.playlist.get(i));
            let old_current_id = old_current.map(|item| item.playlist_item_id);

            group
                .playlist
                .retain(|item| !to_remove.contains(&item.playlist_item_id));

            if payload.clear_playing_item {
                group.playing_item_index = None;
            } else if let Some(current_id) = old_current_id {
                group.playing_item_index = group
                    .playlist
                    .iter()
                    .position(|item| item.playlist_item_id == current_id);
            }

            if group.playlist.is_empty() {
                group.state = GroupStateType::Idle;
                group.is_playing = false;
                group.waiting_resume_playing = false;
            }
            group.touch();
            SyncPlayService::send_play_queue_update_locked(
                sync_state,
                group,
                PlayQueueUpdateReason::RemoveItems,
            );
        })
        .await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn move_playlist_item(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Json(payload): Json<MovePlaylistItemRequestDto>,
) -> Result<StatusCode, StatusCode> {
    let session = session_context_from_request(&state, &headers, &uri).await?;
    state
        .syncplay
        .with_group_for_session(&session, move |group, sync_state| {
            if let Some(current_index) = group
                .playlist
                .iter()
                .position(|item| item.playlist_item_id == payload.playlist_item_id)
            {
                info!(
                    group_id = %group.group_id,
                    playlist_item_id = %payload.playlist_item_id,
                    from_index = current_index,
                    to_index = payload.new_index,
                    "SyncPlay move playlist item"
                );
                let item = group.playlist.remove(current_index);
                let target = payload.new_index.min(group.playlist.len());
                group.playlist.insert(target, item);
                group.touch();
                SyncPlayService::send_play_queue_update_locked(
                    sync_state,
                    group,
                    PlayQueueUpdateReason::MoveItem,
                );
            }
        })
        .await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn queue_items(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Json(payload): Json<QueueRequestDto>,
) -> Result<StatusCode, StatusCode> {
    let session = session_context_from_request(&state, &headers, &uri).await?;

    if let Some(group) = state
        .syncplay
        .get_group_snapshot_for_session(&session.session_id)
        .await
    {
        if !group_has_library_access(&state, &group, &payload.item_ids).await? {
            warn!(
                item_count = payload.item_ids.len(),
                "SyncPlay Queue denied due to library access"
            );
            state
                .syncplay
                .send_library_access_denied_to_session(&session.session_id)
                .await;
            return Ok(StatusCode::NO_CONTENT);
        }
    }

    state
        .syncplay
        .with_group_for_session(&session, move |group, sync_state| {
            let mode = payload.mode;
            info!(
                group_id = %group.group_id,
                item_count = payload.item_ids.len(),
                mode = ?mode,
                "SyncPlay queue items"
            );
            let mut new_items: Vec<SyncPlayQueueItem> = payload
                .item_ids
                .into_iter()
                .map(|item_id| SyncPlayQueueItem {
                    item_id,
                    playlist_item_id: Uuid::new_v4(),
                })
                .collect();

            match mode {
                GroupQueueMode::Queue => group.playlist.append(&mut new_items),
                GroupQueueMode::QueueNext => {
                    let insert_at = group.playing_item_index.map(|idx| idx + 1).unwrap_or(0);
                    for (offset, item) in new_items.into_iter().enumerate() {
                        group.playlist.insert(insert_at + offset, item);
                    }
                }
            }

            if group.playing_item_index.is_none() && !group.playlist.is_empty() {
                group.playing_item_index = Some(0);
            }
            group.touch();
            SyncPlayService::send_play_queue_update_locked(
                sync_state,
                group,
                match mode {
                    GroupQueueMode::Queue => PlayQueueUpdateReason::Queue,
                    GroupQueueMode::QueueNext => PlayQueueUpdateReason::QueueNext,
                },
            );
        })
        .await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn unpause(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<StatusCode, StatusCode> {
    let session = session_context_from_request(&state, &headers, &uri).await?;
    state
        .syncplay
        .with_group_for_session(&session, |group, sync_state| {
            info!(
                group_id = %group.group_id,
                "SyncPlay unpause requested"
            );
            group.state = GroupStateType::Playing;
            group.is_playing = true;
            group.waiting_resume_playing = true;
            group.touch();
            SyncPlayService::send_command_to_group_locked(
                sync_state,
                group,
                SendCommandType::Unpause,
                Some(group.start_position_ticks),
                std::cmp::max(group.participants.values().map(|p| p.ping as i64).max().unwrap_or(DEFAULT_PING_MS) * 2, DEFAULT_PING_MS),
            );
            SyncPlayService::send_state_update_locked(sync_state, group, "Unpause");
        })
        .await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn pause(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<StatusCode, StatusCode> {
    let session = session_context_from_request(&state, &headers, &uri).await?;
    state
        .syncplay
        .with_group_for_session(&session, |group, sync_state| {
            info!(
                group_id = %group.group_id,
                "SyncPlay pause requested"
            );
            group.state = GroupStateType::Paused;
            group.is_playing = false;
            group.waiting_resume_playing = false;
            group.touch();
            SyncPlayService::send_command_to_group_locked(
                sync_state,
                group,
                SendCommandType::Pause,
                Some(group.start_position_ticks),
                0,
            );
            SyncPlayService::send_state_update_locked(sync_state, group, "Pause");
        })
        .await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn stop(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<StatusCode, StatusCode> {
    let session = session_context_from_request(&state, &headers, &uri).await?;
    state
        .syncplay
        .with_group_for_session(&session, |group, sync_state| {
            info!(
                group_id = %group.group_id,
                "SyncPlay stop requested"
            );
            group.state = GroupStateType::Idle;
            group.is_playing = false;
            group.waiting_resume_playing = false;
            group.start_position_ticks = 0;
            group.touch();
            SyncPlayService::send_command_to_group_locked(sync_state, group, SendCommandType::Stop, None, 0);
            SyncPlayService::send_state_update_locked(sync_state, group, "Stop");
        })
        .await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn seek(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Json(payload): Json<SeekRequestDto>,
) -> Result<StatusCode, StatusCode> {
    let session = session_context_from_request(&state, &headers, &uri).await?;
    state
        .syncplay
        .with_group_for_session(&session, move |group, sync_state| {
            info!(
                group_id = %group.group_id,
                position_ticks = payload.position_ticks,
                "SyncPlay seek requested"
            );
            group.start_position_ticks = payload.position_ticks.max(0);
            group.state = GroupStateType::Waiting;
            group.waiting_resume_playing = group.is_playing;
            for member in group.participants.values_mut() {
                member.is_buffering = true;
            }
            group.touch();
            SyncPlayService::send_command_to_group_locked(
                sync_state,
                group,
                SendCommandType::Seek,
                Some(group.start_position_ticks),
                0,
            );
            SyncPlayService::send_state_update_locked(sync_state, group, "Seek");
        })
        .await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn buffering(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Json(payload): Json<BufferRequestDto>,
) -> Result<StatusCode, StatusCode> {
    let session = session_context_from_request(&state, &headers, &uri).await?;
    let session_id = session.session_id.clone();
    state
        .syncplay
        .with_group_for_session(&session, move |group, sync_state| {
            if let Some(current) = group.playing_item_index.and_then(|idx| group.playlist.get(idx))
            {
                if current.playlist_item_id != payload.playlist_item_id {
                    SyncPlayService::send_play_queue_update_to_sessions_locked(
                        sync_state,
                        group,
                        PlayQueueUpdateReason::SetCurrentItem,
                        vec![session_id.clone()],
                    );
                    return;
                }
            }
            if let Some(member) = group.participants.get_mut(&session_id) {
                member.is_buffering = true;
            }
            debug!(
                group_id = %group.group_id,
                playlist_item_id = %payload.playlist_item_id,
                position_ticks = payload.position_ticks,
                "SyncPlay buffering update"
            );
            group.start_position_ticks = payload.position_ticks.max(0);
            group.is_playing = payload.is_playing;
            group.state = GroupStateType::Waiting;
            group.waiting_resume_playing = payload.is_playing;
            if payload.when > group.last_updated_at {
                group.last_updated_at = payload.when;
            }
            group.touch();
            SyncPlayService::send_state_update_locked(sync_state, group, "Buffer");
        })
        .await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn ready(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Json(payload): Json<BufferRequestDto>,
) -> Result<StatusCode, StatusCode> {
    let session = session_context_from_request(&state, &headers, &uri).await?;
    let session_id = session.session_id.clone();
    state
        .syncplay
        .with_group_for_session(&session, move |group, sync_state| {
            if let Some(current) = group.playing_item_index.and_then(|idx| group.playlist.get(idx))
            {
                if current.playlist_item_id != payload.playlist_item_id {
                    SyncPlayService::send_play_queue_update_to_sessions_locked(
                        sync_state,
                        group,
                        PlayQueueUpdateReason::SetCurrentItem,
                        vec![session_id.clone()],
                    );
                    return;
                }
            }
            if let Some(member) = group.participants.get_mut(&session_id) {
                member.is_buffering = false;
            }
            debug!(
                group_id = %group.group_id,
                playlist_item_id = %payload.playlist_item_id,
                position_ticks = payload.position_ticks,
                "SyncPlay ready update"
            );
            group.start_position_ticks = payload.position_ticks.max(0);
            group.waiting_resume_playing = payload.is_playing;
            group.state = GroupStateType::Waiting;
            if payload.when > group.last_updated_at {
                group.last_updated_at = payload.when;
            }
            group.touch();
            SyncPlayService::resolve_waiting_state_locked(sync_state, group, "Ready");
        })
        .await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn set_ignore_wait(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Json(payload): Json<IgnoreWaitRequestDto>,
) -> Result<StatusCode, StatusCode> {
    let session = session_context_from_request(&state, &headers, &uri).await?;
    let session_id = session.session_id.clone();
    state
        .syncplay
        .with_group_for_session(&session, move |group, _| {
            if let Some(member) = group.participants.get_mut(&session_id) {
                member.ignore_wait = payload.ignore_wait;
            }
            group.touch();
        })
        .await;

    state
        .syncplay
        .with_group_for_session(&session, |group, sync_state| {
            SyncPlayService::resolve_waiting_state_locked(sync_state, group, "IgnoreWait");
        })
        .await;

    Ok(StatusCode::NO_CONTENT)
}

pub async fn next_item(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    payload: Option<Json<NextItemRequestDto>>,
) -> Result<StatusCode, StatusCode> {
    let session = session_context_from_request(&state, &headers, &uri).await?;
    let playlist_item_id = payload.as_ref().map(|p| p.0.playlist_item_id);
    state
        .syncplay
        .with_group_for_session(&session, move |group, sync_state| {
            if group.playlist.is_empty() {
                return;
            }
            let current = playlist_item_id
                .and_then(|id| group.playlist.iter().position(|item| item.playlist_item_id == id))
                .or(group.playing_item_index)
                .unwrap_or(0);
            let next = (current + 1) % group.playlist.len();
            info!(
                group_id = %group.group_id,
                from_index = current,
                to_index = next,
                "SyncPlay next item"
            );
            group.playing_item_index = Some(next);
            group.start_position_ticks = 0;
            group.state = GroupStateType::Waiting;
            group.waiting_resume_playing = group.is_playing;
            for member in group.participants.values_mut() {
                member.is_buffering = true;
            }
            group.touch();
            SyncPlayService::send_play_queue_update_locked(
                sync_state,
                group,
                PlayQueueUpdateReason::NextItem,
            );
            SyncPlayService::send_state_update_locked(sync_state, group, "NextItem");
        })
        .await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn previous_item(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    payload: Option<Json<NextItemRequestDto>>,
) -> Result<StatusCode, StatusCode> {
    let session = session_context_from_request(&state, &headers, &uri).await?;
    let playlist_item_id = payload.as_ref().map(|p| p.0.playlist_item_id);
    state
        .syncplay
        .with_group_for_session(&session, move |group, sync_state| {
            if group.playlist.is_empty() {
                return;
            }
            let current = playlist_item_id
                .and_then(|id| group.playlist.iter().position(|item| item.playlist_item_id == id))
                .or(group.playing_item_index)
                .unwrap_or(0);
            let prev = if current == 0 {
                group.playlist.len() - 1
            } else {
                current - 1
            };
            info!(
                group_id = %group.group_id,
                from_index = current,
                to_index = prev,
                "SyncPlay previous item"
            );
            group.playing_item_index = Some(prev);
            group.start_position_ticks = 0;
            group.state = GroupStateType::Waiting;
            group.waiting_resume_playing = group.is_playing;
            for member in group.participants.values_mut() {
                member.is_buffering = true;
            }
            group.touch();
            SyncPlayService::send_play_queue_update_locked(
                sync_state,
                group,
                PlayQueueUpdateReason::PreviousItem,
            );
            SyncPlayService::send_state_update_locked(sync_state, group, "PreviousItem");
        })
        .await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn set_repeat_mode(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Json(payload): Json<SetRepeatModeRequestDto>,
) -> Result<StatusCode, StatusCode> {
    let session = session_context_from_request(&state, &headers, &uri).await?;
    state
        .syncplay
        .with_group_for_session(&session, move |group, sync_state| {
            group.repeat_mode = payload.mode;
            group.touch();
            SyncPlayService::send_play_queue_update_locked(
                sync_state,
                group,
                PlayQueueUpdateReason::RepeatMode,
            );
        })
        .await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn set_shuffle_mode(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Json(payload): Json<SetShuffleModeRequestDto>,
) -> Result<StatusCode, StatusCode> {
    let session = session_context_from_request(&state, &headers, &uri).await?;
    state
        .syncplay
        .with_group_for_session(&session, move |group, sync_state| {
            group.shuffle_mode = payload.mode;
            group.touch();
            SyncPlayService::send_play_queue_update_locked(
                sync_state,
                group,
                PlayQueueUpdateReason::ShuffleMode,
            );
        })
        .await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn ping(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Json(payload): Json<PingRequestDto>,
) -> Result<StatusCode, StatusCode> {
    let session = session_context_from_request(&state, &headers, &uri).await?;
    let session_id = session.session_id.clone();
    state
        .syncplay
        .with_group_for_session(&session, move |group, _| {
            if let Some(member) = group.participants.get_mut(&session_id) {
                member.ping = payload.ping;
            }
            group.touch();
        })
        .await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn get_utc_time(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Query(_query): Query<HashMap<String, String>>,
) -> Result<Json<UtcTimeResponse>, StatusCode> {
    let _ = session_context_from_request(&state, &headers, &uri).await?;
    let request_reception_time = Utc::now();
    let response_transmission_time = Utc::now();
    debug!("Handled local /GetUtcTime request");
    Ok(Json(UtcTimeResponse {
        request_reception_time,
        response_transmission_time,
    }))
}
