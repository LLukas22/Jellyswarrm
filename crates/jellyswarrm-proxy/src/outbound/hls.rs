//! Capability used by playlist transport without depending on concrete translators.
#[async_trait::async_trait]
pub(crate) trait PlaylistUrlTranslator: Send + Sync {
    async fn translate(&self, reference: &str, playlist: &url::Url) -> anyhow::Result<String>;
}
