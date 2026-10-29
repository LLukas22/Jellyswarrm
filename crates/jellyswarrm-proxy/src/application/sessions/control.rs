//! Remote-command capability, media-access and SyncPlay coexistence policy.
use super::{SessionOperationError, SessionOperations};
use crate::sessions::{models::GeneralCommand, ClientSession, SessionContext};
use serde_json::{json, Value};

impl SessionOperations<'_> {
    pub(crate) async fn general_command(
        &self,
        caller: &SessionContext,
        target: &ClientSession,
        command: GeneralCommand,
    ) -> Result<(&'static str, Value), SessionOperationError> {
        let choices = [
            "MoveUp",
            "MoveDown",
            "MoveLeft",
            "MoveRight",
            "PageUp",
            "PageDown",
            "PreviousLetter",
            "NextLetter",
            "ToggleOsd",
            "ToggleContextMenu",
            "Select",
            "Back",
            "TakeScreenshot",
            "SendKey",
            "SendString",
            "GoHome",
            "GoToSettings",
            "VolumeUp",
            "VolumeDown",
            "Mute",
            "Unmute",
            "ToggleMute",
            "SetVolume",
            "SetAudioStreamIndex",
            "SetSubtitleStreamIndex",
            "ToggleFullscreen",
            "DisplayContent",
            "GoToSearch",
            "DisplayMessage",
            "SetRepeatMode",
            "ChannelUp",
            "ChannelDown",
            "Guide",
            "ToggleStats",
            "SetShuffleQueue",
            "ToggleOsdMenu",
            "SetMaxStreamingBitrate",
            "SetPlaybackOrder",
            "PlayMediaSource",
            "PlayTrailers",
            "PlayState",
            "PlayNext",
            "Play",
        ];
        let name = choices
            .into_iter()
            .find(|c| c.eq_ignore_ascii_case(&command.name))
            .ok_or(SessionOperationError::InvalidInput)?;
        if !target
            .capabilities
            .supported_commands
            .iter()
            .any(|c| c.eq_ignore_ascii_case(name))
        {
            return Err(SessionOperationError::NotControllable);
        }
        let arg = |key: &str| {
            command
                .arguments
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v.as_str())
        };
        let item_id = |value: &str| {
            uuid::Uuid::parse_str(value)
                .map(|id| id.simple().to_string())
                .map_err(|_| SessionOperationError::InvalidInput)
        };
        let mut items = Vec::new();
        if let Some(id) = arg("ItemId") {
            items.push(item_id(id)?);
        }
        if let Some(ids) = arg("ItemIds") {
            for id in ids.split(',') {
                items.push(item_id(id.trim())?);
            }
        }
        if matches!(name, "DisplayContent" | "PlayMediaSource" | "PlayTrailers") && items.is_empty()
        {
            return Err(SessionOperationError::InvalidInput);
        }
        if let Some(source) = arg("MediaSourceId") {
            if items.is_empty() {
                return Err(SessionOperationError::InvalidInput);
            }
            self.require_target_media(target, &items, Some(&item_id(source)?))
                .await?;
        } else if !items.is_empty() {
            self.require_target_media(target, &items, None).await?;
        }
        Ok((
            "GeneralCommand",
            json!({"Name": name, "Arguments": command.arguments,
            "ControllingUserId": caller.user.id}),
        ))
    }
}

/// Navigation and keyboard commands can trigger playback. Only presentation and
/// volume commands may coexist with SyncPlay; adapters do not choose this gate.
pub(super) fn mutates_playback(message_type: &str, data: &Value) -> bool {
    message_type != "GeneralCommand"
        || !matches!(
            data["Name"].as_str().unwrap_or(""),
            "VolumeUp"
                | "VolumeDown"
                | "Mute"
                | "Unmute"
                | "ToggleMute"
                | "SetVolume"
                | "DisplayMessage"
                | "ToggleFullscreen"
                | "ToggleStats"
                | "TakeScreenshot"
        )
}
