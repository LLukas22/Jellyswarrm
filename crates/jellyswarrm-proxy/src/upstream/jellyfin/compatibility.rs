//! Named Jellyfin wire compatibility normalizations, applied after translation.
use crate::{
    application::error::ApplicationError,
    upstream::jellyfin::json::{payload_from_request_typed, set_json_body_typed},
};
use serde_json::Value;

pub(crate) fn sync_posted_guide_body(
    request: &mut reqwest::Request,
) -> Result<(), ApplicationError> {
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
    let mut payload: Value = payload_from_request_typed(request)?;
    let fields = payload
        .as_object_mut()
        .ok_or(ApplicationError::InvalidInput(
            "guide body must be an object",
        ))?;
    for (key, value) in request.url().query_pairs() {
        let pagination =
            key.eq_ignore_ascii_case("StartIndex") || key.eq_ignore_ascii_case("Limit");
        if let Some((_, field)) = fields
            .iter_mut()
            .find(|(field, _)| field.eq_ignore_ascii_case(&key))
        {
            *field = if pagination {
                serde_json::from_str(&value).map_err(ApplicationError::InvalidRequestJson)?
            } else {
                match field {
                    Value::Array(_) => Value::Array(
                        value
                            .split(',')
                            .filter(|id| !id.is_empty())
                            .map(|id| Value::String(id.into()))
                            .collect(),
                    ),
                    Value::Bool(_) | Value::Number(_) => serde_json::from_str(&value)
                        .map_err(ApplicationError::InvalidRequestJson)?,
                    _ => Value::String(value.into_owned()),
                }
            };
        } else if pagination {
            fields.insert(
                key.into_owned(),
                serde_json::from_str(&value).map_err(ApplicationError::InvalidRequestJson)?,
            );
        }
    }
    set_json_body_typed(request, &payload)
}
