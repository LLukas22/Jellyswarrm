//! Convert incoming Axum requests into the buffered request form the application
//! workflows operate on. Caller identity and backend collection live in
//! `application::request::collect`.
use anyhow::{anyhow, Result};
use axum::{
    extract::{OriginalUri, Request},
    http,
};
use http_body_util::BodyExt;

use crate::{
    application::{
        accounts::{self, RequestIdentity},
        media_services::MediaServices,
        request::{collect, input::OperationRequest},
    },
    protocol::routes::PlaybackSessionAction,
};

/// Credential parsing reads the query, so give the request URI an absolute form.
pub(crate) fn request_url(uri: &http::Uri) -> Result<url::Url> {
    let path_and_query = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    Ok(url::Url::parse(&format!(
        "http://localhost{path_and_query}"
    ))?)
}

pub(crate) async fn resolve_request_identity(
    headers: &http::HeaderMap,
    uri: &http::Uri,
    state: &crate::auth::UserAuthorizationService,
) -> Result<RequestIdentity> {
    Ok(accounts::resolve_identity(headers, &request_url(uri)?, state).await?)
}

/// Buffer the request and collect its caller context in one step.
pub(crate) async fn collect_operation(
    req: Request,
    state: &MediaServices,
    playback_session_action: Option<PlaybackSessionAction>,
) -> Result<OperationRequest> {
    collect::operation(
        into_buffered_request(req).await?,
        state,
        playback_session_action,
    )
    .await
}

/// Buffer the body and restore the full original URI (including the URL prefix,
/// which `Router::nest` strips from `req.uri()`) on a `localhost` base.
pub(crate) async fn into_buffered_request(req: Request) -> Result<reqwest::Request> {
    let original_uri = req
        .extensions()
        .get::<OriginalUri>()
        .ok_or_else(|| anyhow!("missing original request URI"))?;
    let path_and_query = original_uri
        .path_and_query()
        .ok_or_else(|| anyhow!("missing request path and query"))?;

    let uri_with_host = http::uri::Builder::new()
        .scheme("http")
        .authority("localhost")
        .path_and_query(path_and_query.to_string())
        .build()?;

    let (parts, body) = req.into_parts();
    let body_bytes = body.collect().await?.to_bytes();

    let mut http_req = http::Request::from_parts(parts, reqwest::Body::from(body_bytes));
    *http_req.uri_mut() = uri_with_host;

    Ok(reqwest::Request::try_from(http_req)?)
}
