//! Treat the legacy guide DTO and modern query form as the same catalog policy,
//! while preserving POST and its typed body on the upstream wire.
use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    Json,
};
use hyper::StatusCode;
use serde_json::Value;

use crate::{
    extractors::CatalogPreprocessed,
    handlers::common::{payload_from_request, set_json_body},
    AppState,
};

pub async fn post_live_tv_programs(
    State(state): State<AppState>,
    request: Request,
) -> Result<Json<Value>, StatusCode> {
    let (mut parts, body) = request.into_parts();
    let bytes = to_bytes(body, 2 * 1024 * 1024)
        .await
        .map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)?;
    let payload: Value = serde_json::from_slice(&bytes).map_err(|_| StatusCode::BAD_REQUEST)?;
    let fields = payload.as_object().ok_or(StatusCode::BAD_REQUEST)?;
    let original_uri = &parts
        .extensions
        .get::<axum::extract::OriginalUri>()
        .ok_or(StatusCode::BAD_REQUEST)?
        .0;
    let mut url = url::Url::parse(&format!("http://localhost{original_uri}"))
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    // Body fields are authoritative for the POST DTO. Mirror them before server
    // selection so access checks, ID partitioning and pagination use one policy.
    let mut pairs = url
        .query_pairs()
        .filter(|(key, _)| !fields.keys().any(|field| field.eq_ignore_ascii_case(key)))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    for (key, value) in fields {
        if value.is_null() {
            continue;
        }
        let text = match value {
            Value::String(text) => text.clone(),
            Value::Bool(_) | Value::Number(_) => value.to_string(),
            Value::Array(values) => values
                .iter()
                .map(|value| value.as_str().ok_or(StatusCode::BAD_REQUEST))
                .collect::<Result<Vec<_>, _>>()?
                .join(","),
            _ => return Err(StatusCode::BAD_REQUEST),
        };
        pairs.push((key.clone(), text));
    }
    url.query_pairs_mut().clear().extend_pairs(pairs);
    parts.uri = url[url::Position::BeforePath..]
        .parse()
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    // OriginalUri normally preserves the pre-nest URI. This request now carries
    // the equivalent query policy, including the configured prefix.
    parts
        .extensions
        .insert(axum::extract::OriginalUri(parts.uri.clone()));
    let request = Request::from_parts(parts, Body::from(bytes));
    let preprocessed = crate::request_preprocessing::preprocess_catalog_request(request, &state)
        .await
        .map_err(|error| {
            tracing::error!("Failed to resolve posted guide: {error}");
            if error.is::<crate::request_preprocessing::NoAvailableBackend>() {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::BAD_REQUEST
            }
        })?;
    super::get_live_tv_programs(State(state), CatalogPreprocessed(preprocessed)).await
}

/// URL rewriting has already translated credentials/IDs and selected the page.
/// Copy that policy back to the DTO immediately before each upstream request.
pub(super) fn sync_posted_guide_body(request: &mut reqwest::Request) -> Result<(), StatusCode> {
    if request.method() != reqwest::Method::POST
        || !request
            .url()
            .path()
            .trim_end_matches('/')
            .to_ascii_lowercase()
            .ends_with("/livetv/programs")
    {
        return Ok(());
    }
    let mut payload: Value = payload_from_request(request)?;
    let fields = payload.as_object_mut().ok_or(StatusCode::BAD_REQUEST)?;
    for (key, value) in request.url().query_pairs() {
        let pagination =
            key.eq_ignore_ascii_case("StartIndex") || key.eq_ignore_ascii_case("Limit");
        if let Some((_, field)) = fields
            .iter_mut()
            .find(|(field, _)| field.eq_ignore_ascii_case(&key))
        {
            *field = if pagination {
                serde_json::from_str(&value).map_err(|_| StatusCode::BAD_REQUEST)?
            } else {
                match field {
                    Value::Array(_) => Value::Array(
                        value
                            .split(',')
                            .filter(|id| !id.is_empty())
                            .map(|id| Value::String(id.into()))
                            .collect(),
                    ),
                    Value::Bool(_) | Value::Number(_) => {
                        serde_json::from_str(&value).map_err(|_| StatusCode::BAD_REQUEST)?
                    }
                    _ => Value::String(value.into_owned()),
                }
            };
        } else if pagination {
            fields.insert(
                key.into_owned(),
                serde_json::from_str(&value).map_err(|_| StatusCode::BAD_REQUEST)?,
            );
        }
    }
    set_json_body(request, &payload)
}
