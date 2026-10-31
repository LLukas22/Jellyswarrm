//! Playback and playlist commands. Mutation and ordered notification stay under
//! the feature's write lock; HTTP callers cannot supply arbitrary mutations.
use super::{
    models::*,
    service::{SyncPlayService, SyncPlayState},
};
use crate::sessions::SessionCaller;
use chrono::{DateTime, Duration, Utc};
use std::collections::HashSet;
use uuid::Uuid;

impl SyncPlayService {
    pub(crate) async fn set_playlist_item(
        &self,
        session: &SessionCaller,
        payload: SetPlaylistItemRequestDto,
    ) {
        self.with_group_for_session(session, |group, state| {
            if let Some(index) = group
                .playlist
                .iter()
                .position(|item| item.playlist_item_id == payload.playlist_item_id)
            {
                group.playing_item_index = Some(index);
                group.set_position(0, Utc::now());
                group.pending_position_ticks = Some(0);
                group.transition_to_waiting(group.is_playing);
                group.touch();
                state.broadcast_queue_update(group, PlayQueueUpdateReason::SetCurrentItem);
                state.broadcast_state_update(group, "SetPlaylistItem");
            }
        })
        .await;
    }

    pub(crate) async fn remove_from_playlist(
        &self,
        session: &SessionCaller,
        payload: RemoveFromPlaylistRequestDto,
    ) {
        self.with_group_for_session(session, |group, state| {
            if payload.clear_playlist {
                group.playlist.clear();
                group.playing_item_index = None;
                reset_playback(group);
                group.touch();
                state.broadcast_queue_update(group, PlayQueueUpdateReason::RemoveItems);
                state.broadcast_command(group, SendCommandType::Stop, None, 0);
                state.broadcast_state_update(group, "RemoveFromPlaylist");
                return;
            }
            let to_remove: HashSet<_> = payload.playlist_item_ids.into_iter().collect();
            let old_current = group
                .playing_item_index
                .and_then(|i| group.playlist.get(i))
                .map(|item| item.playlist_item_id);
            group
                .playlist
                .retain(|item| !to_remove.contains(&item.playlist_item_id));
            if payload.clear_playing_item {
                group.playing_item_index = None;
            } else if let Some(current) = old_current {
                group.playing_item_index = group
                    .playlist
                    .iter()
                    .position(|item| item.playlist_item_id == current);
            }
            if group.playlist.is_empty() {
                reset_playback(group);
            }
            group.touch();
            state.broadcast_queue_update(group, PlayQueueUpdateReason::RemoveItems);
            if group.playlist.is_empty() {
                state.broadcast_command(group, SendCommandType::Stop, None, 0);
                state.broadcast_state_update(group, "Stop");
            }
        })
        .await;
    }

    pub(crate) async fn move_playlist_item(
        &self,
        session: &SessionCaller,
        payload: MovePlaylistItemRequestDto,
    ) {
        self.with_group_for_session(session, |group, state| {
            if let Some(index) = group
                .playlist
                .iter()
                .position(|item| item.playlist_item_id == payload.playlist_item_id)
            {
                let current = group
                    .playing_item_index
                    .and_then(|index| group.playlist.get(index))
                    .map(|item| item.playlist_item_id);
                let item = group.playlist.remove(index);
                group
                    .playlist
                    .insert(payload.new_index.min(group.playlist.len()), item);
                if let Some(current) = current {
                    group.playing_item_index = group
                        .playlist
                        .iter()
                        .position(|item| item.playlist_item_id == current);
                }
                group.touch();
                state.broadcast_queue_update(group, PlayQueueUpdateReason::MoveItem);
            }
        })
        .await;
    }

    pub(crate) async fn unpause(&self, session: &SessionCaller) {
        self.with_group_for_session(session, |group, state| {
            let delay = group.ping_delay_ms();
            group.state = GroupStateType::Playing;
            group.is_playing = true;
            group.waiting_resume_playing = true;
            group.pending_position_ticks = None;
            group.position_base_when = Utc::now() + Duration::milliseconds(delay.max(0));
            group.touch();
            state.broadcast_command(
                group,
                SendCommandType::Unpause,
                Some(group.start_position_ticks),
                delay,
            );
            state.broadcast_state_update(group, "Unpause");
        })
        .await;
    }

    pub(crate) async fn pause(&self, session: &SessionCaller) {
        self.with_group_for_session(session, |group, state| {
            let position = group.freeze_at_estimated_position(Utc::now());
            group.state = GroupStateType::Paused;
            group.is_playing = false;
            group.waiting_resume_playing = false;
            group.pending_position_ticks = None;
            group.touch();
            state.broadcast_command(group, SendCommandType::Pause, Some(position), 0);
            state.broadcast_state_update(group, "Pause");
        })
        .await;
    }

    pub(crate) async fn stop(&self, session: &SessionCaller) {
        self.with_group_for_session(session, |group, state| {
            reset_playback(group);
            group.touch();
            state.broadcast_command(group, SendCommandType::Stop, None, 0);
            state.broadcast_state_update(group, "Stop");
        })
        .await;
    }

    pub(crate) async fn seek(&self, session: &SessionCaller, payload: SeekRequestDto) {
        self.with_group_for_session(session, |group, state| {
            group.set_position(payload.position_ticks.max(0), Utc::now());
            group.pending_position_ticks = Some(group.start_position_ticks);
            group.transition_to_waiting(group.is_playing);
            group.touch();
            state.broadcast_command(
                group,
                SendCommandType::Seek,
                Some(group.start_position_ticks),
                0,
            );
            state.broadcast_state_update(group, "Seek");
        })
        .await;
    }

    pub(crate) async fn buffering(&self, session: &SessionCaller, payload: BufferRequestDto) {
        self.with_group_for_session(session, |group, state| {
            let when = sanitize_client_when(payload.when);
            if correct_playlist(group, state, payload.playlist_item_id, &session.session_id) {
                return;
            }
            if let Some(member) = group.participants.get_mut(&session.session_id) {
                if is_stale(member.last_client_when, when) {
                    return;
                }
                member.last_client_when = Some(when);
                member.is_buffering = true;
            }
            let entering_waiting = group.state != GroupStateType::Waiting;
            let was_playing = group.state == GroupStateType::Playing && group.is_playing;
            if group.pending_position_ticks.is_none() {
                group.pending_position_ticks = Some(if was_playing {
                    group.freeze_at_estimated_position(Utc::now())
                } else {
                    payload.position_ticks.max(0)
                });
            }
            group.is_playing = was_playing || payload.is_playing;
            if entering_waiting {
                group.state = GroupStateType::Waiting;
                group.waiting_resume_playing = was_playing || payload.is_playing;
            }
            group.touch();
            if entering_waiting && was_playing {
                state.broadcast_command(
                    group,
                    SendCommandType::Pause,
                    Some(group.start_position_ticks),
                    0,
                );
            }
            state.broadcast_state_update(group, "Buffer");
        })
        .await;
    }

    pub(crate) async fn ready(&self, session: &SessionCaller, payload: BufferRequestDto) {
        self.with_group_for_session(session, |group, state| {
            let when = sanitize_client_when(payload.when);
            if correct_playlist(group, state, payload.playlist_item_id, &session.session_id) {
                return;
            }
            if let Some(member) = group.participants.get_mut(&session.session_id) {
                if is_stale(member.last_client_when, when) {
                    return;
                }
                member.last_client_when = Some(when);
                member.is_buffering = false;
            }
            if group.pending_position_ticks.is_none() && group.state != GroupStateType::Waiting {
                group.touch();
                return;
            }
            group.state = GroupStateType::Waiting;
            if group.pending_position_ticks.is_none() {
                group.pending_position_ticks = Some(payload.position_ticks.max(0));
            }
            group.touch();
            state.resolve_waiting(group, "Ready");
        })
        .await;
    }

    pub(crate) async fn set_ignore_wait(
        &self,
        session: &SessionCaller,
        payload: IgnoreWaitRequestDto,
    ) {
        self.with_group_for_session(session, |group, state| {
            if let Some(member) = group.participants.get_mut(&session.session_id) {
                member.ignore_wait = payload.ignore_wait;
            }
            group.touch();
            state.resolve_waiting(group, "IgnoreWait");
        })
        .await;
    }

    pub(crate) async fn next_item(&self, session: &SessionCaller, item: Option<Uuid>) {
        self.with_group_for_session(session, |group, state| {
            navigate(
                group,
                state,
                item,
                1,
                PlayQueueUpdateReason::NextItem,
                "NextItem",
            )
        })
        .await;
    }

    pub(crate) async fn previous_item(&self, session: &SessionCaller, item: Option<Uuid>) {
        self.with_group_for_session(session, |group, state| {
            navigate(
                group,
                state,
                item,
                -1,
                PlayQueueUpdateReason::PreviousItem,
                "PreviousItem",
            )
        })
        .await;
    }

    pub(crate) async fn set_repeat_mode(
        &self,
        session: &SessionCaller,
        payload: SetRepeatModeRequestDto,
    ) {
        self.with_group_for_session(session, |group, state| {
            group.repeat_mode = payload.mode;
            group.touch();
            state.broadcast_queue_update(group, PlayQueueUpdateReason::RepeatMode);
        })
        .await;
    }

    pub(crate) async fn set_shuffle_mode(
        &self,
        session: &SessionCaller,
        payload: SetShuffleModeRequestDto,
    ) {
        self.with_group_for_session(session, |group, state| {
            group.shuffle_mode = payload.mode;
            group.touch();
            state.broadcast_queue_update(group, PlayQueueUpdateReason::ShuffleMode);
        })
        .await;
    }

    pub(crate) async fn ping(&self, session: &SessionCaller, payload: PingRequestDto) {
        self.with_group_for_session(session, |group, _| {
            if let Some(member) = group.participants.get_mut(&session.session_id) {
                member.ping = payload.ping.max(0.0).round() as u64;
            }
            group.touch();
        })
        .await;
    }
}

fn reset_playback(group: &mut SyncPlayGroup) {
    group.state = GroupStateType::Idle;
    group.is_playing = false;
    group.waiting_resume_playing = false;
    group.set_position(0, Utc::now());
    group.pending_position_ticks = None;
}

fn correct_playlist(
    group: &SyncPlayGroup,
    state: &mut SyncPlayState,
    expected: Option<Uuid>,
    session: &str,
) -> bool {
    if let (Some(expected), Some(current)) = (
        expected,
        group
            .playing_item_index
            .and_then(|index| group.playlist.get(index)),
    ) {
        if current.playlist_item_id != expected {
            state.send_queue_update_to(
                group,
                PlayQueueUpdateReason::SetCurrentItem,
                vec![session.to_owned()],
            );
            return true;
        }
    }
    false
}

fn sanitize_client_when(when: DateTime<Utc>) -> DateTime<Utc> {
    when.min(Utc::now() + Duration::seconds(2))
}
fn is_stale(last: Option<DateTime<Utc>>, when: DateTime<Utc>) -> bool {
    last.is_some_and(|last| when + Duration::milliseconds(500) < last)
}

fn navigate(
    group: &mut SyncPlayGroup,
    state: &mut SyncPlayState,
    item: Option<Uuid>,
    direction: isize,
    reason: PlayQueueUpdateReason,
    label: &'static str,
) {
    if group.playlist.is_empty() {
        return;
    }
    let current = item
        .and_then(|id| {
            group
                .playlist
                .iter()
                .position(|item| item.playlist_item_id == id)
        })
        .or(group.playing_item_index)
        .unwrap_or(0);
    group.playing_item_index =
        Some((current as isize + direction).rem_euclid(group.playlist.len() as isize) as usize);
    group.set_position(0, Utc::now());
    group.pending_position_ticks = Some(0);
    group.transition_to_waiting(group.is_playing);
    group.touch();
    state.broadcast_queue_update(group, reason);
    state.broadcast_state_update(group, label);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::service::{LocalCredential, LocalUser};
    use serde_json::Value;
    use tokio::sync::mpsc;

    async fn fixture() -> (SyncPlayService, SessionCaller, mpsc::Receiver<String>) {
        let service = SyncPlayService::new();
        let session = SessionCaller {
            session_id: "caller".into(),
            user: LocalUser {
                id: "user".into(),
                virtual_key: "key".into(),
                original_username: "User".into(),
                local_credential: LocalCredential::Passwordless,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            },
        };
        let (tx, mut rx) = mpsc::channel(128);
        service
            .register_websocket(session.session_id.clone(), tx)
            .await;
        service
            .create_group(&session, "Party".into())
            .await
            .unwrap();
        let checked = snapshot(&service, &session).await.queue_access_snapshot();
        service
            .set_queue(
                &session,
                PlayRequestDto {
                    playing_queue: vec!["one".into(), "two".into(), "three".into()],
                    playing_item_position: 1,
                    start_position_ticks: 0,
                },
                Some(checked),
                Utc::now(),
            )
            .await;
        drain(&mut rx);
        (service, session, rx)
    }

    async fn snapshot(service: &SyncPlayService, session: &SessionCaller) -> SyncPlayGroup {
        service
            .get_group_snapshot_for_session(&session.session_id)
            .await
            .unwrap()
    }

    fn drain(rx: &mut mpsc::Receiver<String>) -> Vec<Value> {
        let mut messages = Vec::new();
        while let Ok(message) = rx.try_recv() {
            messages.push(serde_json::from_str(&message).unwrap());
        }
        messages
    }

    #[tokio::test]
    async fn playlist_edits_preserve_current_identity_and_publish_queue_before_stop() {
        let (service, session, mut rx) = fixture().await;
        let group = snapshot(&service, &session).await;
        let current = group.current_playlist_item_id();
        service
            .move_playlist_item(
                &session,
                MovePlaylistItemRequestDto {
                    playlist_item_id: group.playlist[0].playlist_item_id,
                    new_index: usize::MAX,
                },
            )
            .await;
        assert_eq!(
            snapshot(&service, &session)
                .await
                .current_playlist_item_id(),
            current
        );
        service
            .remove_from_playlist(
                &session,
                RemoveFromPlaylistRequestDto {
                    playlist_item_ids: vec![group.playlist[2].playlist_item_id],
                    clear_playlist: false,
                    clear_playing_item: false,
                },
            )
            .await;
        assert_eq!(
            snapshot(&service, &session)
                .await
                .current_playlist_item_id(),
            current
        );
        drain(&mut rx);
        service
            .remove_from_playlist(
                &session,
                RemoveFromPlaylistRequestDto {
                    playlist_item_ids: vec![],
                    clear_playlist: true,
                    clear_playing_item: false,
                },
            )
            .await;
        let group = snapshot(&service, &session).await;
        assert!(group.playlist.is_empty());
        assert_eq!(group.playing_item_index, None);
        assert_eq!(group.pending_position_ticks, None);
        assert_eq!(group.state, GroupStateType::Idle);
        assert!(!group.is_playing);
        let messages = drain(&mut rx);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["Data"]["Type"], "PlayQueue");
        assert_eq!(messages[1]["Data"]["Command"], "Stop");
        assert_eq!(messages[2]["Data"]["Data"]["Reason"], "RemoveFromPlaylist");
    }

    #[tokio::test]
    async fn navigation_wraps_and_unknown_item_selection_is_a_noop() {
        let (service, session, mut rx) = fixture().await;
        service
            .set_playlist_item(
                &session,
                SetPlaylistItemRequestDto {
                    playlist_item_id: Uuid::new_v4(),
                },
            )
            .await;
        assert!(drain(&mut rx).is_empty());
        let group = snapshot(&service, &session).await;
        service
            .next_item(&session, Some(group.playlist[2].playlist_item_id))
            .await;
        assert_eq!(
            snapshot(&service, &session).await.playing_item_index,
            Some(0)
        );
        service.previous_item(&session, None).await;
        let group = snapshot(&service, &session).await;
        assert_eq!(group.playing_item_index, Some(2));
        assert_eq!(group.pending_position_ticks, Some(0));
        assert_eq!(group.state, GroupStateType::Waiting);
    }

    #[tokio::test]
    async fn seek_ready_pause_stop_preserve_timing_and_notification_contracts() {
        let (service, session, mut rx) = fixture().await;
        service
            .seek(
                &session,
                SeekRequestDto {
                    position_ticks: -10,
                },
            )
            .await;
        let messages = drain(&mut rx);
        assert_eq!(messages[0]["Data"]["Command"], "Seek");
        assert_eq!(messages[0]["Data"]["PositionTicks"], 0);
        assert_eq!(messages[1]["Data"]["Data"]["Reason"], "Seek");
        service
            .ready(
                &session,
                BufferRequestDto {
                    when: Utc::now(),
                    position_ticks: 0,
                    is_playing: true,
                    playlist_item_id: None,
                },
            )
            .await;
        assert_eq!(
            snapshot(&service, &session).await.state,
            GroupStateType::Playing
        );
        service.pause(&session).await;
        let group = snapshot(&service, &session).await;
        assert_eq!(group.state, GroupStateType::Paused);
        assert!(!group.is_playing);
        assert!(group.pending_position_ticks.is_none());
        service.unpause(&session).await;
        assert_eq!(
            snapshot(&service, &session).await.state,
            GroupStateType::Playing
        );
        service.stop(&session).await;
        let group = snapshot(&service, &session).await;
        assert_eq!(group.start_position_ticks, 0);
        assert!(!group.waiting_resume_playing);
        assert_eq!(group.state, GroupStateType::Idle);
    }

    #[tokio::test]
    async fn stale_and_wrong_playlist_reports_cannot_mutate_participant_state() {
        let (service, session, mut rx) = fixture().await;
        let when = Utc::now();
        service
            .ready(
                &session,
                BufferRequestDto {
                    when,
                    position_ticks: 0,
                    is_playing: true,
                    playlist_item_id: None,
                },
            )
            .await;
        drain(&mut rx);
        service
            .buffering(
                &session,
                BufferRequestDto {
                    when: when - Duration::seconds(1),
                    position_ticks: 42,
                    is_playing: false,
                    playlist_item_id: None,
                },
            )
            .await;
        assert!(drain(&mut rx).is_empty());
        assert_eq!(
            snapshot(&service, &session).await.state,
            GroupStateType::Playing
        );
        service
            .buffering(
                &session,
                BufferRequestDto {
                    when,
                    position_ticks: 42,
                    is_playing: false,
                    playlist_item_id: Some(Uuid::new_v4()),
                },
            )
            .await;
        let messages = drain(&mut rx);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["Data"]["Type"], "PlayQueue");
        assert!(!snapshot(&service, &session).await.participants[&session.session_id].is_buffering);
    }

    #[tokio::test]
    async fn queue_appends_preserve_order_and_reject_unchecked_or_stale_access() {
        use super::super::commands::QueueOutcome;
        let (service, session, _rx) = fixture().await;
        let group = snapshot(&service, &session).await;
        let current = group.current_playlist_item_id();
        assert_eq!(
            service
                .queue_items(
                    &session,
                    QueueRequestDto {
                        item_ids: vec!["next-a".into(), "next-b".into()],
                        mode: GroupQueueMode::QueueNext
                    },
                    Some(group.queue_access_snapshot())
                )
                .await,
            QueueOutcome::Applied
        );
        let group = snapshot(&service, &session).await;
        assert_eq!(
            group.queue_item_ids(),
            ["one", "two", "next-a", "next-b", "three"]
        );
        assert_eq!(group.current_playlist_item_id(), current);
        assert_eq!(
            service
                .queue_items(
                    &session,
                    QueueRequestDto {
                        item_ids: vec!["unchecked".into()],
                        mode: GroupQueueMode::Queue
                    },
                    None
                )
                .await,
            QueueOutcome::AccessChanged
        );
        let checked = group.queue_access_snapshot();
        service.leave_group(&session).await;
        service
            .create_group(&session, "Different".into())
            .await
            .unwrap();
        assert_eq!(
            service
                .queue_items(
                    &session,
                    QueueRequestDto {
                        item_ids: vec!["stale".into()],
                        mode: GroupQueueMode::Queue
                    },
                    Some(checked)
                )
                .await,
            QueueOutcome::AccessChanged
        );
        assert!(snapshot(&service, &session).await.playlist.is_empty());
    }

    #[tokio::test]
    async fn queue_append_rejects_membership_changes_during_authorization() {
        use super::super::commands::QueueOutcome;
        let (service, session, mut rx) = fixture().await;
        let group = snapshot(&service, &session).await;
        let checked = group.queue_access_snapshot();
        let mut other = session.clone();
        other.session_id = "other-session".into();
        other.user.id = "other-user".into();
        let (tx, mut other_rx) = mpsc::channel(64);
        service
            .register_websocket(other.session_id.clone(), tx)
            .await;
        service.join_group(&other, group.group_id, None).await;
        drain(&mut rx);
        drain(&mut other_rx);
        assert_eq!(
            service
                .queue_items(
                    &session,
                    QueueRequestDto {
                        item_ids: vec!["unchecked-for-other".into()],
                        mode: GroupQueueMode::Queue
                    },
                    Some(checked)
                )
                .await,
            QueueOutcome::AccessChanged
        );
        assert_eq!(
            snapshot(&service, &session).await.queue_item_ids(),
            group.queue_item_ids()
        );
        let messages = drain(&mut rx);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["Data"]["Type"], "LibraryAccessDenied");
        assert!(drain(&mut other_rx).is_empty());
    }

    #[tokio::test]
    async fn ignore_wait_and_mode_commands_update_only_their_owned_state() {
        let (service, session, mut rx) = fixture().await;
        service
            .set_ignore_wait(&session, IgnoreWaitRequestDto { ignore_wait: true })
            .await;
        let group = snapshot(&service, &session).await;
        assert!(group.participants[&session.session_id].ignore_wait);
        assert_eq!(group.state, GroupStateType::Playing);
        service.ping(&session, PingRequestDto { ping: -5.0 }).await;
        assert_eq!(
            snapshot(&service, &session).await.participants[&session.session_id].ping,
            0
        );
        service.ping(&session, PingRequestDto { ping: 12.6 }).await;
        assert_eq!(
            snapshot(&service, &session).await.participants[&session.session_id].ping,
            13
        );
        drain(&mut rx);
        // Parse wire enums to exercise the same values accepted by adapters.
        service
            .set_repeat_mode(
                &session,
                serde_json::from_value(serde_json::json!({"Mode":"RepeatAll"})).unwrap(),
            )
            .await;
        service
            .set_shuffle_mode(
                &session,
                serde_json::from_value(serde_json::json!({"Mode":"Shuffle"})).unwrap(),
            )
            .await;
        let group = snapshot(&service, &session).await;
        assert_eq!(group.queue_item_ids(), ["one", "two", "three"]);
        let messages = drain(&mut rx);
        assert_eq!(messages[0]["Data"]["Data"]["RepeatMode"], "RepeatAll");
        assert_eq!(messages[1]["Data"]["Data"]["ShuffleMode"], "Shuffle");
    }
}
