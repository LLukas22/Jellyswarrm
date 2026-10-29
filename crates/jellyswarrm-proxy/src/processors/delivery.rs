//! Backend-to-client delivery translation. No backend selection or account access.
use std::sync::Arc;

use anyhow::Result;

use super::field_matcher::{matches_case_insensitive, API_KEY_QUERY_TAGS, MEDIA_ID_QUERY_TAGS};
use crate::{media::MediaStorageService, servers::Server, upstream::url::is_id_like};

#[derive(Clone)]
pub(crate) struct DeliveryUrlTranslator {
    media: Arc<MediaStorageService>,
}

impl DeliveryUrlTranslator {
    pub(crate) fn new(media: Arc<MediaStorageService>) -> Self {
        Self { media }
    }

    pub(crate) async fn translate(
        &self,
        value: &str,
        server: &Server,
        proxy_api_key: Option<&str>,
    ) -> Result<Option<String>> {
        let Some((mut url, style)) = parse_delivery_url(value) else {
            return Ok(None);
        };
        self.remap_path(&mut url, server).await?;
        self.remap_query(&mut url, server, proxy_api_key).await?;
        Ok(Some(format_delivery_url(url, style)))
    }

    pub(crate) async fn remap_path(&self, url: &mut url::Url, server: &Server) -> Result<()> {
        let Some(segments) = url.path_segments() else {
            return Ok(());
        };
        let mut changed = false;
        let mut remapped_segments = Vec::new();
        for segment in segments {
            if is_id_like(segment) {
                remapped_segments.push(self.virtual_media_id(segment, server).await?);
                changed = true;
            } else {
                remapped_segments.push(segment.to_string());
            }
        }
        if changed {
            url.set_path(&remapped_segments.join("/"));
        }
        Ok(())
    }

    pub(crate) async fn remap_query(
        &self,
        url: &mut url::Url,
        server: &Server,
        proxy_api_key: Option<&str>,
    ) -> Result<()> {
        let Some(query) = url.query() else {
            return Ok(());
        };
        let mut changed = false;
        let mut pairs = Vec::new();
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            if matches_case_insensitive(&key, API_KEY_QUERY_TAGS) {
                changed = true;
                if let Some(proxy_api_key) = proxy_api_key {
                    pairs.push((key.into_owned(), proxy_api_key.to_string()));
                }
                continue;
            }
            let value = if matches_case_insensitive(&key, MEDIA_ID_QUERY_TAGS) {
                let remapped = self.remap_query_value(&value, server).await?;
                changed |= remapped != value;
                remapped
            } else {
                value.into_owned()
            };
            pairs.push((key.into_owned(), value));
        }
        if changed {
            url.query_pairs_mut().clear().extend_pairs(pairs);
        }
        Ok(())
    }

    async fn remap_query_value(&self, value: &str, server: &Server) -> Result<String> {
        let mut changed = false;
        let mut remapped_ids = Vec::new();
        for raw_id in value.split(',') {
            let id = raw_id.trim();
            if id.is_empty() {
                continue;
            }
            if is_id_like(id) {
                remapped_ids.push(self.virtual_media_id(id, server).await?);
                changed = true;
            } else {
                remapped_ids.push(id.to_string());
            }
        }
        Ok(if changed {
            remapped_ids.join(",")
        } else {
            value.to_string()
        })
    }

    async fn virtual_media_id(&self, id: &str, server: &Server) -> Result<String> {
        Ok(self
            .media
            .get_or_create_media_mapping(id, server)
            .await?
            .virtual_media_id)
    }
}

#[derive(Clone, Copy)]
enum DeliveryUrlStyle {
    Absolute,
    RootRelative,
    Relative,
}

fn parse_delivery_url(value: &str) -> Option<(url::Url, DeliveryUrlStyle)> {
    if let Ok(url) = url::Url::parse(value) {
        return Some((url, DeliveryUrlStyle::Absolute));
    }
    let (path, style) = if value.starts_with('/') {
        (value.to_string(), DeliveryUrlStyle::RootRelative)
    } else {
        (format!("/{value}"), DeliveryUrlStyle::Relative)
    };
    url::Url::parse(&format!("http://localhost{path}"))
        .ok()
        .map(|url| (url, style))
}

fn format_delivery_url(url: url::Url, style: DeliveryUrlStyle) -> String {
    match style {
        DeliveryUrlStyle::Absolute => url.to_string(),
        DeliveryUrlStyle::RootRelative => relative_url_from_parts(&url),
        DeliveryUrlStyle::Relative => relative_url_from_parts(&url)
            .strip_prefix('/')
            .unwrap_or(url.path())
            .to_string(),
    }
}

pub(super) fn relative_url_from_parts(url: &url::Url) -> String {
    let mut value = url.path().to_string();
    if let Some(query) = url.query() {
        value.push('?');
        value.push_str(query);
    }
    if let Some(fragment) = url.fragment() {
        value.push('#');
        value.push_str(fragment);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn delivery_translation_needs_only_media_storage_and_a_resolved_server() {
        use crate::{
            config::MediaStreamingMode, persistence::MIGRATOR, servers::ServerStorageService,
        };
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let registry = ServerStorageService::new(pool.clone());
        let id = registry
            .add_server(
                "Backend",
                "http://backend.example",
                100,
                MediaStreamingMode::Proxy,
            )
            .await
            .unwrap();
        let server = registry.get_server_by_id(id).await.unwrap().unwrap();
        let media = Arc::new(MediaStorageService::new(pool));
        let original = "11111111111111111111111111111111";
        let mapped = media
            .get_or_create_media_mapping(original, &server)
            .await
            .unwrap()
            .virtual_media_id;
        let translator = DeliveryUrlTranslator::new(media);
        let source = format!("/Videos/{original}/stream?MediaSourceId={original}&ApiKey=backend-secret&Tag=opaque#part");
        let translated = translator
            .translate(&source, &server, Some("proxy-key"))
            .await
            .unwrap()
            .unwrap();
        let url = url::Url::parse(&format!("http://proxy{translated}")).unwrap();
        assert_eq!(url.path(), format!("/Videos/{mapped}/stream"));
        let query = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(query["MediaSourceId"], mapped);
        assert_eq!(query["ApiKey"], "proxy-key");
        assert_eq!(query["Tag"], "opaque");
        assert_eq!(url.fragment(), Some("part"));
        assert!(!translated.contains("backend-secret"));

        let anonymous = translator
            .translate(&source, &server, None)
            .await
            .unwrap()
            .unwrap();
        assert!(!anonymous.contains("ApiKey"));
        assert!(!anonymous.contains("backend-secret"));
    }

    #[test]
    fn delivery_url_round_trip_preserves_style_query_and_fragment() {
        for value in [
            "https://backend.example/Videos/item?Tag=opaque#part",
            "/Videos/item?Tag=opaque#part",
            "Videos/item?Tag=opaque#part",
        ] {
            let (url, style) = parse_delivery_url(value).unwrap();
            assert_eq!(format_delivery_url(url, style), value);
        }
    }
}
