//! Target-bound redirect and streamed media delivery, independent of Axum.

use std::{io, pin::Pin};

use crate::application::error::ApplicationError;
use futures_util::{Stream, StreamExt};
use hyper::{body::Bytes, header, HeaderMap, StatusCode};

use super::hls;
use crate::upstream::headers::remove_hop_by_hop_headers;

/// Concrete transport dependencies, with authorization resolved by the caller.
pub(crate) struct DeliveryContext<'a> {
    streaming_client: &'a reqwest::Client,
    hls: hls::HlsContext<'a>,
}

impl<'a> DeliveryContext<'a> {
    pub(crate) fn new(streaming_client: &'a reqwest::Client, hls: hls::HlsContext<'a>) -> Self {
        Self {
            streaming_client,
            hls,
        }
    }
}

pub(crate) type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>;

pub(crate) enum DeliveryBody {
    Empty,
    Stream(ByteStream),
    Playlist(String),
}

pub(crate) struct DeliveryResponse {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) body: DeliveryBody,
}

/// Encode a backend-owned temporary redirect using already selected credentials.
pub(crate) fn redirect(
    request: &reqwest::Request,
    backend_token: Option<&str>,
) -> Result<DeliveryResponse, ApplicationError> {
    let mut url = request.url().clone();
    // Redirects cannot carry the upstream Authorization header. Replace any
    // existing query credential with Jellyfin's canonical query token.
    if let Some(token) = backend_token {
        let pairs = url
            .query_pairs()
            .filter(|(key, _)| {
                !crate::processors::url_processor::matches_case_insensitive(
                    key,
                    crate::processors::url_processor::API_KEY_QUERY_TAGS,
                )
            })
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<Vec<_>>();
        url.query_pairs_mut()
            .clear()
            .extend_pairs(pairs)
            .append_pair("ApiKey", token);
    }
    let mut headers = HeaderMap::new();
    headers.insert(
        header::LOCATION,
        header::HeaderValue::from_str(url.as_ref())
            .map_err(|error| ApplicationError::Internal(error.into()))?,
    );
    headers.insert(
        header::REFERRER_POLICY,
        header::HeaderValue::from_static("no-referrer"),
    );
    Ok(DeliveryResponse {
        status: StatusCode::TEMPORARY_REDIRECT,
        headers,
        body: DeliveryBody::Empty,
    })
}

/// Execute a resolved request without selecting or changing its backend.
/// Binary bodies are never collected; only HLS playlists are buffered.
pub(crate) async fn proxy(
    context: &DeliveryContext<'_>,
    mut request: reqwest::Request,
) -> Result<DeliveryResponse, ApplicationError> {
    let playlist_path = hls::is_playlist_path(request.url());
    let head = request.method() == reqwest::Method::HEAD;
    request.headers_mut().insert(
        header::ACCEPT_ENCODING,
        header::HeaderValue::from_static("identity"),
    );
    if playlist_path {
        // Never serve a partial playlist or revalidate a pre-rewrite version.
        for header in [
            header::RANGE,
            header::IF_RANGE,
            header::IF_NONE_MATCH,
            header::IF_MODIFIED_SINCE,
        ] {
            request.headers_mut().remove(header);
        }
    }
    let response = context
        .streaming_client
        .execute(request)
        .await
        .map_err(|error| ApplicationError::UpstreamTransport(error.without_url()))?;

    if playlist_path || hls::is_playlist_type(response.headers()) {
        let playlist = hls::response(&context.hls, response, head).await?;
        return Ok(DeliveryResponse {
            status: StatusCode::OK,
            headers: playlist.headers,
            body: if head {
                DeliveryBody::Empty
            } else {
                DeliveryBody::Playlist(playlist.body)
            },
        });
    }

    let status = response.status();
    let mut headers = response.headers().clone();
    remove_hop_by_hop_headers(&mut headers);
    let stream = response
        .bytes_stream()
        .map(|result| result.map_err(|error| io::Error::other(error.without_url())));
    Ok(DeliveryResponse {
        status,
        headers,
        body: DeliveryBody::Stream(Box::pin(stream)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_replaces_credentials_and_preserves_resource_query() {
        let request = reqwest::Client::new()
            .get("http://upstream/Videos/item/stream?api_key=old&Range=1&Static=true")
            .build()
            .unwrap();
        let response = redirect(&request, Some("backend-token")).unwrap();
        assert_eq!(response.status, StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(response.headers[header::REFERRER_POLICY], "no-referrer");
        assert!(matches!(response.body, DeliveryBody::Empty));
        let url = url::Url::parse(response.headers[header::LOCATION].to_str().unwrap()).unwrap();
        assert_eq!(
            url.query_pairs().collect::<Vec<_>>(),
            vec![
                ("Range".into(), "1".into()),
                ("Static".into(), "true".into()),
                ("ApiKey".into(), "backend-token".into()),
            ]
        );
    }

    #[test]
    fn redirect_without_token_keeps_original_url() {
        let request = reqwest::Client::new()
            .get("http://upstream/Audio/item/universal?Static=true")
            .build()
            .unwrap();
        let response = redirect(&request, None).unwrap();
        assert_eq!(response.headers[header::LOCATION], request.url().as_str());
    }
}
