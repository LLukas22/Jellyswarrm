use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

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
pub(super) struct GroupParticipant {
    pub user_name: String,
    pub ping: u64,
    pub is_buffering: bool,
    pub ignore_wait: bool,
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
    pub playing_item_index: usize,
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
