// use async_trait::async_trait;
// use serde_json::Value;
// use std::collections::HashSet;
// use std::sync::LazyLock;
// use tracing::info;

// use crate::processors::json_processor::{
//     JsonProcessingContext, JsonProcessingResult, JsonProcessor,
// };
// use crate::request_preprocessing::{JellyfinAuthorization, PreprocessedRequest};
// use crate::server_storage::Server;
// use crate::user_authorization_service::{AuthorizationSession, User};
// use crate::AppState;

// pub struct ResponseProcessor {}

// #[async_trait]
// impl JsonProcessor<String> for ResponseProcessor {
//     async fn process(
//         &self,
//         json_context: &JsonProcessingContext,
//         value: &mut Value,
//         context: &String,
//     ) -> JsonProcessingResult {
//         let mut result = JsonProcessingResult::new();

//         match json_context.key.as_str() {
//             // Handle ID fields that need virtual ID transformation
//             "id"
//             | "parent_id"
//             | "item_id"
//             | "etag"
//             | "series_id"
//             | "season_id"
//             | "display_preferences_id"
//             | "parent_logo_item_id"
//             | "parent_backdrop_item_id"
//             | "parent_logo_image_tag"
//             | "parent_thumb_item_id"
//             | "parent_thumb_image_tag"
//             | "series_primary_image_tag" => if let Value::String(ref id_str) = value {},

//             _ => {
//                 // Handle other fields as needed
//             }
//         }
//         result
//     }
// }
