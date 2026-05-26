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
pub(super) enum GroupUpdateType {
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
pub(super) enum SendCommandType {
    Unpause,
    Pause,
    Stop,
    Seek,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "PascalCase")]
pub(super) enum PlayQueueUpdateReason {
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

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub struct NewGroupRequestDto {
    pub group_name: String,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub struct JoinGroupRequestDto {
    pub group_id: Uuid,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub struct PlayRequestDto {
    #[serde(default)]
    pub playing_queue: Vec<String>,
    #[serde(default)]
    pub playing_item_position: usize,
    #[serde(default)]
    pub start_position_ticks: i64,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub struct SetPlaylistItemRequestDto {
    pub playlist_item_id: Uuid,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub struct RemoveFromPlaylistRequestDto {
    #[serde(default)]
    pub playlist_item_ids: Vec<Uuid>,
    #[serde(default)]
    pub clear_playlist: bool,
    #[serde(default)]
    pub clear_playing_item: bool,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub struct MovePlaylistItemRequestDto {
    pub playlist_item_id: Uuid,
    pub new_index: usize,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub struct QueueRequestDto {
    #[serde(default)]
    pub item_ids: Vec<String>,
    #[serde(default = "default_queue_mode")]
    pub mode: GroupQueueMode,
}

fn default_queue_mode() -> GroupQueueMode {
    GroupQueueMode::Queue
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub struct SeekRequestDto {
    #[serde(default)]
    pub position_ticks: i64,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub struct BufferRequestDto {
    #[serde(default = "utc_now")]
    pub when: DateTime<Utc>,
    #[serde(default)]
    pub position_ticks: i64,
    #[serde(default)]
    pub is_playing: bool,
    pub playlist_item_id: Option<Uuid>,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub struct IgnoreWaitRequestDto {
    #[serde(default)]
    pub ignore_wait: bool,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub struct NextItemRequestDto {
    pub playlist_item_id: Option<Uuid>,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub struct SetRepeatModeRequestDto {
    pub mode: GroupRepeatMode,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub struct SetShuffleModeRequestDto {
    pub mode: GroupShuffleMode,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub struct PingRequestDto {
    #[serde(default)]
    pub ping: f64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct UtcTimeResponse {
    pub request_reception_time: DateTime<Utc>,
    pub response_transmission_time: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub(super) struct GroupParticipant {
    pub user_name: String,
    pub ping: u64,
    pub is_buffering: bool,
    pub ignore_wait: bool,
    pub is_connected: bool,
    pub last_client_when: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct SyncPlayQueueItem {
    pub item_id: String,
    pub playlist_item_id: Uuid,
}

#[derive(Debug, Clone)]
pub(super) struct SyncPlayGroup {
    pub group_id: Uuid,
    pub group_name: String,
    pub state: GroupStateType,
    pub participants: HashMap<String, GroupParticipant>,
    pub playlist: Vec<SyncPlayQueueItem>,
    pub playing_item_index: Option<usize>,
    pub start_position_ticks: i64,
    pub pending_position_ticks: Option<i64>,
    pub position_base_when: DateTime<Utc>,
    pub is_playing: bool,
    pub shuffle_mode: GroupShuffleMode,
    pub repeat_mode: GroupRepeatMode,
    pub waiting_resume_playing: bool,
    pub last_updated_at: DateTime<Utc>,
}

impl SyncPlayGroup {
    pub fn to_group_info(&self) -> GroupInfoDto {
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

    pub fn current_playlist_item_id(&self) -> Uuid {
        self.playing_item_index
            .and_then(|idx| self.playlist.get(idx))
            .map(|item| item.playlist_item_id)
            .unwrap_or_else(Uuid::nil)
    }

    pub fn touch(&mut self) {
        self.last_updated_at = Utc::now();
    }

    pub fn set_position(&mut self, position_ticks: i64, when: DateTime<Utc>) {
        self.start_position_ticks = position_ticks.max(0);
        self.position_base_when = when;
    }

    pub fn estimated_position_ticks(&self, now: DateTime<Utc>) -> i64 {
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

    pub fn freeze_at_estimated_position(&mut self, now: DateTime<Utc>) -> i64 {
        let position_ticks = self.estimated_position_ticks(now);
        self.set_position(position_ticks, now);
        position_ticks
    }

    /// Collect all media item IDs from the playlist.
    pub fn queue_item_ids(&self) -> Vec<String> {
        self.playlist
            .iter()
            .map(|item| item.item_id.clone())
            .collect()
    }

    /// Transition the group into the Waiting state, marking all participants as buffering.
    pub fn transition_to_waiting(&mut self, resume_playing: bool) {
        self.state = GroupStateType::Waiting;
        self.waiting_resume_playing = resume_playing;
        for member in self.participants.values_mut() {
            member.is_buffering = true;
        }
    }

    /// Calculate the playback delay based on the highest participant ping.
    pub fn ping_delay_ms(&self) -> i64 {
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
pub(super) struct OutboundWebSocketMessage<T: Serialize> {
    pub message_type: &'static str,
    pub message_id: Uuid,
    pub data: T,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct GroupUpdateEnvelope {
    pub group_id: Uuid,
    #[serde(rename = "Type")]
    pub update_type: GroupUpdateType,
    pub data: Value,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct SendCommandEnvelope {
    pub group_id: Uuid,
    pub playlist_item_id: Uuid,
    pub when: DateTime<Utc>,
    pub position_ticks: Option<i64>,
    pub command: SendCommandType,
    pub emitted_at: DateTime<Utc>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct GroupStateUpdate {
    pub state: GroupStateType,
    pub reason: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct PlayQueueUpdate {
    pub reason: PlayQueueUpdateReason,
    pub last_update: DateTime<Utc>,
    pub playlist: Vec<SyncPlayQueueItem>,
    pub playing_item_index: i64,
    pub start_position_ticks: i64,
    pub is_playing: bool,
    pub shuffle_mode: GroupShuffleMode,
    pub repeat_mode: GroupRepeatMode,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct InboundWebSocketMessage {
    pub message_type: String,
    pub data: Option<Value>,
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
