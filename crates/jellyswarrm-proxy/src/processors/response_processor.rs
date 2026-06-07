use async_trait::async_trait;
use serde_json::{Map, Value};
use tracing::debug;

use crate::{
    processors::{
        field_matcher::{
            DELIVERY_URL_FIELDS, DISABLED_BOOL_FIELDS, MEDIA_ID_ARRAY_FIELDS,
            MEDIA_ID_MAP_KEY_FIELDS, MEDIA_ID_MAP_VALUE_FIELDS, MEDIA_ID_NESTED_MAP_KEY_FIELDS,
            NAME_FIELDS, RESPONSE_MEDIA_ID_FIELDS, SERVER_ID_FIELDS,
        },
        json_processor::{JsonProcessingContext, JsonProcessingResult, JsonProcessor},
        url_processor::UrlProcessor,
    },
    server_storage::Server,
    DataContext,
};

pub struct ResponseProcessor {
    pub data_context: DataContext,
    url_processor: UrlProcessor,
}

impl ResponseProcessor {
    pub fn new(data_context: DataContext) -> Self {
        Self {
            url_processor: UrlProcessor::new(data_context.clone()),
            data_context,
        }
    }

    async fn virtual_media_id(&self, id: &str, server: &Server) -> Result<String, String> {
        self.data_context
            .media_storage
            .get_or_create_media_mapping(id, server)
            .await
            .map(|mapping| mapping.virtual_media_id)
            .map_err(|e| format!("failed to create media mapping for {id}: {e}"))
    }

    async fn remap_delivery_url(
        &self,
        value: &str,
        context: &ResponseProcessingContext,
    ) -> Result<Option<String>, String> {
        self.url_processor
            .server_to_client_delivery_url(value, &context.server, context.proxy_api_key.as_deref())
            .await
            .map_err(|e| e.to_string())
    }
}

pub struct ResponseProcessingContext {
    pub server: Server,
    pub proxy_server_id: String,
    pub proxy_api_key: Option<String>,
    pub profile: ResponseProcessingProfile,
    pub should_change_name: bool,
    pub can_change_item_names: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseProcessingProfile {
    Media,
    BestEffortMedia,
    Disabled,
}

impl ResponseProcessingContext {
    fn rewrites_media_fields(&self) -> bool {
        matches!(
            self.profile,
            ResponseProcessingProfile::Media | ResponseProcessingProfile::BestEffortMedia
        )
    }
}

#[async_trait]
impl JsonProcessor<ResponseProcessingContext> for ResponseProcessor {
    async fn process(
        &self,
        json_context: &JsonProcessingContext,
        value: &mut Value,
        context: &ResponseProcessingContext,
    ) -> JsonProcessingResult {
        let mut result = JsonProcessingResult::new();

        if context.profile == ResponseProcessingProfile::Disabled {
            return result;
        }

        if context.rewrites_media_fields()
            && json_context.is_array_item
            && MEDIA_ID_ARRAY_FIELDS.contains(last_segment(&json_context.parent_path))
        {
            if let Some(id) = value.as_str().map(str::to_string) {
                match self.virtual_media_id(&id, &context.server).await {
                    Ok(virtual_id) => {
                        debug!("Replacing response array media ID {} -> {}", id, virtual_id);
                        *value = Value::String(virtual_id);
                        result = result.mark_modified();
                    }
                    Err(e) => result = result.add_error(e),
                }
            }
            return result;
        }

        if context.rewrites_media_fields() && should_remap_map_value(&json_context.parent_path) {
            if let Some(id) = value.as_str().map(str::to_string) {
                match self.virtual_media_id(&id, &context.server).await {
                    Ok(virtual_id) => {
                        debug!("Replacing response map media ID {} -> {}", id, virtual_id);
                        *value = Value::String(virtual_id);
                        result = result.mark_modified();
                    }
                    Err(e) => result = result.add_error(e),
                }
            }
            return result;
        }

        if context.rewrites_media_fields() && should_remap_map_key(&json_context.parent_path) {
            match self
                .virtual_media_id(&json_context.key, &context.server)
                .await
            {
                Ok(virtual_id) => {
                    debug!(
                        "Replacing response map key media ID {} -> {}",
                        json_context.key, virtual_id
                    );
                    result = result.rename_key(virtual_id);
                }
                Err(e) => result = result.add_error(e),
            }
            return result;
        }

        if context.rewrites_media_fields()
            && RESPONSE_MEDIA_ID_FIELDS.contains(&json_context.key)
            && !is_legacy_unmapped_media_id_field(json_context)
        {
            if let Some(id) = value.as_str().map(str::to_string) {
                match self.virtual_media_id(&id, &context.server).await {
                    Ok(virtual_id) => {
                        debug!(
                            "Replacing response media ID {} -> {} for field {}",
                            id, virtual_id, json_context.key
                        );
                        *value = Value::String(virtual_id);
                        result = result.mark_modified();
                    }
                    Err(e) => result = result.add_error(e),
                }
            }
        } else if DELIVERY_URL_FIELDS.contains(&json_context.key) {
            if let Some(delivery_url) = value.as_str().map(str::to_string) {
                match self.remap_delivery_url(&delivery_url, context).await {
                    Ok(Some(remapped)) => {
                        *value = Value::String(remapped);
                        result = result.mark_modified();
                    }
                    Ok(None) => {}
                    Err(e) => result = result.add_error(e),
                }
            }
        } else if context.rewrites_media_fields()
            && DISABLED_BOOL_FIELDS.contains(&json_context.key)
        {
            if value.is_boolean() {
                *value = Value::Bool(false);
                result = result.mark_modified();
            }
        } else if context.rewrites_media_fields() && SERVER_ID_FIELDS.contains(&json_context.key) {
            if value.is_string() {
                *value = Value::String(context.proxy_server_id.clone());
                result = result.mark_modified();
            }
        } else if context.rewrites_media_fields() && should_change_name(json_context, context) {
            if let Value::String(name) = value {
                *name = format!("{} [{}]", name, context.server.name);
                result = result.mark_modified();
            }
        }

        result
    }
}

fn should_remap_map_value(parent_path: &str) -> bool {
    MEDIA_ID_MAP_VALUE_FIELDS.contains(last_segment(parent_path))
}

fn should_remap_map_key(parent_path: &str) -> bool {
    MEDIA_ID_MAP_KEY_FIELDS.contains(last_segment(parent_path))
        || parent_contains_nested_map_key_field(parent_path)
}

fn parent_contains_nested_map_key_field(parent_path: &str) -> bool {
    let mut seen_nested_map = false;
    for segment in path_segments(parent_path) {
        if MEDIA_ID_NESTED_MAP_KEY_FIELDS.contains(segment) {
            seen_nested_map = true;
            continue;
        }

        if seen_nested_map {
            return true;
        }
    }

    false
}

fn is_legacy_unmapped_media_id_field(json_context: &JsonProcessingContext) -> bool {
    let is_user_data_item_id = json_context.key.eq_ignore_ascii_case("ItemId")
        && path_segments(&json_context.parent_path)
            .any(|segment| segment.eq_ignore_ascii_case("UserData"));
    let is_media_source_etag = json_context.key.eq_ignore_ascii_case("Etag")
        && path_segments(&json_context.parent_path)
            .any(|segment| segment.eq_ignore_ascii_case("MediaSources"));

    is_user_data_item_id || is_media_source_etag
}

fn should_change_name(
    json_context: &JsonProcessingContext,
    context: &ResponseProcessingContext,
) -> bool {
    context.should_change_name
        && context.can_change_item_names
        && NAME_FIELDS.contains(&json_context.key)
        && is_media_item_root_path(&json_context.parent_path)
        && !is_live_tv_item(json_context.parent_object.as_ref())
}

fn is_media_item_root_path(parent_path: &str) -> bool {
    parent_path.is_empty()
        || (parent_path.starts_with('[') && !parent_path.contains('.'))
        || last_segment(parent_path).eq_ignore_ascii_case("Items")
}

fn is_live_tv_item(parent_object: Option<&Map<String, Value>>) -> bool {
    let Some(parent_object) = parent_object else {
        return false;
    };

    parent_object
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("CollectionType"))
        .and_then(|(_, value)| value.as_str())
        .is_some_and(|collection_type| collection_type.eq_ignore_ascii_case("LiveTv"))
}

fn last_segment(path: &str) -> &str {
    path.rsplit('.')
        .next()
        .map(strip_array_index)
        .unwrap_or(path)
}

fn path_segments(path: &str) -> impl Iterator<Item = &str> {
    path.split('.').map(strip_array_index)
}

fn strip_array_index(segment: &str) -> &str {
    segment.split('[').next().unwrap_or(segment)
}
