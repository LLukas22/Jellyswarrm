use anyhow::Result;
use async_recursion::async_recursion;
use async_trait::async_trait;
use serde_json::{Map, Value};
use std::boxed::Box;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct JsonProcessingContext {
    pub path: String,
    pub key: String,
    pub parent_path: String,
    pub depth: usize,
    pub is_array_item: bool,
    pub array_index: Option<usize>,
    pub parent_object: Option<Map<String, Value>>, // Access to parent for conditional logic
}

#[derive(Debug)]
pub struct JsonProcessingResult {
    pub new_key: Option<String>,
    pub remove: bool,
    pub add_fields: HashMap<String, Value>,
    pub modified: bool,
    pub errors: Vec<String>,
}

impl JsonProcessingResult {
    pub fn new() -> Self {
        Self {
            new_key: None,
            remove: false,
            add_fields: HashMap::new(),
            modified: false,
            errors: Vec::new(),
        }
    }

    pub fn rename_key(mut self, new_key: String) -> Self {
        self.new_key = Some(new_key);
        self.modified = true;
        self
    }

    pub fn remove_field(mut self) -> Self {
        self.remove = true;
        self.modified = true;
        self
    }

    pub fn add_field(mut self, key: String, value: Value) -> Self {
        self.add_fields.insert(key, value);
        self.modified = true;
        self
    }

    pub fn mark_modified(mut self) -> Self {
        self.modified = true;
        self
    }

    pub fn add_error(mut self, error: String) -> Self {
        self.errors.push(error);
        self
    }
}

#[derive(Debug)]
pub struct AsyncProcessingResponse {
    pub data: Value,
    pub was_modified: bool,
    pub errors: Vec<String>,
}

#[async_trait]
pub trait AsyncJsonProcessor<C>: Send + Sync {
    async fn process(
        &self,
        json_context: &JsonProcessingContext,
        value: &mut Value,
        context: &C,
    ) -> JsonProcessingResult;
}

#[async_recursion]
async fn process_json_advanced<'a, P: AsyncJsonProcessor<C>, C: Send + Sync>(
    value: &'a mut Value,
    processor: &'a P,
    context: &'a C,
    parent_path: &'a str,
    depth: usize,
    parent_object: Option<&'a Map<String, Value>>,
    errors: &'a mut Vec<String>,
) -> Result<(Map<String, Value>, bool)> {
    let mut new_map = Map::new();
    let mut was_modified = false;

    if let Value::Object(map) = value {
        // Clone the map once to avoid borrowing conflicts
        let map_clone = map.clone();

        for (key, val) in map.iter_mut() {
            let current_path = if parent_path.is_empty() {
                key.clone()
            } else {
                format!("{}.{}", parent_path, key)
            };

            let json_context = JsonProcessingContext {
                path: current_path.clone(),
                key: key.clone(),
                parent_path: parent_path.to_string(),
                depth,
                is_array_item: false,
                array_index: None,
                parent_object: parent_object.cloned(),
            };

            // Process nested structures
            match val {
                Value::Object(_) => {
                    let (nested_result, nested_modified) = process_json_advanced(
                        val,
                        processor,
                        &context,
                        &current_path,
                        depth + 1,
                        Some(&map_clone),
                        errors,
                    )
                    .await?;
                    *val = Value::Object(nested_result);
                    if nested_modified {
                        was_modified = true;
                    }
                }
                Value::Array(arr) => {
                    for (index, item) in arr.iter_mut().enumerate() {
                        if let Value::Object(_) = item {
                            let array_path = format!("{}[{}]", current_path, index);
                            let (nested_result, nested_modified) = process_json_advanced(
                                item,
                                processor,
                                &context,
                                &array_path,
                                depth + 1,
                                Some(&map_clone),
                                errors,
                            )
                            .await?;
                            *item = Value::Object(nested_result);
                            if nested_modified {
                                was_modified = true;
                            }
                        }
                    }
                }
                _ => {}
            }

            // Process current field
            let result = processor.process(&json_context, val, context).await;

            // Handle any errors
            for error in result.errors {
                errors.push(error);
            }

            // Track modifications
            if result.modified {
                was_modified = true;
            }

            // Apply processing result
            if result.remove {
                was_modified = true;
            } else {
                let final_key = if let Some(new_key) = result.new_key {
                    was_modified = true;
                    new_key
                } else {
                    key.clone()
                };

                new_map.insert(final_key, val.clone());

                // Add any additional fields
                for (add_key, add_value) in result.add_fields {
                    new_map.insert(add_key, add_value);
                    was_modified = true;
                }
            }
        }
    }

    Ok((new_map, was_modified))
}

pub async fn process_json_async<P: AsyncJsonProcessor<C>, C: Send + Sync>(
    data: &mut Value,
    processor: &P,
    context: &C,
) -> Result<AsyncProcessingResponse> {
    let mut errors = Vec::new();

    let was_modified = if let Value::Object(_) = data {
        let (processed_map, modified) =
            process_json_advanced(data, processor, context, "", 0, None, &mut errors).await?;
        *data = Value::Object(processed_map);
        modified
    } else {
        false
    };

    Ok(AsyncProcessingResponse {
        data: data.clone(),
        was_modified,
        errors,
    })
}
