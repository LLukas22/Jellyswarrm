use serde::Deserialize;
use serde_json::Value;

pub(crate) use crate::models::Capabilities;

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct InboundMessage {
    #[serde(alias = "messageType")]
    pub message_type: String,
    #[serde(default, alias = "data")]
    pub data: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct GeneralCommand {
    #[serde(alias = "name")]
    pub name: String,
    #[serde(default, alias = "arguments")]
    pub arguments: std::collections::BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MessageCommand {
    #[serde(default, alias = "header")]
    pub header: String,
    #[serde(alias = "text")]
    pub text: String,
    #[serde(default, alias = "timeoutMs")]
    pub timeout_ms: Option<u32>,
}
