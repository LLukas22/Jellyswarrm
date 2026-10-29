//! Named transitions with synchronization and ordered publication owned by SyncPlay.
use super::{models::*, service::SyncPlayService};
use crate::sessions::SessionContext;
use chrono::{DateTime, Utc};
use std::collections::HashSet;
use uuid::Uuid;

/// Capture before awaiting access checks; revalidate group identity as well as
/// membership when the command commits under the service's exclusive lock.
pub(crate) struct QueueAccessSnapshot {
    group_id: Uuid,
    participants: HashSet<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum QueueOutcome {
    Applied,
    NotInGroup,
    AccessChanged,
}

impl SyncPlayGroup {
    pub(crate) fn queue_access_snapshot(&self) -> QueueAccessSnapshot {
        QueueAccessSnapshot {
            group_id: self.group_id,
            participants: self.participants.keys().cloned().collect(),
        }
    }

    fn replace_queue(&mut self, payload: PlayRequestDto, now: DateTime<Utc>) {
        self.playlist = payload
            .playing_queue
            .into_iter()
            .map(|item_id| SyncPlayQueueItem {
                item_id,
                playlist_item_id: Uuid::new_v4(),
            })
            .collect();
        self.playing_item_index = if self.playlist.is_empty() {
            None
        } else {
            Some(payload.playing_item_position.min(self.playlist.len() - 1))
        };
        self.set_position(payload.start_position_ticks, now);
        self.pending_position_ticks = Some(self.start_position_ticks);
        self.is_playing = true;
        if self.playlist.is_empty() {
            self.state = GroupStateType::Idle;
            self.is_playing = false;
            self.set_position(0, now);
            self.pending_position_ticks = None;
            self.waiting_resume_playing = false;
        } else {
            self.transition_to_waiting(true);
        }
        self.last_updated_at = now;
    }
}

impl SyncPlayService {
    pub(crate) async fn queue_items(
        &self,
        session: &SessionContext,
        payload: QueueRequestDto,
        expected: Option<QueueAccessSnapshot>,
    ) -> QueueOutcome {
        let mut outcome = QueueOutcome::NotInGroup;
        self.with_group_for_session(session, |group, state| {
            if !expected
                .as_ref()
                .is_some_and(|expected| expected.matches(group))
            {
                state.notify_session_for_group(
                    &session.session_id,
                    group.group_id,
                    GroupUpdateType::LibraryAccessDenied,
                );
                outcome = QueueOutcome::AccessChanged;
                return;
            }
            let mode = payload.mode;
            let mut items: Vec<_> = payload
                .item_ids
                .into_iter()
                .map(|item_id| SyncPlayQueueItem {
                    item_id,
                    playlist_item_id: Uuid::new_v4(),
                })
                .collect();
            match mode {
                GroupQueueMode::Queue => group.playlist.append(&mut items),
                GroupQueueMode::QueueNext => {
                    let index = group.playing_item_index.map(|index| index + 1).unwrap_or(0);
                    group.playlist.splice(index..index, items);
                }
            }
            if group.playing_item_index.is_none() && !group.playlist.is_empty() {
                group.playing_item_index = Some(0);
            }
            group.touch();
            state.broadcast_queue_update(
                group,
                match mode {
                    GroupQueueMode::Queue => PlayQueueUpdateReason::Queue,
                    GroupQueueMode::QueueNext => PlayQueueUpdateReason::QueueNext,
                },
            );
            outcome = QueueOutcome::Applied;
        })
        .await;
        outcome
    }

    pub(crate) async fn set_queue(
        &self,
        session: &SessionContext,
        payload: PlayRequestDto,
        expected: Option<QueueAccessSnapshot>,
        now: DateTime<Utc>,
    ) -> QueueOutcome {
        let mut outcome = QueueOutcome::NotInGroup;
        self.with_group_for_session(session, |group, state| {
            if !expected
                .as_ref()
                .is_some_and(|expected| expected.matches(group))
            {
                state.notify_session_for_group(
                    &session.session_id,
                    group.group_id,
                    GroupUpdateType::LibraryAccessDenied,
                );
                outcome = QueueOutcome::AccessChanged;
                return;
            }
            group.replace_queue(payload, now);
            state.broadcast_queue_update(group, PlayQueueUpdateReason::NewPlaylist);
            if group.playlist.is_empty() {
                state.broadcast_command(group, SendCommandType::Stop, None, 0);
                state.broadcast_state_update(group, "Stop");
            } else {
                state.broadcast_state_update(group, "Play");
            }
            outcome = QueueOutcome::Applied;
        })
        .await;
        outcome
    }
}

impl QueueAccessSnapshot {
    fn matches(&self, group: &SyncPlayGroup) -> bool {
        self.group_id == group.group_id
            && self.participants == group.participants.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::service::{LocalCredential, User};

    fn session(id: &str) -> SessionContext {
        SessionContext {
            session_id: id.into(),
            user: User {
                id: id.into(),
                virtual_key: "key".into(),
                original_username: id.into(),
                local_credential: LocalCredential::Passwordless,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            },
        }
    }
    fn payload(items: &[&str]) -> PlayRequestDto {
        PlayRequestDto {
            playing_queue: items.iter().map(|s| (*s).into()).collect(),
            playing_item_position: 99,
            start_position_ticks: -1,
        }
    }

    #[tokio::test]
    async fn queue_command_preserves_transition_invariants_and_uses_supplied_time() {
        let service = SyncPlayService::new();
        let caller = session("caller");
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        service
            .register_websocket(caller.session_id.clone(), tx)
            .await;
        let group = service.create_group(&caller, "party".into()).await.unwrap();
        let now = Utc::now();
        let checked = service
            .get_group_snapshot_by_id(group.group_id)
            .await
            .unwrap()
            .queue_access_snapshot();
        assert_eq!(
            service
                .set_queue(&caller, payload(&["one", "two"]), Some(checked), now)
                .await,
            QueueOutcome::Applied
        );
        let snapshot = service
            .get_group_snapshot_by_id(group.group_id)
            .await
            .unwrap();
        assert_eq!(snapshot.playing_item_index, Some(1));
        assert_eq!(snapshot.state, GroupStateType::Waiting);
        assert_eq!(snapshot.pending_position_ticks, Some(0));
        assert_eq!(snapshot.last_updated_at, now);
        assert!(snapshot.participants.values().all(|p| p.is_buffering));
        let checked = snapshot.queue_access_snapshot();
        service
            .set_queue(&caller, payload(&[]), Some(checked), now)
            .await;
        let snapshot = service
            .get_group_snapshot_by_id(group.group_id)
            .await
            .unwrap();
        assert_eq!(snapshot.state, GroupStateType::Idle);
        assert_eq!(snapshot.playing_item_index, None);
        assert_eq!(snapshot.pending_position_ticks, None);
        assert!(!snapshot.is_playing);
    }

    #[tokio::test]
    async fn membership_or_group_changes_during_access_checks_do_not_commit() {
        let service = SyncPlayService::new();
        let caller = session("caller");
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        service
            .register_websocket(caller.session_id.clone(), tx)
            .await;
        let other = session("other");
        let (tx, _other_rx) = tokio::sync::mpsc::channel(64);
        service
            .register_websocket(other.session_id.clone(), tx)
            .await;
        let group = service.create_group(&caller, "party".into()).await.unwrap();
        let checked = service
            .get_group_snapshot_by_id(group.group_id)
            .await
            .unwrap()
            .queue_access_snapshot();
        service.join_group(&other, group.group_id, None).await;
        assert_eq!(
            service
                .set_queue(&caller, payload(&["secret"]), Some(checked), Utc::now())
                .await,
            QueueOutcome::AccessChanged
        );
        assert!(service
            .get_group_snapshot_by_id(group.group_id)
            .await
            .unwrap()
            .playlist
            .is_empty());
        assert_eq!(
            service
                .set_queue(&caller, payload(&["unchecked"]), None, Utc::now())
                .await,
            QueueOutcome::AccessChanged
        );
        service.leave_group(&other).await;
        let checked = service
            .get_group_snapshot_by_id(group.group_id)
            .await
            .unwrap()
            .queue_access_snapshot();
        service.leave_group(&caller).await;
        let replacement = service
            .create_group(&caller, "replacement".into())
            .await
            .unwrap();
        assert_ne!(replacement.group_id, group.group_id);
        assert_eq!(
            service
                .set_queue(
                    &caller,
                    payload(&["stale-group"]),
                    Some(checked),
                    Utc::now()
                )
                .await,
            QueueOutcome::AccessChanged
        );
        assert!(service
            .get_group_snapshot_by_id(replacement.group_id)
            .await
            .unwrap()
            .playlist
            .is_empty());
    }
}
