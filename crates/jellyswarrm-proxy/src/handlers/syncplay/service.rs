use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, info};
use uuid::Uuid;

use crate::user_authorization_service::User;

use super::models::{
    GroupInfoDto, GroupParticipant, GroupStateType, GroupStateUpdate, GroupUpdateEnvelope,
    GroupUpdateType, OutboundWebSocketMessage, PlayQueueUpdate, PlayQueueUpdateReason,
    SendCommandEnvelope, SendCommandType, SyncPlayGroup,
};

pub(crate) const DEFAULT_PING_MS: i64 = 500;

#[derive(Default)]
pub(super) struct SyncPlayState {
    groups: HashMap<Uuid, SyncPlayGroup>,
    session_to_group: HashMap<String, Uuid>,
    ws_connections: HashMap<String, mpsc::UnboundedSender<String>>,
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
        let (cmd, delay_ms) = if group.waiting_resume_playing {
            group.state = GroupStateType::Playing;
            group.is_playing = true;
            (SendCommandType::Unpause, group.ping_delay_ms())
        } else {
            group.state = GroupStateType::Paused;
            group.is_playing = false;
            (SendCommandType::Pause, 0)
        };
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
            .all(|p| !p.is_buffering || p.ignore_wait)
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
    ) {
        let mut state = self.state.write().await;
        state.ws_connections.insert(session_id.clone(), tx);
        debug!(session_id = %session_id, "Registered SyncPlay websocket");
        state.send_to_session(&session_id, "ForceKeepAlive", &15_u64);
    }

    pub(super) async fn unregister_websocket_and_leave(&self, session_id: &str) {
        let mut state = self.state.write().await;
        state.ws_connections.remove(session_id);
        debug!(session_id = %session_id, "Unregistered SyncPlay websocket and leaving group");
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
        }
    }

    pub(super) async fn create_group(
        &self,
        session: &SessionContext,
        group_name: String,
    ) -> GroupInfoDto {
        let group_id = Uuid::new_v4();
        let mut state = self.state.write().await;
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

        group.to_group_info()
    }

    pub(super) async fn join_group(&self, session: &SessionContext, group_id: Uuid) {
        let mut state = self.state.write().await;
        Self::leave_locked(&mut state, &session.session_id);

        let Some(mut group) = state.groups.remove(&group_id) else {
            state.notify_session(&session.session_id, GroupUpdateType::GroupDoesNotExist);
            return;
        };

        group.participants.insert(
            session.session_id.clone(),
            Self::make_participant(session, !group.playlist.is_empty()),
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
        state.broadcast_group_update(&group, GroupUpdateType::UserJoined, Value::String(username));
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
            state.broadcast_state_update(&group, "Buffer");
        }

        state.groups.insert(group_id, group);
        info!(
            group_id = %group_id,
            user = %session.user.original_username,
            session_id = %session.session_id,
            "SyncPlay member joined group"
        );
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

        let group = service.create_group(&s1, "party".to_string()).await;
        assert_eq!(group.participants, vec!["alice".to_string()]);

        service.join_group(&s2, group.group_id).await;
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
                sync_state.broadcast_command(g, SendCommandType::Pause, Some(123), 0);
                sync_state.broadcast_state_update(g, "Pause");
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
    async fn test_websocket_disconnect_removes_participant() {
        let service = SyncPlayService::new();
        let s1 = make_session(make_user("u1", "alice"), "web");
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();

        service.register_websocket(s1.session_id.clone(), tx).await;
        let _ = recv_json(&mut rx).await;

        let group = service.create_group(&s1, "party".to_string()).await;
        let _ = recv_json(&mut rx).await;

        service.unregister_websocket_and_leave(&s1.session_id).await;
        assert!(service
            .get_group_snapshot_by_id(group.group_id)
            .await
            .is_none());
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

        let group = service.create_group(&s1, "party".to_string()).await;
        let _ = recv_json(&mut rx1).await;
        service.join_group(&s2, group.group_id).await;
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
