//! Only playlists are buffered; media segments retain the streaming byte path.
use futures_util::StreamExt;
use hyper::StatusCode;
use std::{sync::LazyLock, time::Duration};

use crate::{outbound::hls::PlaylistUrlTranslator, upstream::headers::remove_hop_by_hop_headers};

#[derive(thiserror::Error)]
pub(crate) enum HlsError {
    #[error("unexpected playlist response status")]
    Status(StatusCode),
    #[error("invalid upstream playlist")]
    InvalidPayload(&'static str),
    #[error("playlist transport failed")]
    Transport(#[source] reqwest::Error),
    #[error("playlist decoding failed")]
    Encoding(#[source] std::string::FromUtf8Error),
    #[error("playlist deadline exceeded")]
    Deadline(#[source] tokio::time::error::Elapsed),
    #[error("playlist URL translation failed")]
    Translation(#[source] anyhow::Error),
}

impl std::fmt::Debug for HlsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

/// Already-authorized dependencies for rewriting a selected backend's playlist.
/// The caller resolves the viewer; transport never looks up authorization or
/// selects a playback session. URL mapping is a supplied capability, not a
/// dependency on any concrete processor or application service.
pub(crate) struct HlsContext<'a> {
    translator: &'a dyn PlaylistUrlTranslator,
    timeout: Duration,
}

impl<'a> HlsContext<'a> {
    pub(crate) fn new(translator: &'a dyn PlaylistUrlTranslator, timeout_seconds: u64) -> Self {
        Self {
            translator,
            timeout: Duration::from_secs(timeout_seconds.clamp(1, 300)),
        }
    }
}

/// A rewritten playlist response; HTTP adapters own their framework body type.
pub(crate) struct PlaylistResponse {
    pub(crate) headers: hyper::HeaderMap,
    pub(crate) body: String,
}

const MAX_PLAYLIST_BYTES: usize = 8 * 1024 * 1024;
// Consume every quoted attribute, so a comma/URI= inside another quoted value
// cannot be mistaken for a new attribute.
static QUOTED_ATTRIBUTE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"(?:^|[:,])\s*([A-Z0-9-]+)="([^"]*)""#).unwrap());

pub(crate) fn is_playlist_path(url: &url::Url) -> bool {
    url.path().to_ascii_lowercase().ends_with(".m3u8")
}

pub(crate) fn is_playlist_type(headers: &hyper::HeaderMap) -> bool {
    headers
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            matches!(
                value
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase()
                    .as_str(),
                "application/vnd.apple.mpegurl"
                    | "application/x-mpegurl"
                    | "audio/mpegurl"
                    | "audio/x-mpegurl"
            )
        })
}

pub(crate) async fn response(
    context: &HlsContext<'_>,
    upstream: reqwest::Response,
    head: bool,
) -> Result<PlaylistResponse, HlsError> {
    // Do not leak partial, cached, compressed, or error-page playlist bodies.
    if upstream.status() != StatusCode::OK {
        return Err(HlsError::Status(upstream.status()));
    }
    if upstream
        .headers()
        .get(hyper::header::CONTENT_ENCODING)
        .is_some_and(|value| value != "identity")
    {
        return Err(HlsError::InvalidPayload("compressed HLS playlist"));
    }
    let playlist = upstream.url().clone();
    let mut headers = upstream.headers().clone();
    remove_hop_by_hop_headers(&mut headers);
    for header in [
        "content-length",
        "content-encoding",
        "content-range",
        "accept-ranges",
        "etag",
        "last-modified",
        "content-md5",
        "digest",
    ] {
        headers.remove(header);
    }
    headers.insert(
        hyper::header::CACHE_CONTROL,
        hyper::header::HeaderValue::from_static("private, no-store"),
    );
    let body = if head {
        String::new()
    } else {
        let text = tokio::time::timeout(context.timeout, async {
            let mut bytes = Vec::new();
            let mut stream = upstream.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|error| HlsError::Transport(error.without_url()))?;
                if chunk.len() > MAX_PLAYLIST_BYTES.saturating_sub(bytes.len()) {
                    return Err(HlsError::InvalidPayload(
                        "HLS playlist exceeds buffer limit",
                    ));
                }
                bytes.extend_from_slice(&chunk);
            }
            String::from_utf8(bytes).map_err(HlsError::Encoding)
        })
        .await
        .map_err(HlsError::Deadline)??;
        rewrite(context, &playlist, &text).await?
    };
    Ok(PlaylistResponse { headers, body })
}

async fn rewrite(
    context: &HlsContext<'_>,
    playlist: &url::Url,
    text: &str,
) -> Result<String, HlsError> {
    if !text.trim_start_matches('\u{feff}').starts_with("#EXTM3U") {
        return Err(HlsError::InvalidPayload("invalid HLS playlist signature"));
    }
    let mut output = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        let mut end = 0;
        let ranges = if trimmed.starts_with("#EXT") {
            QUOTED_ATTRIBUTE
                .captures_iter(line)
                .filter(|capture| &capture[1] == "URI")
                .map(|capture| {
                    let uri = capture.get(2).unwrap();
                    uri.start()..uri.end()
                })
                .collect::<Vec<_>>()
        } else if !trimmed.is_empty() && !trimmed.starts_with('#') {
            let start = line.len() - line.trim_start().len();
            std::iter::once(start..start + trimmed.len()).collect()
        } else {
            Vec::new()
        };
        for range in ranges {
            output.push_str(&line[end..range.start]);
            output.push_str(
                &context
                    .translator
                    .translate(&line[range.clone()], playlist)
                    .await
                    .map_err(HlsError::Translation)?,
            );
            end = range.end;
        }
        output.push_str(&line[end..]);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::{
        auth::service::UserAuthorizationService,
        config::{AppConfig, MediaStreamingMode},
        domain::ids::ServerId,
        libraries::service::VirtualLibraryService,
        media::storage::MediaStorageService,
        persistence::MIGRATOR,
        playback::SessionStorage,
        processors::{hls::HlsUrlTranslation, url_processor::UrlProcessor},
        servers::storage::ServerStorageService,
        servers::Server,
    };

    async fn dependencies() -> (UrlProcessor, Server) {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let servers = ServerStorageService::new(pool.clone());
        let media = MediaStorageService::new(pool.clone());
        let libraries = VirtualLibraryService::new(pool.clone(), servers, media.clone());
        let processor = UrlProcessor::new(
            Arc::new(media),
            Arc::new(libraries),
            Arc::new(SessionStorage::new()),
            Arc::new(UserAuthorizationService::new(pool)),
            crate::config::SettingsView::new(Arc::new(tokio::sync::RwLock::new(
                AppConfig::default(),
            ))),
        );
        let server = Server {
            id: ServerId::new(1),
            name: "Backend".into(),
            url: crate::servers::url::ServerUrl::parse("http://backend.example/base").unwrap(),
            priority: 1,
            media_streaming_mode: MediaStreamingMode::Proxy,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        (processor, server)
    }

    #[tokio::test]
    async fn explicit_context_rewrites_uri_attributes_without_authorization_lookup() {
        let (processor, server) = dependencies().await;
        // Deliberately not a stored authorization token: transport uses only
        // the already-resolved caller identity and credential supplied here.
        let translation =
            HlsUrlTranslation::new(&processor, &server, Some("proxy-token"), Some("viewer"));
        let context = HlsContext::new(&translation, 5);
        let playlist = "http://backend.example/base/hls/master.m3u8"
            .parse()
            .unwrap();
        let input = concat!(
            "#EXTM3U\r\n",
            "#EXT-X-KEY:METHOD=AES-128,URI=\"key?value=a%2Fb\"\r\n",
            "#EXT-X-MEDIA:NAME=\"text,URI=not-a-reference\",URI=\"child.m3u8\"\r\n",
            "  segment.ts?value=x%20y  \r\n",
            "https://external.example/segment.ts?api_key=external\n",
        );
        let output = rewrite(&context, &playlist, input).await.unwrap();
        assert!(output.contains("URI=\"/hls/key?value=a%2Fb&ApiKey=proxy-token\"\r\n"));
        assert!(output.contains(
            "NAME=\"text,URI=not-a-reference\",URI=\"/hls/child.m3u8?ApiKey=proxy-token\""
        ));
        assert!(output.contains("  /hls/segment.ts?value=x+y&ApiKey=proxy-token  \r\n"));
        assert!(output.ends_with("https://external.example/segment.ts?api_key=external\n"));
    }

    #[tokio::test]
    async fn explicit_context_bounds_timeout_and_rejects_non_playlists() {
        let (processor, server) = dependencies().await;
        for (configured, bounded) in [(0, 1), (5, 5), (u64::MAX, 300)] {
            let translation = HlsUrlTranslation::new(&processor, &server, None, None);
            let context = HlsContext::new(&translation, configured);
            assert_eq!(context.timeout, Duration::from_secs(bounded));
            assert!(matches!(
                rewrite(&context, &server.url, "not a playlist").await,
                Err(HlsError::InvalidPayload("invalid HLS playlist signature"))
            ));
        }
    }
}
