use anyhow::{anyhow, Result};
use async_recursion::async_recursion;
use async_trait::async_trait;
use serde_json::{Map, Value};
use std::boxed::Box;
use std::collections::HashMap;

#[allow(dead_code)]
#[derive(Debug, Clone)]
/// Context provided to the processor/analyzer for each JSON field
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
/// Result of processing a single JSON field
pub struct JsonProcessingResult {
    pub new_key: Option<String>,
    pub remove: bool,
    pub add_fields: HashMap<String, Value>,
    pub modified: bool,
    pub errors: Vec<String>,
}

#[allow(dead_code)]
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
pub struct ProcessingResponse {
    pub data: Value,
    pub was_modified: bool,
}

#[async_trait]
pub trait JsonProcessor<C>: Send + Sync {
    async fn process(
        &self,
        json_context: &JsonProcessingContext,
        value: &mut Value,
        context: &C,
    ) -> JsonProcessingResult;
}

#[async_trait]
/// A generic JSON analyzer trait that can be implemented to analyze JSON data.
pub trait JsonAnalyzer<C, A>: Send + Sync {
    async fn analyze(
        &self,
        json_context: &JsonProcessingContext,
        value: &Value,
        context: &C,
        accumulator: &mut A,
    ) -> Result<Option<Vec<String>>>; // Returns any errors encountered during analysis
}

#[async_recursion]
async fn _process_json<'a, P: JsonProcessor<C>, C: Send + Sync>(
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
                    let (nested_result, nested_modified) = _process_json(
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
                            let (nested_result, nested_modified) = _process_json(
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

/// Process a JSON Value using the provided JsonProcessor and context.
pub async fn process_json<P: JsonProcessor<C>, C: Send + Sync>(
    data: &mut Value,
    processor: &P,
    context: &C,
) -> Result<ProcessingResponse> {
    let mut errors = Vec::new();

    let was_modified = if let Value::Object(_) = data {
        let (processed_map, modified) =
            _process_json(data, processor, context, "", 0, None, &mut errors).await?;
        *data = Value::Object(processed_map);
        modified
    } else {
        false
    };

    if errors.is_empty() {
        Ok(ProcessingResponse {
            data: data.clone(),
            was_modified,
        })
    } else {
        // Join errors into a readable multi-line string for better diagnostics
        let joined = errors.join("\n- ");
        Err(anyhow!(
            "Encountered {} error(s) during processing:\n- {}",
            errors.len(),
            joined
        ))
    }
}

#[async_recursion]
async fn _analyze_json<'a, A: JsonAnalyzer<C, Acc>, C: Send + Sync, Acc: Send + Sync>(
    value: &'a Value,
    analyzer: &'a A,
    context: &'a C,
    accumulator: &'a mut Acc,
    parent_path: &'a str,
    depth: usize,
    parent_object: Option<&'a Map<String, Value>>,
    errors: &'a mut Vec<String>,
) -> Result<()> {
    if let Value::Object(map) = value {
        for (key, val) in map.iter() {
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

            // Analyze current field
            match analyzer
                .analyze(&json_context, val, context, accumulator)
                .await
            {
                Ok(analysis_errors) => {
                    if let Some(mut analysis_errors) = analysis_errors {
                        errors.append(&mut analysis_errors);
                    }
                }
                Err(e) => {
                    errors.push(format!("Analysis error at {}: {}", current_path, e));
                }
            }

            // Recursively analyze nested structures
            match val {
                Value::Object(_) => {
                    _analyze_json(
                        val,
                        analyzer,
                        context,
                        accumulator,
                        &current_path,
                        depth + 1,
                        Some(map),
                        errors,
                    )
                    .await?;
                }
                Value::Array(arr) => {
                    for (index, item) in arr.iter().enumerate() {
                        let array_json_context = JsonProcessingContext {
                            path: format!("{}[{}]", current_path, index),
                            key: index.to_string(),
                            parent_path: current_path.clone(),
                            depth: depth + 1,
                            is_array_item: true,
                            array_index: Some(index),
                            parent_object: parent_object.cloned(),
                        };

                        // Analyze array item
                        match analyzer
                            .analyze(&array_json_context, item, context, accumulator)
                            .await
                        {
                            Ok(analysis_errors) => {
                                if let Some(mut analysis_errors) = analysis_errors {
                                    errors.append(&mut analysis_errors);
                                }
                            }
                            Err(e) => {
                                errors.push(format!(
                                    "Analysis error at {}[{}]: {}",
                                    current_path, index, e
                                ));
                            }
                        }

                        if let Value::Object(_) = item {
                            let array_path = format!("{}[{}]", current_path, index);
                            _analyze_json(
                                item,
                                analyzer,
                                context,
                                accumulator,
                                &array_path,
                                depth + 1,
                                Some(map),
                                errors,
                            )
                            .await?;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    Ok(())
}

/// Analyze a JSON Value using the provided JsonAnalyzer and context, accumulates the results into the provided accumulator.
pub async fn analyze_json<A: JsonAnalyzer<C, Acc>, C: Send + Sync, Acc: Send + Sync>(
    data: &Value,
    analyzer: &A,
    context: &C,
    mut accumulator: Acc,
) -> Result<Acc> {
    let mut errors = Vec::new();

    if let Value::Object(_) = data {
        _analyze_json(
            data,
            analyzer,
            context,
            &mut accumulator,
            "",
            0,
            None,
            &mut errors,
        )
        .await?;
    }

    if errors.is_empty() {
        Ok(accumulator)
    } else {
        // Join errors into a readable multi-line string for better diagnostics
        let joined = errors.join("\n- ");
        Err(anyhow!(
            "Encountered {} error(s) during analysis:\n- {}",
            errors.len(),
            joined
        ))
    }
}
