//! Client presence, playback state, capabilities, and remote-control wire records.
use serde::Deserialize;
use serde_json::Value;

pub(crate) use crate::models::Capabilities;

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct InboundMessage {
    #[serde(alias = "messageType")]
    pub(crate) message_type: String,
    #[serde(default, alias = "data")]
    pub(crate) data: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct GeneralCommand {
    #[serde(alias = "name")]
    pub(crate) name: String,
    #[serde(default, alias = "arguments")]
    pub(crate) arguments: std::collections::BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct MessageCommand {
    #[serde(default, alias = "header")]
    pub(crate) header: String,
    #[serde(alias = "text")]
    pub(crate) text: String,
    #[serde(default, alias = "timeoutMs")]
    pub(crate) timeout_ms: Option<u32>,
}
