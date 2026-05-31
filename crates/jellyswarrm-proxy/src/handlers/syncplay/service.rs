use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{mpsc, RwLock};
use tokio::time::{sleep, Duration};
use tracing::{debug, info};
use uuid::Uuid;

use crate::user_authorization_service::User;

use super::models::{
    GroupInfoDto, GroupParticipant, GroupStateType, GroupStateUpdate, GroupUpdateEnvelope,
    GroupUpdateType, OutboundWebSocketMessage, PlayQueueUpdate, PlayQueueUpdateReason,
    SendCommandEnvelope, SendCommandType, SyncPlayGroup,
};

pub(crate) const DEFAULT_PING_MS: i64 = 500;
const WEBSOCKET_DISCONNECT_GRACE: Duration = Duration::from_secs(30);

#[derive(Default)]
pub(super) struct SyncPlayState {
    groups: HashMap<Uuid, SyncPlayGroup>,
    session_to_group: HashMap<String, Uuid>,
    ws_connections: HashMap<String, mpsc::UnboundedSender<String>>,
    ws_connection_ids: HashMap<String, Uuid>,
    disconnect_grace_ids: HashMap<String, Uuid>,
}

impl SyncPlayState {
    /// Send a raw websocket message to a single session.
    fn send_to_session<T: Serialize>(
        &mut self,
        session_id: &str,
        msg_type: &'static str,
        data: &T,
    ) {
        let Some(tx) = self.ws_connections.get(session_id).cloned() else {
            return;
        };
        let payload = OutboundWebSocketMessage {
            message_type: msg_type,
            message_id: Uuid::new_v4(),
            data,
        };
        if let Ok(text) = serde_json::to_string(&payload) {
            if tx.send(text).is_err() {
                self.ws_connections.remove(session_id);
                self.ws_connection_ids.remove(session_id);
            }
        }
    }

    /// Send a group-update envelope to specific sessions.
    pub(super) fn send_group_update(
        &mut self,
        sessions: impl IntoIterator<Item = String>,
        group_id: Uuid,
        update_type: GroupUpdateType,
        data: Value,
    ) {
        let msg = GroupUpdateEnvelope {
            group_id,
            update_type,
            data,
        };
        for session_id in sessions {
            self.send_to_session(&session_id, "SyncPlayGroupUpdate", &msg);
        }
    }

    /// Broadcast a group-update to all participants in a group.
    fn broadcast_group_update(
        &mut self,
        group: &SyncPlayGroup,
        update_type: GroupUpdateType,
        data: Value,
    ) {
        let sessions: Vec<_> = group.participants.keys().cloned().collect();
        self.send_group_update(sessions, group.group_id, update_type, data);
    }

    /// Send a one-shot notification (empty data) to a single session.
    pub(super) fn notify_session(&mut self, session_id: &str, update_type: GroupUpdateType) {
        self.send_group_update(
            std::iter::once(session_id.to_string()),
            Uuid::nil(),
            update_type,
            Value::String(String::new()),
        );
    }

    /// Send a one-shot notification scoped to a known group.
    pub(super) fn notify_session_for_group(
        &mut self,
        session_id: &str,
        group_id: Uuid,
        update_type: GroupUpdateType,
    ) {
        self.send_group_update(
            std::iter::once(session_id.to_string()),
            group_id,
            update_type,
            Value::String(String::new()),
        );
    }

    /// Broadcast a playback command to all group participants.
    pub(super) fn broadcast_command(
        &mut self,
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
        for session_id in group.participants.keys() {
            self.send_to_session(session_id, "SyncPlayCommand", &cmd);
        }
    }

    /// Broadcast a state update to all group participants.
    pub(super) fn broadcast_state_update(&mut self, group: &SyncPlayGroup, reason: &'static str) {
        let update = GroupStateUpdate {
            state: group.state.clone(),
            reason,
        };
        self.broadcast_group_update(
            group,
            GroupUpdateType::StateUpdate,
            serde_json::to_value(update).unwrap_or(Value::Null),
        );
    }

    /// Broadcast a play-queue update to all group participants.
    pub(super) fn broadcast_queue_update(
        &mut self,
        group: &SyncPlayGroup,
        reason: PlayQueueUpdateReason,
    ) {
        let sessions: Vec<_> = group.participants.keys().cloned().collect();
        self.send_queue_update_to(group, reason, sessions);
    }

    /// Send a play-queue update to specific sessions.
    pub(super) fn send_queue_update_to(
        &mut self,
        group: &SyncPlayGroup,
        reason: PlayQueueUpdateReason,
        sessions: Vec<String>,
    ) {
        let playing_item_index = group
            .playing_item_index
            .filter(|index| *index < group.playlist.len())
            .map(|index| index as i64)
            .unwrap_or(-1);

        let update = PlayQueueUpdate {
            reason,
            last_update: group.last_updated_at,
            playlist: group.playlist.clone(),
            playing_item_index,
            start_position_ticks: group.start_position_ticks,
            is_playing: group.is_playing,
            shuffle_mode: group.shuffle_mode,
            repeat_mode: group.repeat_mode,
        };
        self.send_group_update(
            sessions,
            group.group_id,
            GroupUpdateType::PlayQueue,
            serde_json::to_value(update).unwrap_or(Value::Null),
        );
    }

    /// Resolve the Waiting state: if all participants are ready, transition to
    /// Playing (with delayed unpause) or Paused.
    pub(super) fn resolve_waiting(&mut self, group: &mut SyncPlayGroup, reason: &'static str) {
        if group.state != GroupStateType::Waiting || !group.all_ready() {
            return;
        }
        let now = Utc::now();
        let (cmd, delay_ms) = if group.waiting_resume_playing {
            group.state = GroupStateType::Playing;
            group.is_playing = true;
            (SendCommandType::Unpause, group.ping_delay_ms())
        } else {
            group.state = GroupStateType::Paused;
            group.is_playing = false;
            (SendCommandType::Pause, 0)
        };
        if let Some(position_ticks) = group.pending_position_ticks.take() {
            group.set_position(position_ticks, now);
        }
        if group.is_playing {
            group.position_base_when = now + chrono::Duration::milliseconds(delay_ms.max(0));
        } else {
            group.position_base_when = now;
        }
        group.touch();
        self.broadcast_command(group, cmd, Some(group.start_position_ticks), delay_ms);
        self.broadcast_state_update(group, reason);
    }
}

#[derive(Clone, Default)]
pub struct SyncPlayService {
    state: Arc<RwLock<SyncPlayState>>,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionContext {
    pub user: User,
    pub session_id: String,
}

impl SyncPlayGroup {
    fn all_ready(&self) -> bool {
        self.participants
            .values()
            .all(|p| !p.is_connected || !p.is_buffering || p.ignore_wait)
    }
}

impl SyncPlayService {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(SyncPlayState::default())),
        }
    }

    pub(super) async fn register_websocket(
        &self,
        session_id: String,
        tx: mpsc::UnboundedSender<String>,
    ) -> Uuid {
        let mut state = self.state.write().await;
        let connection_id = Uuid::new_v4();
        state.ws_connections.insert(session_id.clone(), tx);
        state
            .ws_connection_ids
            .insert(session_id.clone(), connection_id);
        state.disconnect_grace_ids.remove(&session_id);
        debug!(session_id = %session_id, "Registered SyncPlay websocket");
        state.send_to_session(&session_id, "ForceKeepAlive", &15_u64);

        if let Some(group_id) = state.session_to_group.get(&session_id).copied() {
            if let Some(mut group) = state.groups.remove(&group_id) {
                if let Some(member) = group.participants.get_mut(&session_id) {
                    member.is_connected = true;
                    member.is_buffering = !group.playlist.is_empty();
                    member.last_client_when = None;
                }
                if !group.playlist.is_empty() {
                    let now = Utc::now();
                    let resume_playing = group.is_playing;
                    let position_ticks = group.freeze_at_estimated_position(now);
                    group.pending_position_ticks = Some(position_ticks);
                    group.state = GroupStateType::Waiting;
                    group.waiting_resume_playing = resume_playing;
                }
                group.touch();
                state.send_group_update(
                    vec![session_id.clone()],
                    group_id,
                    GroupUpdateType::GroupJoined,
                    serde_json::to_value(group.to_group_info()).unwrap_or(Value::Null),
                );
                state.send_queue_update_to(
                    &group,
                    PlayQueueUpdateReason::NewPlaylist,
                    vec![session_id.clone()],
                );
                if group.state == GroupStateType::Waiting {
                    if group.waiting_resume_playing {
                        state.broadcast_command(
                            &group,
                            SendCommandType::Pause,
                            Some(group.start_position_ticks),
                            0,
                        );
                    }
                    state.broadcast_state_update(&group, "Buffer");
                }
                state.groups.insert(group_id, group);
            }
        }

        connection_id
    }

    pub(super) async fn unregister_websocket_with_grace(
        &self,
        session_id: String,
        connection_id: Uuid,
    ) {
        {
            let mut state = self.state.write().await;
            if state.ws_connection_ids.get(&session_id) != Some(&connection_id) {
                return;
            }
            state.ws_connection_ids.remove(&session_id);
            state.ws_connections.remove(&session_id);
            let disconnect_grace_id = Uuid::new_v4();
            state
                .disconnect_grace_ids
                .insert(session_id.clone(), disconnect_grace_id);
            if let Some(group_id) = state.session_to_group.get(&session_id).copied() {
                if let Some(mut group) = state.groups.remove(&group_id) {
                    if let Some(member) = group.participants.get_mut(&session_id) {
                        member.is_connected = false;
                    }
                    state.resolve_waiting(&mut group, "Disconnect");
                    state.groups.insert(group_id, group);
                }
            }
            debug!(session_id = %session_id, "Unregistered SyncPlay websocket with reconnect grace");

            let service = self.clone();
            tokio::spawn(async move {
                sleep(WEBSOCKET_DISCONNECT_GRACE).await;
                service
                    .leave_if_still_disconnected(&session_id, disconnect_grace_id)
                    .await;
            });
        }
    }

    async fn leave_if_still_disconnected(&self, session_id: &str, disconnect_grace_id: Uuid) {
        let mut state = self.state.write().await;
        if state.ws_connections.contains_key(session_id) {
            return;
        }
        if state.disconnect_grace_ids.get(session_id) != Some(&disconnect_grace_id) {
            return;
        }
        state.disconnect_grace_ids.remove(session_id);
        debug!(session_id = %session_id, "SyncPlay websocket grace expired; leaving group");
        Self::leave_locked(&mut state, session_id);
    }

    pub(super) async fn send_keepalive(&self, session_id: &str) {
        let mut state = self.state.write().await;
        state.send_to_session(session_id, "KeepAlive", &Value::Null);
    }

    fn make_participant(session: &SessionContext, is_buffering: bool) -> GroupParticipant {
        GroupParticipant {
            user_name: session.user.original_username.clone(),
            ping: DEFAULT_PING_MS as u64,
            is_buffering,
            ignore_wait: false,
            is_connected: true,
            last_client_when: None,
        }
    }

    pub(super) async fn create_group(
        &self,
        session: &SessionContext,
        group_name: String,
    ) -> Option<GroupInfoDto> {
        let group_id = Uuid::new_v4();
        let mut state = self.state.write().await;
        if !state.ws_connections.contains_key(&session.session_id) {
            return None;
        }
        Self::leave_locked(&mut state, &session.session_id);

        let participant = Self::make_participant(session, false);

        let group = SyncPlayGroup {
            group_id,
            group_name,
            state: GroupStateType::Idle,
            participants: HashMap::from([(session.session_id.clone(), participant)]),
            playlist: Vec::new(),
            playing_item_index: None,
            start_position_ticks: 0,
            pending_position_ticks: None,
            position_base_when: Utc::now(),
            is_playing: false,
            shuffle_mode: super::models::GroupShuffleMode::Sorted,
            repeat_mode: super::models::GroupRepeatMode::RepeatNone,
            waiting_resume_playing: false,
            last_updated_at: Utc::now(),
        };

        state
            .session_to_group
            .insert(session.session_id.clone(), group_id);
        state.groups.insert(group_id, group.clone());

        info!(
            group_id = %group_id,
            group_name = %group.group_name,
            user = %session.user.original_username,
            session_id = %session.session_id,
            "SyncPlay group created"
        );

        state.send_group_update(
            vec![session.session_id.clone()],
            group_id,
            GroupUpdateType::GroupJoined,
            serde_json::to_value(group.to_group_info()).unwrap_or(Value::Null),
        );

        Some(group.to_group_info())
    }

    pub(super) async fn join_group(
        &self,
        session: &SessionContext,
        group_id: Uuid,
        expected_queue_item_ids: Option<Vec<String>>,
    ) -> bool {
        let mut state = self.state.write().await;
        if !state.ws_connections.contains_key(&session.session_id) {
            return false;
        }
        Self::leave_locked(&mut state, &session.session_id);

        let Some(mut group) = state.groups.remove(&group_id) else {
            state.notify_session(&session.session_id, GroupUpdateType::GroupDoesNotExist);
            return true;
        };

        if expected_queue_item_ids
            .as_ref()
            .is_some_and(|expected| *expected != group.queue_item_ids())
        {
            state.groups.insert(group_id, group);
            state.notify_session(&session.session_id, GroupUpdateType::LibraryAccessDenied);
            return true;
        }

        group.participants.insert(
            session.session_id.clone(),
            Self::make_participant(session, !group.playlist.is_empty()),
        );
        if !group.playlist.is_empty() {
            let now = Utc::now();
            let resume_playing = group.is_playing;
            let position_ticks = group.freeze_at_estimated_position(now);
            group.pending_position_ticks = Some(position_ticks);
            group.state = GroupStateType::Waiting;
            group.waiting_resume_playing = resume_playing;
        }
        group.touch();
        state
            .session_to_group
            .insert(session.session_id.clone(), group_id);

        let username = session.user.original_username.clone();
        state.send_group_update(
            vec![session.session_id.clone()],
            group_id,
            GroupUpdateType::GroupJoined,
            serde_json::to_value(group.to_group_info()).unwrap_or(Value::Null),
        );
        state.send_queue_update_to(
            &group,
            PlayQueueUpdateReason::NewPlaylist,
            vec![session.session_id.clone()],
        );
        if group.state == GroupStateType::Waiting {
            if group.waiting_resume_playing {
                state.broadcast_command(
                    &group,
                    SendCommandType::Pause,
                    Some(group.start_position_ticks),
                    0,
                );
            }
            state.broadcast_state_update(&group, "Buffer");
        }

        let existing_members = group
            .participants
            .keys()
            .filter(|session_id| *session_id != &session.session_id)
            .cloned()
            .collect::<Vec<_>>();
        state.send_group_update(
            existing_members,
            group_id,
            GroupUpdateType::UserJoined,
            Value::String(username),
        );

        state.groups.insert(group_id, group);
        info!(
            group_id = %group_id,
            user = %session.user.original_username,
            session_id = %session.session_id,
            "SyncPlay member joined group"
        );
        true
    }

    fn leave_locked(state: &mut SyncPlayState, session_id: &str) {
        state.disconnect_grace_ids.remove(session_id);
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
            state.broadcast_group_update(
                &group,
                GroupUpdateType::UserLeft,
                Value::String(username.clone()),
            );
            state.send_group_update(
                std::iter::once(session_id.to_string()),
                group_id,
                GroupUpdateType::GroupLeft,
                Value::String(group_id.to_string()),
            );
            if !group.participants.is_empty() {
                state.resolve_waiting(&mut group, "Leave");
                state.groups.insert(group_id, group);
                info!(
                    group_id = %group_id,
                    user = %username,
                    session_id = %session_id,
                    "SyncPlay member left group"
                );
            } else {
                info!(
                    group_id = %group_id,
                    user = %username,
                    session_id = %session_id,
                    "SyncPlay member left and group was removed"
                );
            }
        }
    }

    pub(super) async fn leave_group(&self, session: &SessionContext) {
        let mut state = self.state.write().await;
        Self::leave_locked(&mut state, &session.session_id);
    }

    pub(super) async fn list_group_snapshots(&self) -> Vec<SyncPlayGroup> {
        let state = self.state.read().await;
        state.groups.values().cloned().collect()
    }

    pub(super) async fn with_group_for_session(
        &self,
        session: &SessionContext,
        f: impl FnOnce(&mut SyncPlayGroup, &mut SyncPlayState),
    ) -> bool {
        let mut state = self.state.write().await;
        let Some(group_id) = state.session_to_group.get(&session.session_id).copied() else {
            state.notify_session(&session.session_id, GroupUpdateType::NotInGroup);
            return false;
        };

        let mut group = match state.groups.remove(&group_id) {
            Some(g) => g,
            None => {
                state.notify_session(&session.session_id, GroupUpdateType::NotInGroup);
                return false;
            }
        };

        f(&mut group, &mut state);
        state.groups.insert(group_id, group);
        true
    }

    pub(super) async fn get_group_snapshot_by_id(&self, group_id: Uuid) -> Option<SyncPlayGroup> {
        let state = self.state.read().await;
        state.groups.get(&group_id).cloned()
    }

    pub(super) async fn get_group_snapshot_for_session(
        &self,
        session_id: &str,
    ) -> Option<SyncPlayGroup> {
        let state = self.state.read().await;
        let group_id = state.session_to_group.get(session_id).copied()?;
        state.groups.get(&group_id).cloned()
    }

    pub(super) async fn send_library_access_denied_to_session(&self, session_id: &str) {
        let mut state = self.state.write().await;
        state.notify_session(session_id, GroupUpdateType::LibraryAccessDenied);
    }
}

#[cfg(test)]
mod tests {
    use super::super::models::SyncPlayQueueItem;
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

        let (tx1, _rx1) = mpsc::unbounded_channel::<String>();
        let (tx2, _rx2) = mpsc::unbounded_channel::<String>();
        service.register_websocket(s1.session_id.clone(), tx1).await;
        service.register_websocket(s2.session_id.clone(), tx2).await;

        let group = service
            .create_group(&s1, "party".to_string())
            .await
            .expect("group creation should succeed");
        assert_eq!(group.participants, vec!["alice".to_string()]);

        service.join_group(&s2, group.group_id, None).await;
        let joined = service
            .get_group_snapshot_by_id(group.group_id)
            .await
            .expect("group missing")
            .to_group_info();
        assert!(joined.participants.contains(&"alice".to_string()));
        assert!(joined.participants.contains(&"bob".to_string()));

        service.leave_group(&s1).await;
        let after_leave = service
            .get_group_snapshot_by_id(group.group_id)
            .await
            .expect("group missing")
            .to_group_info();
        assert_eq!(after_leave.participants, vec!["bob".to_string()]);

        service.leave_group(&s2).await;
        assert!(service
            .get_group_snapshot_by_id(group.group_id)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn test_queue_media_ids_and_playlist_ids() {
        let service = SyncPlayService::new();
        let s1 = make_session(make_user("u1", "alice"), "web");
        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        service.register_websocket(s1.session_id.clone(), tx).await;
        let group = service
            .create_group(&s1, "party".to_string())
            .await
            .expect("group creation should succeed");

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

        let group = service
            .create_group(&s1, "party".to_string())
            .await
            .expect("group creation should succeed");
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
                sync_state.broadcast_command(g, SendCommandType::Pause, Some(123), 0);
                sync_state.broadcast_state_update(g, "Pause");
            })
            .await;
        assert!(ok);

        let m1 = recv_json(&mut rx).await;
        let m2 = recv_json(&mut rx).await;
        let messages = [m1, m2];
        assert!(messages
            .iter()
            .any(|m| m["MessageType"] == "SyncPlayCommand"
                && m["Data"]["Command"] == "Pause"
                && m["Data"]["PositionTicks"] == 123));
        assert!(messages.iter().any(
            |m| m["MessageType"] == "SyncPlayGroupUpdate" && m["Data"]["Type"] == "StateUpdate"
        ));

        assert!(service
            .get_group_snapshot_by_id(group.group_id)
            .await
            .is_some());
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
    async fn test_group_scoped_notification_uses_current_group_id() {
        let service = SyncPlayService::new();
        let s1 = make_session(make_user("u1", "alice"), "web");
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();

        service.register_websocket(s1.session_id.clone(), tx).await;
        let _ = recv_json(&mut rx).await;
        let group = service
            .create_group(&s1, "party".to_string())
            .await
            .expect("group creation should succeed");
        let _ = recv_json(&mut rx).await;

        let session_id = s1.session_id.clone();
        let ok = service
            .with_group_for_session(&s1, move |g, sync_state| {
                sync_state.notify_session_for_group(
                    &session_id,
                    g.group_id,
                    GroupUpdateType::LibraryAccessDenied,
                );
            })
            .await;
        assert!(ok);

        let access_denied = recv_json(&mut rx).await;
        assert_eq!(access_denied["MessageType"], "SyncPlayGroupUpdate");
        assert_eq!(access_denied["Data"]["Type"], "LibraryAccessDenied");
        assert_eq!(access_denied["Data"]["GroupId"], group.group_id.to_string());
    }

    #[tokio::test]
    async fn test_websocket_disconnect_keeps_group_during_grace() {
        let service = SyncPlayService::new();
        let s1 = make_session(make_user("u1", "alice"), "web");
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();

        let connection_id = service.register_websocket(s1.session_id.clone(), tx).await;
        let _ = recv_json(&mut rx).await;

        let group = service
            .create_group(&s1, "party".to_string())
            .await
            .expect("group creation should succeed");
        let _ = recv_json(&mut rx).await;

        service
            .unregister_websocket_with_grace(s1.session_id.clone(), connection_id)
            .await;
        assert!(service
            .get_group_snapshot_by_id(group.group_id)
            .await
            .is_some());
    }

    #[tokio::test]
    async fn test_stale_websocket_close_does_not_unregister_reconnect() {
        let service = SyncPlayService::new();
        let s1 = make_session(make_user("u1", "alice"), "web");
        let (tx1, mut rx1) = mpsc::unbounded_channel::<String>();
        let (tx2, mut rx2) = mpsc::unbounded_channel::<String>();

        let old_connection_id = service.register_websocket(s1.session_id.clone(), tx1).await;
        let _ = recv_json(&mut rx1).await;
        let group = service
            .create_group(&s1, "party".to_string())
            .await
            .expect("group creation should succeed");
        let _ = recv_json(&mut rx1).await;

        service.register_websocket(s1.session_id.clone(), tx2).await;
        service
            .unregister_websocket_with_grace(s1.session_id.clone(), old_connection_id)
            .await;
        service.send_keepalive(&s1.session_id).await;

        let messages = recv_many(&mut rx2, 6).await;
        assert!(messages.iter().any(|m| m["MessageType"] == "KeepAlive"));
        assert!(service
            .get_group_snapshot_by_id(group.group_id)
            .await
            .is_some());
    }

    #[tokio::test]
    async fn test_stale_disconnect_grace_does_not_remove_later_disconnect() {
        let service = SyncPlayService::new();
        let s1 = make_session(make_user("u1", "alice"), "web");
        let (tx1, mut rx1) = mpsc::unbounded_channel::<String>();
        let (tx2, mut rx2) = mpsc::unbounded_channel::<String>();

        let first_connection_id = service.register_websocket(s1.session_id.clone(), tx1).await;
        let _ = recv_json(&mut rx1).await;
        let group = service
            .create_group(&s1, "party".to_string())
            .await
            .expect("group creation should succeed");
        let _ = recv_json(&mut rx1).await;

        service
            .unregister_websocket_with_grace(s1.session_id.clone(), first_connection_id)
            .await;
        let first_grace_id = {
            let state = service.state.read().await;
            *state
                .disconnect_grace_ids
                .get(&s1.session_id)
                .expect("disconnect grace missing")
        };

        let second_connection_id = service.register_websocket(s1.session_id.clone(), tx2).await;
        let _ = recv_many(&mut rx2, 6).await;
        service
            .unregister_websocket_with_grace(s1.session_id.clone(), second_connection_id)
            .await;

        service
            .leave_if_still_disconnected(&s1.session_id, first_grace_id)
            .await;

        assert!(service
            .get_group_snapshot_by_id(group.group_id)
            .await
            .is_some());
    }

    #[tokio::test]
    async fn test_reconnect_receives_group_and_queue_snapshot() {
        let service = SyncPlayService::new();
        let s1 = make_session(make_user("u1", "alice"), "web");
        let (tx1, mut rx1) = mpsc::unbounded_channel::<String>();
        let (tx2, mut rx2) = mpsc::unbounded_channel::<String>();

        service.register_websocket(s1.session_id.clone(), tx1).await;
        let _ = recv_json(&mut rx1).await;
        service
            .create_group(&s1, "party".to_string())
            .await
            .expect("group creation should succeed");
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

        service.register_websocket(s1.session_id.clone(), tx2).await;

        let messages = recv_many(&mut rx2, 6).await;
        assert!(messages.iter().any(|m| {
            m["MessageType"] == "SyncPlayGroupUpdate" && m["Data"]["Type"] == "GroupJoined"
        }));
        assert!(messages.iter().any(|m| {
            m["MessageType"] == "SyncPlayGroupUpdate" && m["Data"]["Type"] == "PlayQueue"
        }));
    }

    #[tokio::test]
    async fn test_disconnect_resolves_waiting_when_remaining_members_ready() {
        let service = SyncPlayService::new();
        let s1 = make_session(make_user("u1", "alice"), "web");
        let s2 = make_session(make_user("u2", "bob"), "tv");
        let (tx1, mut rx1) = mpsc::unbounded_channel::<String>();
        let (tx2, mut rx2) = mpsc::unbounded_channel::<String>();

        let s1_connection_id = service.register_websocket(s1.session_id.clone(), tx1).await;
        service.register_websocket(s2.session_id.clone(), tx2).await;
        let _ = recv_json(&mut rx1).await;
        let _ = recv_json(&mut rx2).await;
        let group = service
            .create_group(&s1, "party".to_string())
            .await
            .expect("group creation should succeed");
        let _ = recv_json(&mut rx1).await;
        service.join_group(&s2, group.group_id, None).await;
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
                g.pending_position_ticks = Some(500);
                for p in g.participants.values_mut() {
                    p.is_buffering = true;
                }
                g.participants
                    .get_mut(&s2.session_id)
                    .expect("member missing")
                    .is_buffering = false;
            })
            .await;

        service
            .unregister_websocket_with_grace(s1.session_id.clone(), s1_connection_id)
            .await;

        let snapshot = service
            .get_group_snapshot_by_id(group.group_id)
            .await
            .expect("group missing");
        assert_eq!(snapshot.state, GroupStateType::Playing);
    }

    #[tokio::test]
    async fn test_leave_resolves_waiting_when_remaining_members_ready() {
        let service = SyncPlayService::new();
        let s1 = make_session(make_user("u1", "alice"), "web");
        let s2 = make_session(make_user("u2", "bob"), "tv");
        let (tx1, mut rx1) = mpsc::unbounded_channel::<String>();
        let (tx2, mut rx2) = mpsc::unbounded_channel::<String>();

        service.register_websocket(s1.session_id.clone(), tx1).await;
        service.register_websocket(s2.session_id.clone(), tx2).await;
        let _ = recv_json(&mut rx1).await;
        let _ = recv_json(&mut rx2).await;
        let group = service
            .create_group(&s1, "party".to_string())
            .await
            .expect("group creation should succeed");
        let _ = recv_json(&mut rx1).await;
        service.join_group(&s2, group.group_id, None).await;
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
                g.pending_position_ticks = Some(500);
                for p in g.participants.values_mut() {
                    p.is_buffering = true;
                }
                g.participants
                    .get_mut(&s2.session_id)
                    .expect("member missing")
                    .is_buffering = false;
            })
            .await;

        service.leave_group(&s1).await;

        let snapshot = service
            .get_group_snapshot_by_id(group.group_id)
            .await
            .expect("group missing");
        assert_eq!(snapshot.state, GroupStateType::Playing);
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

        let group = service
            .create_group(&s1, "party".to_string())
            .await
            .expect("group creation should succeed");
        let _ = recv_json(&mut rx1).await;
        service.join_group(&s2, group.group_id, None).await;
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
                g.participants
                    .get_mut(&s1.session_id)
                    .expect("member missing")
                    .is_buffering = false;
                sync_state.resolve_waiting(g, "Ready");
            })
            .await;

        let _ = service
            .with_group_for_session(&s2, |g, sync_state| {
                g.participants
                    .get_mut(&s2.session_id)
                    .expect("member missing")
                    .is_buffering = false;
                sync_state.resolve_waiting(g, "Ready");
            })
            .await;

        let msgs = recv_many(&mut rx1, 8).await;
        let cmd = msgs
            .iter()
            .find(|m| m["MessageType"] == "SyncPlayCommand" && m["Data"]["Command"] == "Unpause")
            .expect("expected unpause command");
        let when =
            DateTime::parse_from_rfc3339(cmd["Data"]["When"].as_str().expect("When missing"))
                .expect("invalid When timestamp");
        let emitted = DateTime::parse_from_rfc3339(
            cmd["Data"]["EmittedAt"]
                .as_str()
                .expect("EmittedAt missing"),
        )
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

        let group = service
            .create_group(&s1, "party".to_string())
            .await
            .expect("group creation should succeed");
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

        service.join_group(&s2, group.group_id, None).await;
        let msgs = recv_many(&mut rx2, 8).await;
        assert!(msgs
            .iter()
            .any(|m| m["MessageType"] == "SyncPlayGroupUpdate"
                && m["Data"]["Type"] == "PlayQueue"
                && m["Data"]["Data"]["Reason"] == "NewPlaylist"));

        let _ = recv_many(&mut rx1, 8).await;
    }

    #[tokio::test]
    async fn test_joiner_does_not_receive_user_joined_before_group_joined() {
        let service = SyncPlayService::new();
        let s1 = make_session(make_user("u1", "alice"), "web");
        let s2 = make_session(make_user("u2", "bob"), "tv");

        let (tx1, mut rx1) = mpsc::unbounded_channel::<String>();
        let (tx2, mut rx2) = mpsc::unbounded_channel::<String>();
        service.register_websocket(s1.session_id.clone(), tx1).await;
        service.register_websocket(s2.session_id.clone(), tx2).await;
        let _ = recv_json(&mut rx1).await;
        let _ = recv_json(&mut rx2).await;

        let group = service
            .create_group(&s1, "party".to_string())
            .await
            .expect("group creation should succeed");
        let _ = recv_json(&mut rx1).await;

        service.join_group(&s2, group.group_id, None).await;

        let joiner_messages = recv_many(&mut rx2, 8).await;
        assert!(joiner_messages.iter().any(|m| {
            m["MessageType"] == "SyncPlayGroupUpdate" && m["Data"]["Type"] == "GroupJoined"
        }));
        assert!(!joiner_messages.iter().any(|m| {
            m["MessageType"] == "SyncPlayGroupUpdate" && m["Data"]["Type"] == "UserJoined"
        }));

        let existing_member_messages = recv_many(&mut rx1, 8).await;
        assert!(existing_member_messages.iter().any(|m| {
            m["MessageType"] == "SyncPlayGroupUpdate"
                && m["Data"]["Type"] == "UserJoined"
                && m["Data"]["Data"] == "bob"
        }));
    }

    #[tokio::test]
    async fn test_empty_queue_snapshot_uses_minus_one_playing_item_index() {
        let service = SyncPlayService::new();
        let s1 = make_session(make_user("u1", "alice"), "web");
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();

        service.register_websocket(s1.session_id.clone(), tx).await;
        let _ = recv_json(&mut rx).await;

        let group = service
            .create_group(&s1, "party".to_string())
            .await
            .expect("group creation should succeed");
        let _ = recv_json(&mut rx).await;

        let _ = service
            .with_group_for_session(&s1, |g, sync_state| {
                g.playlist.clear();
                g.playing_item_index = None;
                g.touch();
                sync_state.send_queue_update_to(
                    g,
                    PlayQueueUpdateReason::NewPlaylist,
                    vec![s1.session_id.clone()],
                );
            })
            .await;

        let queue_update = recv_json(&mut rx).await;
        assert_eq!(queue_update["MessageType"], "SyncPlayGroupUpdate");
        assert_eq!(queue_update["Data"]["Type"], "PlayQueue");
        assert_eq!(
            queue_update["Data"]["Data"]["Playlist"],
            serde_json::json!([])
        );
        assert_eq!(queue_update["Data"]["Data"]["PlayingItemIndex"], -1);

        assert!(service
            .get_group_snapshot_by_id(group.group_id)
            .await
            .is_some());
    }

    #[tokio::test]
    async fn test_create_group_requires_websocket_connection() {
        let service = SyncPlayService::new();
        let s1 = make_session(make_user("u1", "alice"), "web");

        let group = service.create_group(&s1, "party".to_string()).await;
        assert!(group.is_none());
    }

    #[tokio::test]
    async fn test_join_group_requires_websocket_connection() {
        let service = SyncPlayService::new();
        let s1 = make_session(make_user("u1", "alice"), "web");
        let s2 = make_session(make_user("u2", "bob"), "tv");

        let (tx1, _rx1) = mpsc::unbounded_channel::<String>();
        service.register_websocket(s1.session_id.clone(), tx1).await;

        let group = service
            .create_group(&s1, "party".to_string())
            .await
            .expect("group creation should succeed");

        let joined = service.join_group(&s2, group.group_id, None).await;
        assert!(!joined);

        let snapshot = service
            .get_group_snapshot_by_id(group.group_id)
            .await
            .expect("group missing");
        assert_eq!(snapshot.participants.len(), 1);
    }
}
