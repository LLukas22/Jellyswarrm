//! Treat the legacy guide DTO and modern query form as the same catalog policy,
//! while preserving POST and its typed body on the upstream wire.
use axum::{
    body::{to_bytes, Body},
    extract::{FromRequest, Request, State},
    Json,
};
use hyper::StatusCode;
use serde_json::Value;

use crate::{http::extractors::CatalogInput, AppState};

pub(crate) async fn post_live_tv_programs(
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
    let input = CatalogInput::from_request(request, &state).await?;
    super::get_live_tv_programs(State(state), input).await
}
