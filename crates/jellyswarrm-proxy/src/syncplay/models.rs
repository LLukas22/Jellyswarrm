//! Wire-compatible SyncPlay requests, group state, queues, and notification payloads.
use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use jellyswarrm_macros::multi_case_struct;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

fn utc_now() -> DateTime<Utc> {
    Utc::now()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) enum GroupStateType {
    Idle,
    Waiting,
    Paused,
    Playing,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) enum GroupQueueMode {
    Queue,
    QueueNext,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) enum GroupShuffleMode {
    Sorted,
    Shuffle,
}

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) enum GroupRepeatMode {
    RepeatOne,
    RepeatAll,
    RepeatNone,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) enum GroupUpdateType {
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
pub(crate) enum SendCommandType {
    Unpause,
    Pause,
    Stop,
    Seek,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) enum PlayQueueUpdateReason {
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
pub(crate) struct GroupInfoDto {
    pub(crate) group_id: Uuid,
    pub(crate) group_name: String,
    pub(crate) state: GroupStateType,
    pub(crate) participants: Vec<String>,
    pub(crate) last_updated_at: DateTime<Utc>,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub(crate) struct NewGroupRequestDto {
    pub(crate) group_name: String,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub(crate) struct JoinGroupRequestDto {
    pub(crate) group_id: Uuid,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub(crate) struct PlayRequestDto {
    #[serde(default)]
    pub(crate) playing_queue: Vec<String>,
    #[serde(default)]
    pub(crate) playing_item_position: usize,
    #[serde(default)]
    pub(crate) start_position_ticks: i64,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub(crate) struct SetPlaylistItemRequestDto {
    pub(crate) playlist_item_id: Uuid,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub(crate) struct RemoveFromPlaylistRequestDto {
    #[serde(default)]
    pub(crate) playlist_item_ids: Vec<Uuid>,
    #[serde(default)]
    pub(crate) clear_playlist: bool,
    #[serde(default)]
    pub(crate) clear_playing_item: bool,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub(crate) struct MovePlaylistItemRequestDto {
    pub(crate) playlist_item_id: Uuid,
    pub(crate) new_index: usize,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub(crate) struct QueueRequestDto {
    #[serde(default)]
    pub(crate) item_ids: Vec<String>,
    #[serde(default = "default_queue_mode")]
    pub(crate) mode: GroupQueueMode,
}

fn default_queue_mode() -> GroupQueueMode {
    GroupQueueMode::Queue
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub(crate) struct SeekRequestDto {
    #[serde(default)]
    pub(crate) position_ticks: i64,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub(crate) struct BufferRequestDto {
    #[serde(default = "utc_now")]
    pub(crate) when: DateTime<Utc>,
    #[serde(default)]
    pub(crate) position_ticks: i64,
    #[serde(default)]
    pub(crate) is_playing: bool,
    pub(crate) playlist_item_id: Option<Uuid>,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub(crate) struct IgnoreWaitRequestDto {
    #[serde(default)]
    pub(crate) ignore_wait: bool,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub(crate) struct NextItemRequestDto {
    pub(crate) playlist_item_id: Option<Uuid>,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub(crate) struct SetRepeatModeRequestDto {
    pub(crate) mode: GroupRepeatMode,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub(crate) struct SetShuffleModeRequestDto {
    pub(crate) mode: GroupShuffleMode,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub(crate) struct PingRequestDto {
    #[serde(default)]
    pub(crate) ping: f64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct UtcTimeResponse {
    pub(crate) request_reception_time: DateTime<Utc>,
    pub(crate) response_transmission_time: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub(crate) struct GroupParticipant {
    pub(super) user_id: String,
    pub(super) user_name: String,
    pub(super) ping: u64,
    pub(super) is_buffering: bool,
    pub(super) ignore_wait: bool,
    pub(super) is_connected: bool,
    pub(super) last_client_when: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct SyncPlayQueueItem {
    pub(crate) item_id: String,
    pub(crate) playlist_item_id: Uuid,
}

#[derive(Debug, Clone)]
pub(crate) struct SyncPlayGroup {
    pub(super) group_id: Uuid,
    pub(super) group_name: String,
    pub(super) state: GroupStateType,
    pub(super) participants: HashMap<String, GroupParticipant>,
    pub(super) playlist: Vec<SyncPlayQueueItem>,
    pub(super) playing_item_index: Option<usize>,
    pub(super) start_position_ticks: i64,
    pub(super) pending_position_ticks: Option<i64>,
    pub(super) position_base_when: DateTime<Utc>,
    pub(super) is_playing: bool,
    pub(super) shuffle_mode: GroupShuffleMode,
    pub(super) repeat_mode: GroupRepeatMode,
    pub(super) waiting_resume_playing: bool,
    pub(super) last_updated_at: DateTime<Utc>,
}

impl SyncPlayGroup {
    pub(crate) fn participant_user_ids(&self) -> Vec<String> {
        self.participants
            .values()
            .map(|participant| participant.user_id.clone())
            .collect()
    }

    pub(crate) fn to_group_info(&self) -> GroupInfoDto {
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

    pub(crate) fn current_playlist_item_id(&self) -> Uuid {
        self.playing_item_index
            .and_then(|idx| self.playlist.get(idx))
            .map(|item| item.playlist_item_id)
            .unwrap_or_else(Uuid::nil)
    }

    pub(super) fn touch(&mut self) {
        self.last_updated_at = Utc::now();
    }

    pub(super) fn set_position(&mut self, position_ticks: i64, when: DateTime<Utc>) {
        self.start_position_ticks = position_ticks.max(0);
        self.position_base_when = when;
    }

    pub(crate) fn estimated_position_ticks(&self, now: DateTime<Utc>) -> i64 {
        let base = self
            .pending_position_ticks
            .unwrap_or(self.start_position_ticks)
            .max(0);
        if self.state != GroupStateType::Playing
            || !self.is_playing
            || self.pending_position_ticks.is_some()
        {
            return base;
        }

        let elapsed_ms = now
            .signed_duration_since(self.position_base_when)
            .num_milliseconds()
            .max(0);
        base.saturating_add(elapsed_ms.saturating_mul(10_000))
    }

    pub(super) fn freeze_at_estimated_position(&mut self, now: DateTime<Utc>) -> i64 {
        let position_ticks = self.estimated_position_ticks(now);
        self.set_position(position_ticks, now);
        position_ticks
    }

    /// Collect all media item IDs from the playlist.
    pub(crate) fn queue_item_ids(&self) -> Vec<String> {
        self.playlist
            .iter()
            .map(|item| item.item_id.clone())
            .collect()
    }

    /// Transition the group into the Waiting state, marking all participants as buffering.
    pub(super) fn transition_to_waiting(&mut self, resume_playing: bool) {
        self.state = GroupStateType::Waiting;
        self.waiting_resume_playing = resume_playing;
        for member in self.participants.values_mut() {
            member.is_buffering = true;
        }
    }

    /// Calculate the playback delay based on the highest participant ping.
    pub(crate) fn ping_delay_ms(&self) -> i64 {
        let highest = self
            .participants
            .values()
            .filter(|p| p.is_connected)
            .map(|p| p.ping as i64)
            .max()
            .unwrap_or(super::service::DEFAULT_PING_MS);
        std::cmp::max(highest * 2, super::service::DEFAULT_PING_MS)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct GroupUpdateEnvelope {
    pub(crate) group_id: Uuid,
    #[serde(rename = "Type")]
    pub(crate) update_type: GroupUpdateType,
    pub(crate) data: Value,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct SendCommandEnvelope {
    pub(crate) group_id: Uuid,
    pub(crate) playlist_item_id: Uuid,
    pub(crate) when: DateTime<Utc>,
    pub(crate) position_ticks: Option<i64>,
    pub(crate) command: SendCommandType,
    pub(crate) emitted_at: DateTime<Utc>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct GroupStateUpdate {
    pub(crate) state: GroupStateType,
    pub(crate) reason: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct PlayQueueUpdate {
    pub(crate) reason: PlayQueueUpdateReason,
    pub(crate) last_update: DateTime<Utc>,
    pub(crate) playlist: Vec<SyncPlayQueueItem>,
    pub(crate) playing_item_index: i64,
    pub(crate) start_position_ticks: i64,
    pub(crate) is_playing: bool,
    pub(crate) shuffle_mode: GroupShuffleMode,
    pub(crate) repeat_mode: GroupRepeatMode,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_from_playlist_accepts_clear_payload_without_item_ids() {
        let payload: RemoveFromPlaylistRequestDto =
            serde_json::from_value(serde_json::json!({ "ClearPlaylist": true }))
                .expect("clear playlist payload should deserialize");

        assert!(payload.clear_playlist);
        assert!(payload.playlist_item_ids.is_empty());
    }

    #[test]
    fn remove_from_playlist_accepts_item_payload_without_clear_flags() {
        let playlist_item_id = Uuid::new_v4();
        let payload: RemoveFromPlaylistRequestDto = serde_json::from_value(serde_json::json!({
            "PlaylistItemIds": [playlist_item_id]
        }))
        .expect("remove item payload should deserialize");

        assert!(!payload.clear_playlist);
        assert_eq!(payload.playlist_item_ids, vec![playlist_item_id]);
    }

    #[test]
    fn ping_accepts_fractional_javascript_number() {
        let payload: PingRequestDto = serde_json::from_value(serde_json::json!({
            "Ping": 17.5
        }))
        .expect("fractional ping should deserialize");

        assert_eq!(payload.ping, 17.5);
    }

    #[test]
    fn buffer_payload_accepts_missing_playlist_item_id() {
        let payload: BufferRequestDto = serde_json::from_value(serde_json::json!({
            "When": "2026-05-25T12:00:00Z",
            "PositionTicks": 1234,
            "IsPlaying": true
        }))
        .expect("buffer payload should deserialize without playlist item id");

        assert_eq!(payload.playlist_item_id, None);
    }

    #[test]
    fn syncplay_requests_accept_camel_case_aliases() {
        let payload: QueueRequestDto = serde_json::from_value(serde_json::json!({
            "itemIds": ["media-a"],
            "mode": "QueueNext"
        }))
        .expect("camel case queue payload should deserialize");

        assert_eq!(payload.item_ids, vec!["media-a".to_string()]);
        assert!(matches!(payload.mode, GroupQueueMode::QueueNext));
    }
}
