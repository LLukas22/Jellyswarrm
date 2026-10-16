use crate::application::error::ApplicationError;
use serde_json::Value;
use std::sync::Arc;

use crate::{models::MediaSource, server_storage::Server, AppState};

/// Virtualize opaque live IDs and their delivery URL references together.
pub(crate) async fn prepare_live_source(
    state: &AppState,
    source: &mut MediaSource,
    user: &str,
    server: &Server,
) -> Result<Option<Arc<crate::session_storage::PreparedLiveStream>>, ApplicationError> {
    let mut value = serde_json::to_value(&*source).map_err(ApplicationError::Serialization)?;
    let Some(original) = value
        .as_object()
        .and_then(|object| {
            object
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case("LiveStreamId"))
        })
        .and_then(|(_, value)| value.as_str())
        .filter(|id| !id.is_empty())
        .map(str::to_string)
    else {
        return Ok(None);
    };
    let plan = state
        .play_sessions
        .prepare_live_stream(&original, user, server.id)
        .await;
    rewrite_live_id(&mut value, plan.virtual_id());
    *source = serde_json::from_value(value).map_err(ApplicationError::InvalidUpstreamJson)?;
    Ok(Some(plan))
}

pub(super) fn rewrite_live_id(value: &mut Value, id: &str) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if key.eq_ignore_ascii_case("LiveStreamId") {
                    *value = Value::String(id.into());
                } else if ["TranscodingUrl", "StreamUrl", "DeliveryUrl"]
                    .iter()
                    .any(|field| key.eq_ignore_ascii_case(field))
                {
                    if let Some(url) = value.as_str() {
                        if let Some((path, query)) = url.split_once('?') {
                            let mut pairs = url::form_urlencoded::Serializer::new(String::new());
                            for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
                                pairs.append_pair(
                                    &key,
                                    if key.eq_ignore_ascii_case("LiveStreamId") {
                                        id
                                    } else {
                                        &value
                                    },
                                );
                            }
                            *value = Value::String(format!("{path}?{}", pairs.finish()));
                        }
                    }
                } else {
                    rewrite_live_id(value, id);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                rewrite_live_id(value, id);
            }
        }
        _ => {}
    }
}
