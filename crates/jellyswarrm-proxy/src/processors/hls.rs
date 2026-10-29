//! Bind URL mapping to an already selected server and caller for playlist transport.
use super::url_processor::UrlProcessor;
use crate::{outbound::hls::PlaylistUrlTranslator, servers::Server};

pub(crate) struct HlsUrlTranslation<'a> {
    urls: &'a UrlProcessor,
    server: &'a Server,
    proxy_api_key: Option<&'a str>,
    viewer: Option<&'a str>,
}

impl<'a> HlsUrlTranslation<'a> {
    pub(crate) fn new(
        urls: &'a UrlProcessor,
        server: &'a Server,
        proxy_api_key: Option<&'a str>,
        viewer: Option<&'a str>,
    ) -> Self {
        Self {
            urls,
            server,
            proxy_api_key,
            viewer,
        }
    }
}

#[async_trait::async_trait]
impl PlaylistUrlTranslator for HlsUrlTranslation<'_> {
    async fn translate(&self, reference: &str, playlist: &url::Url) -> anyhow::Result<String> {
        self.urls
            .server_to_client_hls_url(
                reference,
                playlist,
                self.server,
                self.proxy_api_key,
                self.viewer,
            )
            .await
    }
}
