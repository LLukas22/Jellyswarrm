//! Version metadata belonging to the embedded Jellyfin web assets.
use super::Asset;
use std::sync::LazyLock;

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct JellyfinUiVersion {
    pub(crate) version: String,
    pub(crate) commit: String,
}
impl Default for JellyfinUiVersion {
    fn default() -> Self {
        Self {
            version: "unknown".into(),
            commit: "unknown".into(),
        }
    }
}
fn embedded_version() -> Option<JellyfinUiVersion> {
    let Some(file) = Asset::get("ui-version.env") else {
        tracing::error!("Failed to load Jellyfin UI version info from embedded resources");
        return None;
    };
    let content = String::from_utf8_lossy(&file.data);
    let mut result = JellyfinUiVersion::default();
    for line in content.lines() {
        if let Some(version) = line.strip_prefix("UI_VERSION=") {
            result.version = version.into();
        } else if let Some(commit) = line.strip_prefix("UI_COMMIT=") {
            result.commit = commit.into();
        }
    }
    Some(result)
}
pub(crate) static JELLYFIN_UI_VERSION: LazyLock<Option<JellyfinUiVersion>> =
    LazyLock::new(embedded_version);
