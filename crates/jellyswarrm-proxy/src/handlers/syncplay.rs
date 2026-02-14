use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, RwLock};
use tracing::debug;
use uuid::Uuid;

use crate::{models::Authorization, user_authorization_service::User, AppState};

const DEFAULT_PING_MS: i64 = 500;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum GroupStateType {
    Idle,
    Waiting,
    Paused,
    Playing,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum GroupQueueMode {
    Queue,
    QueueNext,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum GroupShuffleMode {
    Sorted,
    Shuffle,
}

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum GroupRepeatMode {
    RepeatOne,
    RepeatAll,
    RepeatNone,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "PascalCase")]
enum GroupUpdateType {
    UserJoined,
    UserLeft,
    GroupJoined,
    GroupLeft,
    StateUpdate,
    PlayQueue,
    NotInGroup,
    GroupDoesNotExist,
    LibraryAccessDenied,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "PascalCase")]
enum SendCommandType {
    Unpause,
    Pause,
    Stop,
    Seek,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "PascalCase")]
enum PlayQueueUpdateReason {
    NewPlaylist,
    SetCurrentItem,
    RemoveItems,
    MoveItem,
    Queue,
    QueueNext,
    NextItem,
    PreviousItem,
    RepeatMode,
    ShuffleMode,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct GroupInfoDto {
    pub group_id: Uuid,
    pub group_name: String,
    pub state: GroupStateType,
    pub participants: Vec<String>,
    pub last_updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct NewGroupRequestDto {
    pub group_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct JoinGroupRequestDto {
    pub group_id: Uuid,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PlayRequestDto {
    pub playing_queue: Vec<String>,
    pub playing_item_position: usize,
    pub start_position_ticks: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SetPlaylistItemRequestDto {
    pub playlist_item_id: Uuid,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RemoveFromPlaylistRequestDto {
    pub playlist_item_ids: Vec<Uuid>,
    pub clear_playlist: bool,
    pub clear_playing_item: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MovePlaylistItemRequestDto {
    pub playlist_item_id: Uuid,
    pub new_index: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct QueueRequestDto {
    pub item_ids: Vec<String>,
    pub mode: GroupQueueMode,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SeekRequestDto {
    pub position_ticks: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct BufferRequestDto {
    pub when: DateTime<Utc>,
    pub position_ticks: i64,
    pub is_playing: bool,
    pub playlist_item_id: Uuid,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct IgnoreWaitRequestDto {
    pub ignore_wait: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct NextItemRequestDto {
    pub playlist_item_id: Uuid,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SetRepeatModeRequestDto {
    pub mode: GroupRepeatMode,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SetShuffleModeRequestDto {
    pub mode: GroupShuffleMode,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PingRequestDto {
    pub ping: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct UtcTimeResponse {
    pub request_reception_time: DateTime<Utc>,
    pub response_transmission_time: DateTime<Utc>,
}

#[derive(Debug, Clone)]
struct GroupParticipant {
    user_name: String,
    ping: u64,
    is_buffering: bool,
    ignore_wait: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
struct SyncPlayQueueItem {
    item_id: String,
    playlist_item_id: Uuid,
}

#[derive(Debug, Clone)]
struct SyncPlayGroup {
    group_id: Uuid,
    group_name: String,
    state: GroupStateType,
    participants: HashMap<String, GroupParticipant>,
    playlist: Vec<SyncPlayQueueItem>,
    playing_item_index: Option<usize>,
    start_position_ticks: i64,
    is_playing: bool,
    shuffle_mode: GroupShuffleMode,
    repeat_mode: GroupRepeatMode,
    waiting_resume_playing: bool,
    last_updated_at: DateTime<Utc>,
}

impl SyncPlayGroup {
    fn to_group_info(&self) -> GroupInfoDto {
        let mut unique_names = HashSet::new();
        let mut participants = Vec::new();
        for p in self.participants.values() {
            if unique_names.insert(p.user_name.clone()) {
                participants.push(p.user_name.clone());
            }
        }
        GroupInfoDto {
            group_id: self.group_id,
            group_name: self.group_name.clone(),
            state: self.state.clone(),
            participants,
            last_updated_at: self.last_updated_at,
        }
    }

    fn current_playlist_item_id(&self) -> Uuid {
        self.playing_item_index
            .and_then(|idx| self.playlist.get(idx))
            .map(|item| item.playlist_item_id)
            .unwrap_or_else(Uuid::nil)
    }

    fn touch(&mut self) {
        self.last_updated_at = Utc::now();
    }

    fn highest_ping_ms(&self) -> i64 {
        self.participants
            .values()
            .map(|p| p.ping as i64)
            .max()
            .unwrap_or(DEFAULT_PING_MS)
    }

    fn all_ready(&self) -> bool {
        self.participants
            .values()
            .all(|p| !p.is_buffering || p.ignore_wait)
    }
}

#[derive(Default)]
struct SyncPlayState {
    groups: HashMap<Uuid, SyncPlayGroup>,
    session_to_group: HashMap<String, Uuid>,
    ws_connections: HashMap<String, mpsc::UnboundedSender<String>>,
}

#[derive(Clone, Default)]
pub struct SyncPlayService {
    state: Arc<RwLock<SyncPlayState>>,
}

#[derive(Debug, Clone)]
struct SessionContext {
    user: User,
    session_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct OutboundWebSocketMessage<T: Serialize> {
    message_type: &'static str,
    message_id: Uuid,
    data: T,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct GroupUpdateEnvelope {
    group_id: Uuid,
    #[serde(rename = "Type")]
    update_type: GroupUpdateType,
    data: Value,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct SendCommandEnvelope {
    group_id: Uuid,
    playlist_item_id: Uuid,
    when: DateTime<Utc>,
    position_ticks: Option<i64>,
    command: SendCommandType,
    emitted_at: DateTime<Utc>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct GroupStateUpdate {
    state: GroupStateType,
    reason: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct PlayQueueUpdate {
    reason: PlayQueueUpdateReason,
    last_update: DateTime<Utc>,
    playlist: Vec<SyncPlayQueueItem>,
    playing_item_index: usize,
    start_position_ticks: i64,
    is_playing: bool,
    shuffle_mode: GroupShuffleMode,
    repeat_mode: GroupRepeatMode,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InboundWebSocketMessage {
    message_type: String,
    data: Option<Value>,
}

impl SyncPlayService {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(SyncPlayState::default())),
        }
    }

    async fn register_websocket(&self, session_id: String, tx: mpsc::UnboundedSender<String>) {
        let mut state = self.state.write().await;
        state.ws_connections.insert(session_id.clone(), tx);
        Self::send_to_session_locked(&mut state, &session_id, "ForceKeepAlive", &15_u64);
    }

    async fn unregister_websocket_and_leave(&self, session_id: &str) {
        let mut state = self.state.write().await;
        state.ws_connections.remove(session_id);
        Self::leave_locked(&mut state, session_id);
    }

    async fn send_keepalive(&self, session_id: &str) {
        let mut state = self.state.write().await;
        Self::send_to_session_locked(&mut state, session_id, "KeepAlive", &Value::Null);
    }

    fn send_to_session_locked<T: Serialize>(
        state: &mut SyncPlayState,
        session_id: &str,
        message_type: &'static str,
        data: &T,
    ) {
        let Some(tx) = state.ws_connections.get(session_id).cloned() else {
            return;
        };

        let payload = OutboundWebSocketMessage {
            message_type,
            message_id: Uuid::new_v4(),
            data,
        };
        if let Ok(text) = serde_json::to_string(&payload) {
            if tx.send(text).is_err() {
                state.ws_connections.remove(session_id);
            }
        }
    }

    fn send_group_update_to_sessions_locked(
        state: &mut SyncPlayState,
        session_ids: impl IntoIterator<Item = String>,
        group_id: Uuid,
        update_type: GroupUpdateType,
        data: Value,
    ) {
        let msg = GroupUpdateEnvelope {
            group_id,
            update_type,
            data,
        };
        for session_id in session_ids {
            Self::send_to_session_locked(state, &session_id, "SyncPlayGroupUpdate", &msg);
        }
    }

    fn send_command_to_group_locked(
        state: &mut SyncPlayState,
        group: &SyncPlayGroup,
        command: SendCommandType,
        position_ticks: Option<i64>,
        delay_ms: i64,
    ) {
        let when = Utc::now() + chrono::Duration::milliseconds(delay_ms.max(0));
        let cmd = SendCommandEnvelope {
            group_id: group.group_id,
            playlist_item_id: group.current_playlist_item_id(),
            when,
            position_ticks,
            command,
            emitted_at: Utc::now(),
        };
        let recipients = group.participants.keys().cloned().collect::<Vec<_>>();
        for session_id in recipients {
            Self::send_to_session_locked(state, &session_id, "SyncPlayCommand", &cmd);
        }
    }

    fn send_play_queue_update_locked(
        state: &mut SyncPlayState,
        group: &SyncPlayGroup,
        reason: PlayQueueUpdateReason,
    ) {
        let recipients = group.participants.keys().cloned().collect::<Vec<_>>();
        Self::send_play_queue_update_to_sessions_locked(state, group, reason, recipients);
    }

    fn send_play_queue_update_to_sessions_locked(
        state: &mut SyncPlayState,
        group: &SyncPlayGroup,
        reason: PlayQueueUpdateReason,
        recipients: Vec<String>,
    ) {
        let update = PlayQueueUpdate {
            reason,
            last_update: group.last_updated_at,
            playlist: group.playlist.clone(),
            playing_item_index: group.playing_item_index.unwrap_or(0),
            start_position_ticks: group.start_position_ticks,
            is_playing: group.is_playing,
            shuffle_mode: group.shuffle_mode,
            repeat_mode: group.repeat_mode,
        };
        Self::send_group_update_to_sessions_locked(
            state,
            recipients,
            group.group_id,
            GroupUpdateType::PlayQueue,
            serde_json::to_value(update).unwrap_or(Value::Null),
        );
    }

    fn send_library_access_denied_to_session_locked(state: &mut SyncPlayState, session_id: &str) {
        Self::send_group_update_to_sessions_locked(
            state,
            vec![session_id.to_string()],
            Uuid::nil(),
            GroupUpdateType::LibraryAccessDenied,
            Value::String(String::new()),
        );
    }

    fn resolve_waiting_state_locked(
        state: &mut SyncPlayState,
        group: &mut SyncPlayGroup,
        reason: &'static str,
    ) {
        if group.state != GroupStateType::Waiting || !group.all_ready() {
            return;
        }

        if group.waiting_resume_playing {
            let delay_ms = std::cmp::max(group.highest_ping_ms() * 2, DEFAULT_PING_MS);
            group.state = GroupStateType::Playing;
            group.is_playing = true;
            group.touch();
            Self::send_command_to_group_locked(
                state,
                group,
                SendCommandType::Unpause,
                Some(group.start_position_ticks),
                delay_ms,
            );
            Self::send_state_update_locked(state, group, reason);
            return;
        }

        group.state = GroupStateType::Paused;
        group.is_playing = false;
        group.touch();
        Self::send_command_to_group_locked(
            state,
            group,
            SendCommandType::Pause,
            Some(group.start_position_ticks),
            0,
        );
        Self::send_state_update_locked(state, group, reason);
    }

    fn send_state_update_locked(
        state: &mut SyncPlayState,
        group: &SyncPlayGroup,
        reason: &'static str,
    ) {
        let update = GroupStateUpdate {
            state: group.state.clone(),
            reason,
        };
        let recipients = group.participants.keys().cloned().collect::<Vec<_>>();
        Self::send_group_update_to_sessions_locked(
            state,
            recipients,
            group.group_id,
            GroupUpdateType::StateUpdate,
            serde_json::to_value(update).unwrap_or(Value::Null),
        );
    }

    async fn create_group(&self, session: &SessionContext, group_name: String) -> GroupInfoDto {
        let group_id = Uuid::new_v4();
        let mut state = self.state.write().await;
        Self::leave_locked(&mut state, &session.session_id);

        let participant = GroupParticipant {
            user_name: session.user.original_username.clone(),
            ping: DEFAULT_PING_MS as u64,
            is_buffering: false,
            ignore_wait: false,
        };

        let group = SyncPlayGroup {
            group_id,
            group_name,
            state: GroupStateType::Idle,
            participants: HashMap::from([(session.session_id.clone(), participant)]),
            playlist: Vec::new(),
            playing_item_index: None,
            start_position_ticks: 0,
            is_playing: false,
            shuffle_mode: GroupShuffleMode::Sorted,
            repeat_mode: GroupRepeatMode::RepeatNone,
            waiting_resume_playing: false,
            last_updated_at: Utc::now(),
        };

        state
            .session_to_group
            .insert(session.session_id.clone(), group_id);
        state.groups.insert(group_id, group.clone());

        Self::send_group_update_to_sessions_locked(
            &mut state,
            vec![session.session_id.clone()],
            group_id,
            GroupUpdateType::GroupJoined,
            serde_json::to_value(group.to_group_info()).unwrap_or(Value::Null),
        );

        group.to_group_info()
    }

    async fn join_group(&self, session: &SessionContext, group_id: Uuid) {
        let mut state = self.state.write().await;
        Self::leave_locked(&mut state, &session.session_id);

        let Some(mut group) = state.groups.remove(&group_id) else {
            Self::send_group_update_to_sessions_locked(
                &mut state,
                vec![session.session_id.clone()],
                Uuid::nil(),
                GroupUpdateType::GroupDoesNotExist,
                Value::String(String::new()),
            );
            return;
        };

        group.participants.insert(
            session.session_id.clone(),
            GroupParticipant {
                user_name: session.user.original_username.clone(),
                ping: DEFAULT_PING_MS as u64,
                is_buffering: !group.playlist.is_empty(),
                ignore_wait: false,
            },
        );
        if !group.playlist.is_empty() {
            group.state = GroupStateType::Waiting;
            group.waiting_resume_playing = group.is_playing;
        }
        group.touch();
        state
            .session_to_group
            .insert(session.session_id.clone(), group_id);

        let username = session.user.original_username.clone();
        let recipients = group.participants.keys().cloned().collect::<Vec<_>>();
        Self::send_group_update_to_sessions_locked(
            &mut state,
            recipients,
            group_id,
            GroupUpdateType::UserJoined,
            Value::String(username),
        );
        Self::send_group_update_to_sessions_locked(
            &mut state,
            vec![session.session_id.clone()],
            group_id,
            GroupUpdateType::GroupJoined,
            serde_json::to_value(group.to_group_info()).unwrap_or(Value::Null),
        );
        Self::send_play_queue_update_to_sessions_locked(
            &mut state,
            &group,
            PlayQueueUpdateReason::NewPlaylist,
            vec![session.session_id.clone()],
        );
        if group.state == GroupStateType::Waiting {
            Self::send_state_update_locked(&mut state, &group, "Buffer");
        }

        state.groups.insert(group_id, group);
    }

    fn leave_locked(state: &mut SyncPlayState, session_id: &str) {
        let Some(group_id) = state.session_to_group.remove(session_id) else {
            return;
        };

        if let Some(mut group) = state.groups.remove(&group_id) {
            let username = group
                .participants
                .get(session_id)
                .map(|p| p.user_name.clone())
                .unwrap_or_default();
            group.participants.remove(session_id);
            group.touch();
            let recipients = group.participants.keys().cloned().collect::<Vec<_>>();
            Self::send_group_update_to_sessions_locked(
                state,
                recipients,
                group_id,
                GroupUpdateType::UserLeft,
                Value::String(username),
            );
            Self::send_group_update_to_sessions_locked(
                state,
                vec![session_id.to_string()],
                group_id,
                GroupUpdateType::GroupLeft,
                Value::String(group_id.to_string()),
            );
            if !group.participants.is_empty() {
                state.groups.insert(group_id, group);
            }
        }
    }

    async fn leave_group(&self, session: &SessionContext) {
        let mut state = self.state.write().await;
        Self::leave_locked(&mut state, &session.session_id);
    }

    async fn list_groups(&self) -> Vec<GroupInfoDto> {
        let state = self.state.read().await;
        state
            .groups
            .values()
            .map(SyncPlayGroup::to_group_info)
            .collect()
    }

    async fn get_group(&self, group_id: Uuid) -> Option<GroupInfoDto> {
        let state = self.state.read().await;
        state
            .groups
            .get(&group_id)
            .map(SyncPlayGroup::to_group_info)
    }

    async fn with_group_for_session(
        &self,
        session: &SessionContext,
        f: impl FnOnce(&mut SyncPlayGroup, &mut SyncPlayState),
    ) -> bool {
        let mut state = self.state.write().await;
        let Some(group_id) = state.session_to_group.get(&session.session_id).copied() else {
            Self::send_group_update_to_sessions_locked(
                &mut state,
                vec![session.session_id.clone()],
                Uuid::nil(),
                GroupUpdateType::NotInGroup,
                Value::String(String::new()),
            );
            return false;
        };

        let mut group = match state.groups.remove(&group_id) {
            Some(g) => g,
            None => {
                Self::send_group_update_to_sessions_locked(
                    &mut state,
                    vec![session.session_id.clone()],
                    Uuid::nil(),
                    GroupUpdateType::NotInGroup,
                    Value::String(String::new()),
                );
                return false;
            }
        };

        f(&mut group, &mut state);
        state.groups.insert(group_id, group);
        true
    }

    async fn get_group_snapshot_by_id(&self, group_id: Uuid) -> Option<SyncPlayGroup> {
        let state = self.state.read().await;
        state.groups.get(&group_id).cloned()
    }

    async fn get_group_snapshot_for_session(&self, session_id: &str) -> Option<SyncPlayGroup> {
        let state = self.state.read().await;
        let group_id = state.session_to_group.get(session_id).copied()?;
        state.groups.get(&group_id).cloned()
    }

    async fn send_library_access_denied_to_session(&self, session_id: &str) {
        let mut state = self.state.write().await;
        Self::send_library_access_denied_to_session_locked(&mut state, session_id);
    }
}

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

async fn group_has_library_access(
    state: &AppState,
    group: &SyncPlayGroup,
    item_ids: &[String],
) -> Result<bool, StatusCode> {
    if item_ids.is_empty() {
        return Ok(true);
    }

    let mut user_server_urls: HashMap<String, HashSet<String>> = HashMap::new();
    for session_id in group.participants.keys() {
        let Some(user_id) = user_id_from_session_id(session_id) else {
            return Ok(false);
        };

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
        user_server_urls.insert(user_id.to_string(), urls);
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

    if let Some(group) = state
        .syncplay
        .get_group_snapshot_by_id(payload.group_id)
        .await
    {
        let queue_item_ids = group
            .playlist
            .iter()
            .map(|item| item.item_id.clone())
            .collect::<Vec<_>>();
        let mut join_preview = group.clone();
        join_preview.participants.insert(
            session.session_id.clone(),
            GroupParticipant {
                user_name: session.user.original_username.clone(),
                ping: DEFAULT_PING_MS as u64,
                is_buffering: false,
                ignore_wait: false,
            },
        );
        if !group_has_library_access(&state, &join_preview, &queue_item_ids).await? {
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
            group.state = GroupStateType::Playing;
            group.is_playing = true;
            group.waiting_resume_playing = true;
            group.touch();
            SyncPlayService::send_command_to_group_locked(
                sync_state,
                group,
                SendCommandType::Unpause,
                Some(group.start_position_ticks),
                std::cmp::max(group.highest_ping_ms() * 2, DEFAULT_PING_MS),
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
            group.state = GroupStateType::Idle;
            group.is_playing = false;
            group.waiting_resume_playing = false;
            group.start_position_ticks = 0;
            group.touch();
            SyncPlayService::send_command_to_group_locked(
                sync_state,
                group,
                SendCommandType::Stop,
                None,
                0,
            );
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
            if let Some(current) = group
                .playing_item_index
                .and_then(|idx| group.playlist.get(idx))
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
            if let Some(current) = group
                .playing_item_index
                .and_then(|idx| group.playlist.get(idx))
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
                .and_then(|id| {
                    group
                        .playlist
                        .iter()
                        .position(|item| item.playlist_item_id == id)
                })
                .or(group.playing_item_index)
                .unwrap_or(0);
            let next = (current + 1) % group.playlist.len();
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
                .and_then(|id| {
                    group
                        .playlist
                        .iter()
                        .position(|item| item.playlist_item_id == id)
                })
                .or(group.playing_item_index)
                .unwrap_or(0);
            let prev = if current == 0 {
                group.playlist.len() - 1
            } else {
                current - 1
            };
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encryption::HashedPassword;
    use chrono::DateTime;
    use tokio::time::{timeout, Duration};

    fn make_user(id: &str, name: &str) -> User {
        User {
            id: id.to_string(),
            virtual_key: format!("vk-{id}"),
            original_username: name.to_string(),
            original_password_hash: HashedPassword::from_hashed("0".repeat(64)),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn make_session(user: User, device: &str) -> SessionContext {
        SessionContext {
            session_id: format!("{}:{}", user.id, device),
            user,
        }
    }

    async fn recv_json(rx: &mut mpsc::UnboundedReceiver<String>) -> serde_json::Value {
        let msg = timeout(Duration::from_millis(1000), rx.recv())
            .await
            .expect("timed out waiting for websocket message")
            .expect("channel closed unexpectedly");
        serde_json::from_str(&msg).expect("invalid json message")
    }

    async fn recv_many(
        rx: &mut mpsc::UnboundedReceiver<String>,
        max: usize,
    ) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        for _ in 0..max {
            match timeout(Duration::from_millis(150), rx.recv()).await {
                Ok(Some(msg)) => {
                    out.push(serde_json::from_str(&msg).expect("invalid json message"))
                }
                _ => break,
            }
        }
        out
    }

    #[tokio::test]
    async fn test_group_lifecycle_uses_jellyswarrm_users() {
        let service = SyncPlayService::new();
        let s1 = make_session(make_user("u1", "alice"), "web");
        let s2 = make_session(make_user("u2", "bob"), "tv");

        let group = service.create_group(&s1, "party".to_string()).await;
        assert_eq!(group.participants, vec!["alice".to_string()]);

        service.join_group(&s2, group.group_id).await;
        let joined = service
            .get_group(group.group_id)
            .await
            .expect("group missing");
        assert!(joined.participants.contains(&"alice".to_string()));
        assert!(joined.participants.contains(&"bob".to_string()));

        service.leave_group(&s1).await;
        let after_leave = service
            .get_group(group.group_id)
            .await
            .expect("group missing");
        assert_eq!(after_leave.participants, vec!["bob".to_string()]);

        service.leave_group(&s2).await;
        assert!(service.get_group(group.group_id).await.is_none());
    }

    #[tokio::test]
    async fn test_queue_media_ids_and_playlist_ids() {
        let service = SyncPlayService::new();
        let s1 = make_session(make_user("u1", "alice"), "web");
        let group = service.create_group(&s1, "party".to_string()).await;

        let ok = service
            .with_group_for_session(&s1, |g, _| {
                g.playlist = vec![
                    SyncPlayQueueItem {
                        item_id: "virtual-media-a".to_string(),
                        playlist_item_id: Uuid::new_v4(),
                    },
                    SyncPlayQueueItem {
                        item_id: "virtual-media-b".to_string(),
                        playlist_item_id: Uuid::new_v4(),
                    },
                ];
                g.playing_item_index = Some(0);
            })
            .await;
        assert!(ok);

        let state = service.state.read().await;
        let g = state.groups.get(&group.group_id).expect("group missing");
        assert_eq!(g.playlist[0].item_id, "virtual-media-a");
        assert_eq!(g.playlist[1].item_id, "virtual-media-b");
        assert_ne!(g.playlist[0].playlist_item_id, Uuid::nil());
        assert_ne!(g.playlist[1].playlist_item_id, Uuid::nil());
        assert_ne!(
            g.playlist[0].playlist_item_id,
            g.playlist[1].playlist_item_id
        );
    }

    #[tokio::test]
    async fn test_websocket_envelopes_for_commands_and_updates() {
        let service = SyncPlayService::new();
        let s1 = make_session(make_user("u1", "alice"), "web");
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();

        service.register_websocket(s1.session_id.clone(), tx).await;
        let keepalive = recv_json(&mut rx).await;
        assert_eq!(keepalive["MessageType"], "ForceKeepAlive");

        let group = service.create_group(&s1, "party".to_string()).await;
        let joined = recv_json(&mut rx).await;
        assert_eq!(joined["MessageType"], "SyncPlayGroupUpdate");
        assert_eq!(joined["Data"]["Type"], "GroupJoined");

        let ok = service
            .with_group_for_session(&s1, |g, sync_state| {
                g.playlist = vec![SyncPlayQueueItem {
                    item_id: "virtual-media-a".to_string(),
                    playlist_item_id: Uuid::new_v4(),
                }];
                g.playing_item_index = Some(0);
                g.start_position_ticks = 123;
                g.state = GroupStateType::Paused;
                g.touch();
                SyncPlayService::send_command_to_group_locked(
                    sync_state,
                    g,
                    SendCommandType::Pause,
                    Some(123),
                    0,
                );
                SyncPlayService::send_state_update_locked(sync_state, g, "Pause");
            })
            .await;
        assert!(ok);

        let m1 = recv_json(&mut rx).await;
        let m2 = recv_json(&mut rx).await;
        let messages = vec![m1, m2];
        assert!(messages
            .iter()
            .any(|m| m["MessageType"] == "SyncPlayCommand"
                && m["Data"]["Command"] == "Pause"
                && m["Data"]["PositionTicks"] == 123));
        assert!(messages.iter().any(
            |m| m["MessageType"] == "SyncPlayGroupUpdate" && m["Data"]["Type"] == "StateUpdate"
        ));

        assert!(service.get_group(group.group_id).await.is_some());
    }

    #[tokio::test]
    async fn test_not_in_group_update_is_emitted() {
        let service = SyncPlayService::new();
        let s1 = make_session(make_user("u1", "alice"), "web");
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();

        service.register_websocket(s1.session_id.clone(), tx).await;
        let _ = recv_json(&mut rx).await;

        let ok = service.with_group_for_session(&s1, |_g, _| {}).await;
        assert!(!ok);

        let not_in_group = recv_json(&mut rx).await;
        assert_eq!(not_in_group["MessageType"], "SyncPlayGroupUpdate");
        assert_eq!(not_in_group["Data"]["Type"], "NotInGroup");
    }

    #[tokio::test]
    async fn test_websocket_disconnect_removes_participant() {
        let service = SyncPlayService::new();
        let s1 = make_session(make_user("u1", "alice"), "web");
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();

        service.register_websocket(s1.session_id.clone(), tx).await;
        let _ = recv_json(&mut rx).await;

        let group = service.create_group(&s1, "party".to_string()).await;
        let _ = recv_json(&mut rx).await;

        service.unregister_websocket_and_leave(&s1.session_id).await;
        assert!(service.get_group(group.group_id).await.is_none());
    }

    #[tokio::test]
    async fn test_waiting_ready_all_ready_emits_delayed_unpause() {
        let service = SyncPlayService::new();
        let s1 = make_session(make_user("u1", "alice"), "web");
        let s2 = make_session(make_user("u2", "bob"), "tv");

        let (tx1, mut rx1) = mpsc::unbounded_channel::<String>();
        let (tx2, mut rx2) = mpsc::unbounded_channel::<String>();
        service.register_websocket(s1.session_id.clone(), tx1).await;
        service.register_websocket(s2.session_id.clone(), tx2).await;
        let _ = recv_json(&mut rx1).await;
        let _ = recv_json(&mut rx2).await;

        let _ = service.create_group(&s1, "party".to_string()).await;
        let _ = recv_json(&mut rx1).await;
        service
            .join_group(&s2, service.list_groups().await[0].group_id)
            .await;
        let _ = recv_many(&mut rx1, 8).await;
        let _ = recv_many(&mut rx2, 8).await;

        let _ = service
            .with_group_for_session(&s1, |g, _| {
                g.playlist = vec![SyncPlayQueueItem {
                    item_id: "virtual-media-a".to_string(),
                    playlist_item_id: Uuid::new_v4(),
                }];
                g.playing_item_index = Some(0);
                g.state = GroupStateType::Waiting;
                g.waiting_resume_playing = true;
                g.start_position_ticks = 777;
                for p in g.participants.values_mut() {
                    p.is_buffering = true;
                }
                if let Some(p) = g.participants.get_mut(&s1.session_id) {
                    p.ping = 50;
                }
                if let Some(p) = g.participants.get_mut(&s2.session_id) {
                    p.ping = 300;
                }
            })
            .await;

        let _ = service
            .with_group_for_session(&s1, |g, sync_state| {
                g.participants.get_mut(&s1.session_id).unwrap().is_buffering = false;
                SyncPlayService::resolve_waiting_state_locked(sync_state, g, "Ready");
            })
            .await;

        let _ = service
            .with_group_for_session(&s2, |g, sync_state| {
                g.participants.get_mut(&s2.session_id).unwrap().is_buffering = false;
                SyncPlayService::resolve_waiting_state_locked(sync_state, g, "Ready");
            })
            .await;

        let msgs = recv_many(&mut rx1, 8).await;
        let cmd = msgs
            .iter()
            .find(|m| m["MessageType"] == "SyncPlayCommand" && m["Data"]["Command"] == "Unpause")
            .expect("expected unpause command");
        let when = DateTime::parse_from_rfc3339(cmd["Data"]["When"].as_str().unwrap())
            .expect("invalid When timestamp");
        let emitted = DateTime::parse_from_rfc3339(cmd["Data"]["EmittedAt"].as_str().unwrap())
            .expect("invalid EmittedAt timestamp");
        let delay_ms = (when - emitted).num_milliseconds();
        assert!(
            delay_ms >= 500,
            "expected delayed unpause, got {delay_ms}ms"
        );
    }

    #[tokio::test]
    async fn test_join_sends_queue_snapshot_to_joiner() {
        let service = SyncPlayService::new();
        let s1 = make_session(make_user("u1", "alice"), "web");
        let s2 = make_session(make_user("u2", "bob"), "tv");

        let (tx1, mut rx1) = mpsc::unbounded_channel::<String>();
        let (tx2, mut rx2) = mpsc::unbounded_channel::<String>();
        service.register_websocket(s1.session_id.clone(), tx1).await;
        service.register_websocket(s2.session_id.clone(), tx2).await;
        let _ = recv_json(&mut rx1).await;
        let _ = recv_json(&mut rx2).await;

        let group = service.create_group(&s1, "party".to_string()).await;
        let _ = recv_json(&mut rx1).await;

        let _ = service
            .with_group_for_session(&s1, |g, _| {
                g.playlist = vec![SyncPlayQueueItem {
                    item_id: "virtual-media-a".to_string(),
                    playlist_item_id: Uuid::new_v4(),
                }];
                g.playing_item_index = Some(0);
            })
            .await;

        service.join_group(&s2, group.group_id).await;
        let msgs = recv_many(&mut rx2, 8).await;
        assert!(msgs
            .iter()
            .any(|m| m["MessageType"] == "SyncPlayGroupUpdate"
                && m["Data"]["Type"] == "PlayQueue"
                && m["Data"]["Data"]["Reason"] == "NewPlaylist"));

        let _ = recv_many(&mut rx1, 8).await;
    }
}
