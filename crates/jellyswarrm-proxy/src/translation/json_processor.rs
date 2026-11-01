//! Walk JSON recursively and apply field analyzers or request/response transformations.
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Map, Value};
use std::boxed::Box;
use std::collections::HashMap;

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
/// Context provided to the processor/analyzer for each JSON field
pub(crate) struct JsonProcessingContext {
    pub(crate) path: String,
    pub(crate) key: String,
    pub(crate) parent_path: String,
    pub(crate) depth: usize,
    pub(crate) is_array_item: bool,
    pub(crate) array_index: Option<usize>,
    pub(crate) parent_object: Option<Map<String, Value>>, // Access to parent for conditional logic
}

#[derive(Debug)]
/// Result of processing a single JSON field
pub(crate) struct JsonProcessingResult {
    pub(crate) new_key: Option<String>,
    pub(crate) remove: bool,
    pub(crate) add_fields: HashMap<String, Value>,
    pub(crate) modified: bool,
    pub(crate) errors: Vec<anyhow::Error>,
}

#[allow(dead_code)]
impl JsonProcessingResult {
    pub(crate) fn new() -> Self {
        Self {
            new_key: None,
            remove: false,
            add_fields: HashMap::new(),
            modified: false,
            errors: Vec::new(),
        }
    }

    pub(crate) fn rename_key(mut self, new_key: String) -> Self {
        self.new_key = Some(new_key);
        self.modified = true;
        self
    }

    pub(crate) fn remove_field(mut self) -> Self {
        self.remove = true;
        self.modified = true;
        self
    }

    pub(crate) fn add_field(mut self, key: String, value: Value) -> Self {
        self.add_fields.insert(key, value);
        self.modified = true;
        self
    }

    pub(crate) fn mark_modified(mut self) -> Self {
        self.modified = true;
        self
    }

    pub(crate) fn add_error(mut self, error: String) -> Self {
        self.errors.push(anyhow::Error::msg(error));
        self
    }

    pub(crate) fn add_cause(mut self, error: anyhow::Error) -> Self {
        self.errors.push(error);
        self
    }
}

/// Retain every field failure. The standard error chain exposes the first
/// cause; callers needing all failures can inspect `causes` without parsing text.
#[derive(Debug)]
pub(crate) struct ProcessingErrors {
    pub(crate) causes: Vec<anyhow::Error>,
}

impl std::fmt::Display for ProcessingErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Encountered {} error(s) during processing:",
            self.causes.len()
        )?;
        for cause in &self.causes {
            write!(f, "\n- {cause}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ProcessingErrors {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.causes.first().map(|cause| cause.as_ref() as _)
    }
}

/// Inspect all aggregated field failures, including nested aggregates and
/// context wrappers. `Error::source` alone only visits the first sibling.
impl ProcessingErrors {
    pub(crate) fn contains_cause<E: std::error::Error + 'static>(
        error: &(dyn std::error::Error + 'static),
    ) -> bool {
        if error.is::<E>() {
            return true;
        }
        if let Some(errors) = error.downcast_ref::<ProcessingErrors>() {
            return errors
                .causes
                .iter()
                .any(|cause| Self::contains_cause::<E>(cause.as_ref()));
        }
        error.source().is_some_and(Self::contains_cause::<E>)
    }
}

#[derive(Debug)]
pub(crate) struct ProcessingResponse {
    pub(crate) was_modified: bool,
}

#[async_trait]
pub(crate) trait JsonProcessor<C>: Send + Sync {
    async fn process(
        &self,
        json_context: &JsonProcessingContext,
        value: &mut Value,
        context: &C,
    ) -> JsonProcessingResult;
}

#[async_trait]
/// Read-only counterpart of JsonProcessor, under the same traversal contract.
pub(crate) trait JsonAnalyzer<C, A>: Send + Sync {
    async fn analyze(
        &self,
        json_context: &JsonProcessingContext,
        value: &Value,
        context: &C,
        accumulator: &mut A,
    ) -> Result<()>;
}

async fn _process_json<'a, P, C>(
    value: &'a mut Value,
    processor: &'a P,
    context: &'a C,
    parent_path: &'a str,
    depth: usize,
    parent_object: Option<&'a Map<String, Value>>,
    errors: &'a mut Vec<anyhow::Error>,
) -> Result<(Map<String, Value>, bool)>
where
    P: JsonProcessor<C> + Send + Sync,
    C: Send + Sync,
{
    let mut new_map = Map::new();
    let mut was_modified = false;

    if let Value::Object(map) = value {
        let original_map = std::mem::take(map);
        let parent_context_object =
            parent_context_object(&original_map).or_else(|| parent_object.cloned());

        for (key, mut val) in original_map {
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
                parent_object: parent_context_object.clone(),
            };

            // Process nested structures
            match &mut val {
                Value::Object(_) => {
                    let (nested_result, nested_modified) = Box::pin(_process_json(
                        &mut val,
                        processor,
                        context,
                        &current_path,
                        depth + 1,
                        parent_context_object.as_ref(),
                        errors,
                    ))
                    .await?;
                    val = Value::Object(nested_result);
                    if nested_modified {
                        was_modified = true;
                    }
                }
                Value::Array(_) => {
                    was_modified |= Box::pin(_process_array_items(
                        &mut val,
                        processor,
                        context,
                        &current_path,
                        depth + 1,
                        parent_context_object.as_ref(),
                        errors,
                    ))
                    .await?;
                }
                _ => {}
            }

            // Process current field
            let result = processor.process(&json_context, &mut val, context).await;

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

                new_map.insert(final_key, val);

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

fn parent_context_object(map: &Map<String, Value>) -> Option<Map<String, Value>> {
    let mut context = Map::new();

    if let Some((key, value)) = map
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("CollectionType"))
    {
        context.insert(key.clone(), value.clone());
    }

    if let Some((_, value)) = map.iter().find(|(key, _)| key.eq_ignore_ascii_case("Type")) {
        context.insert("Type".to_string(), value.clone());
    }
    (!context.is_empty()).then_some(context)
}

async fn _process_array_items<'a, P, C>(
    value: &'a mut Value,
    processor: &'a P,
    context: &'a C,
    parent_path: &'a str,
    depth: usize,
    parent_object: Option<&'a Map<String, Value>>,
    errors: &'a mut Vec<anyhow::Error>,
) -> Result<bool>
where
    P: JsonProcessor<C> + Send + Sync,
    C: Send + Sync,
{
    let mut was_modified = false;

    let Value::Array(arr) = value else {
        return Ok(false);
    };

    for (index, item) in arr.iter_mut().enumerate() {
        let array_path = format!("{}[{}]", parent_path, index);

        if let Value::Object(_) = item {
            let (nested_result, nested_modified) = Box::pin(_process_json(
                item,
                processor,
                context,
                &array_path,
                depth,
                parent_object,
                errors,
            ))
            .await?;
            *item = Value::Object(nested_result);
            if nested_modified {
                was_modified = true;
            }
            continue;
        }

        if let Value::Array(_) = item {
            if Box::pin(_process_array_items(
                item,
                processor,
                context,
                &array_path,
                depth + 1,
                parent_object,
                errors,
            ))
            .await?
            {
                was_modified = true;
            }
            continue;
        }

        let json_context = JsonProcessingContext {
            path: array_path,
            key: index.to_string(),
            parent_path: parent_path.to_string(),
            depth,
            is_array_item: true,
            array_index: Some(index),
            parent_object: parent_object.cloned(),
        };

        let result = processor.process(&json_context, item, context).await;

        for error in result.errors {
            errors.push(error);
        }

        if result.modified {
            was_modified = true;
        }
    }

    Ok(was_modified)
}

/// Mutate in place, returning metadata only. Both visitors walk object fields
/// post-order (including container-valued fields), and scalar array elements.
/// Container array elements are traversed, not separately visited. Root arrays
/// and arbitrarily nested arrays use the same rule. Root fields have depth 0;
/// entering an object/array adds one level except array-contained object fields,
/// whose depth is the array element's depth. Parent context is the nearest
/// original Type/CollectionType projection, never partially transformed data.
/// Errors retain their causes; mutation may be partial on failure/cancellation.
pub(crate) async fn process_json<P, C>(
    data: &mut Value,
    processor: &P,
    context: &C,
) -> Result<ProcessingResponse>
where
    P: JsonProcessor<C> + Send + Sync,
    C: Send + Sync,
{
    let mut errors = Vec::new();

    let was_modified = match data {
        Value::Object(_) => {
            let (processed_map, modified) =
                _process_json(data, processor, context, "", 0, None, &mut errors).await?;
            *data = Value::Object(processed_map);
            modified
        }
        Value::Array(_) => {
            _process_array_items(data, processor, context, "", 0, None, &mut errors).await?
        }
        _ => false,
    };

    if errors.is_empty() {
        Ok(ProcessingResponse { was_modified })
    } else {
        Err(ProcessingErrors { causes: errors }.into())
    }
}

#[allow(clippy::too_many_arguments)]
async fn _analyze_json<'a, A, C, Acc>(
    value: &'a Value,
    analyzer: &'a A,
    context: &'a C,
    accumulator: &'a mut Acc,
    parent_path: &'a str,
    depth: usize,
    parent_object: Option<&'a Map<String, Value>>,
    errors: &'a mut Vec<anyhow::Error>,
) -> Result<()>
where
    A: JsonAnalyzer<C, Acc> + Send + Sync,
    C: Send + Sync,
    Acc: Send + Sync,
{
    if let Value::Object(map) = value {
        let parent_context = parent_context_object(map).or_else(|| parent_object.cloned());
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
                parent_object: parent_context.clone(),
            };

            // Recursively analyze nested structures
            match val {
                Value::Object(_) => {
                    Box::pin(_analyze_json(
                        val,
                        analyzer,
                        context,
                        accumulator,
                        &current_path,
                        depth + 1,
                        parent_context.as_ref(),
                        errors,
                    ))
                    .await?;
                }
                Value::Array(_) => {
                    Box::pin(_analyze_array(
                        val,
                        analyzer,
                        context,
                        accumulator,
                        &current_path,
                        depth + 1,
                        parent_context.as_ref(),
                        errors,
                    ))
                    .await?;
                }
                _ => {}
            }
            if let Err(error) = analyzer
                .analyze(&json_context, val, context, accumulator)
                .await
            {
                errors.push(error);
            }
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn _analyze_array<A, C, Acc>(
    value: &Value,
    analyzer: &A,
    context: &C,
    accumulator: &mut Acc,
    parent_path: &str,
    depth: usize,
    parent_object: Option<&Map<String, Value>>,
    errors: &mut Vec<anyhow::Error>,
) -> Result<()>
where
    A: JsonAnalyzer<C, Acc> + Send + Sync,
    C: Send + Sync,
    Acc: Send + Sync,
{
    let Value::Array(items) = value else {
        return Ok(());
    };
    for (index, item) in items.iter().enumerate() {
        let path = format!("{parent_path}[{index}]");
        match item {
            Value::Object(_) => {
                Box::pin(_analyze_json(
                    item,
                    analyzer,
                    context,
                    accumulator,
                    &path,
                    depth,
                    parent_object,
                    errors,
                ))
                .await?
            }
            Value::Array(_) => {
                Box::pin(_analyze_array(
                    item,
                    analyzer,
                    context,
                    accumulator,
                    &path,
                    depth + 1,
                    parent_object,
                    errors,
                ))
                .await?
            }
            _ => {
                let field = JsonProcessingContext {
                    path,
                    key: index.to_string(),
                    parent_path: parent_path.to_owned(),
                    depth,
                    is_array_item: true,
                    array_index: Some(index),
                    parent_object: parent_object.cloned(),
                };
                if let Err(error) = analyzer.analyze(&field, item, context, accumulator).await {
                    errors.push(error);
                }
            }
        }
    }
    Ok(())
}

/// Analyze a JSON Value using the provided JsonAnalyzer and context, accumulates the results into the provided accumulator.
pub(crate) async fn analyze_json<A, C, Acc>(
    data: &Value,
    analyzer: &A,
    context: &C,
    mut accumulator: Acc,
) -> Result<Acc>
where
    A: JsonAnalyzer<C, Acc> + Send + Sync,
    C: Send + Sync,
    Acc: Send + Sync,
{
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
    } else if data.is_array() {
        _analyze_array(
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
        Err(ProcessingErrors { causes: errors }.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    struct Record(Mutex<Vec<JsonProcessingContext>>);
    #[async_trait]
    impl JsonProcessor<()> for Record {
        async fn process(
            &self,
            field: &JsonProcessingContext,
            _: &mut Value,
            _: &(),
        ) -> JsonProcessingResult {
            self.0.lock().unwrap().push(field.clone());
            JsonProcessingResult::new()
        }
    }
    #[async_trait]
    impl JsonAnalyzer<(), Vec<JsonProcessingContext>> for Record {
        async fn analyze(
            &self,
            field: &JsonProcessingContext,
            _: &Value,
            _: &(),
            seen: &mut Vec<JsonProcessingContext>,
        ) -> Result<()> {
            seen.push(field.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn analyzers_and_transformers_share_order_context_and_nested_array_contract() {
        for payload in [
            serde_json::json!({"Type":"Playlist", "Items":[{"Id":"first", "Children":[["nested"]]}, ["scalar"]]}),
            serde_json::json!([[{"Type":"Movie", "Id":"second"}], ["third", ["fourth"]]]),
            serde_json::json!("root scalar"),
        ] {
            let recorder = Record(Mutex::new(Vec::new()));
            let analyzed = analyze_json(&payload, &recorder, &(), Vec::new())
                .await
                .unwrap();
            let mut transformed = payload.clone();
            assert!(
                !process_json(&mut transformed, &recorder, &())
                    .await
                    .unwrap()
                    .was_modified
            );
            assert_eq!(transformed, payload);
            assert_eq!(*recorder.0.lock().unwrap(), analyzed);
            if payload.is_array() {
                assert!(analyzed.iter().any(|field| field.path == "[1][1][0]"));
            }
        }
    }

    struct FailingAnalyzer;
    #[async_trait]
    impl JsonAnalyzer<(), ()> for FailingAnalyzer {
        async fn analyze(
            &self,
            _: &JsonProcessingContext,
            _: &Value,
            _: &(),
            _: &mut (),
        ) -> Result<()> {
            Err(std::io::Error::other("original cause").into())
        }
    }

    #[tokio::test]
    async fn analysis_preserves_all_error_causes_without_string_flattening() {
        let error = analyze_json(&serde_json::json!([1, 2]), &FailingAnalyzer, &(), ())
            .await
            .unwrap_err();
        let causes = &error.downcast_ref::<ProcessingErrors>().unwrap().causes;
        assert_eq!(causes.len(), 2);
        assert!(causes.iter().all(|cause| cause.is::<std::io::Error>()));
    }
}
