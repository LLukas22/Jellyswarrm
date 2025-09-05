use serde_json::{Value, Map};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct ProcessingContext {
    pub path: String,
    pub key: String,
    pub parent_path: String,
    pub depth: usize,
    pub is_array_item: bool,
    pub array_index: Option<usize>,
}

#[derive(Debug)]
pub struct ProcessingResult {
    pub new_key: Option<String>,
    pub remove: bool,
    pub add_fields: HashMap<String, Value>,
    pub modified: bool,
    pub metadata: HashMap<String, Value>,
}

impl ProcessingResult {
    pub fn new() -> Self {
        Self {
            new_key: None,
            remove: false,
            add_fields: HashMap::new(),
            modified: false,
            metadata: HashMap::new(),
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
    
    pub fn add_metadata(mut self, key: String, value: Value) -> Self {
        self.metadata.insert(key, value);
        self
    }
}

#[derive(Debug, Clone)]
pub struct ProcessingStats {
    pub total_fields_processed: usize,
    pub fields_modified: usize,
    pub fields_removed: usize,
    pub fields_added: usize,
    pub fields_renamed: usize,
    pub processing_errors: Vec<String>,
    pub processing_warnings: Vec<String>,
    pub custom_metrics: HashMap<String, Value>,
}

impl ProcessingStats {
    pub fn new() -> Self {
        Self {
            total_fields_processed: 0,
            fields_modified: 0,
            fields_removed: 0,
            fields_added: 0,
            fields_renamed: 0,
            processing_errors: Vec::new(),
            processing_warnings: Vec::new(),
            custom_metrics: HashMap::new(),
        }
    }
    
    pub fn add_error(&mut self, error: String) {
        self.processing_errors.push(error);
    }
    
    pub fn add_warning(&mut self, warning: String) {
        self.processing_warnings.push(warning);
    }
    
    pub fn add_metric(&mut self, key: String, value: Value) {
        self.custom_metrics.insert(key, value);
    }
}

#[derive(Debug)]
pub struct ProcessingResponse {
    pub data: Value,
    pub was_modified: bool,
    pub stats: ProcessingStats,
    pub metadata: HashMap<String, Value>,
}

pub trait AdvancedJsonProcessor {
    fn process(&self, context: &ProcessingContext, value: &mut Value, stats: &mut ProcessingStats) -> ProcessingResult;
}

struct ExampleProcessor;

impl AdvancedJsonProcessor for ExampleProcessor {
    fn process(&self, context: &ProcessingContext, value: &mut Value, stats: &mut ProcessingStats) -> ProcessingResult {
        println!("Processing: {} = {}", context.path, value);
        
        let mut result = ProcessingResult::new();
        let original_value = value.clone();
        
        match context.key.as_str() {
            "password" | "secret" | "api_key" => {
                *value = Value::String("***".to_string());
                result = result.mark_modified()
                    .add_metadata("security_action".to_string(), Value::String("masked".to_string()));
                stats.add_warning(format!("Masked sensitive field at {}", context.path));
            }
            "email" => {
                if let Value::String(ref mut email) = value {
                    let was_valid = email.contains('@');
                    if !was_valid {
                        *email = format!("{}@example.com", email);
                        result = result.mark_modified();
                        stats.add_warning(format!("Fixed invalid email at {}", context.path));
                    }
                    // Add validation info
                    result = result.add_field(
                        "email_valid".to_string(), 
                        Value::Bool(email.contains('@'))
                    ).add_metadata("validation_performed".to_string(), Value::Bool(true));
                }
            }
            "age" => {
                if let Value::Number(num) = value {
                    if let Some(age) = num.as_i64() {
                        if age < 0 || age > 150 {
                            *value = Value::Number(25.into());
                            result = result.mark_modified()
                                .add_metadata("validation_error".to_string(), 
                                    Value::String(format!("Invalid age: {}", age)));
                            stats.add_error(format!("Invalid age {} at {}", age, context.path));
                        }
                    }
                }
            }
            "deprecated_field" => {
                result = result.remove_field()
                    .add_metadata("removal_reason".to_string(), 
                        Value::String("deprecated".to_string()));
                stats.add_warning(format!("Removed deprecated field at {}", context.path));
            }
            "temp_id" => {
                result = result.rename_key("id".to_string())
                    .add_metadata("rename_reason".to_string(), 
                        Value::String("standardization".to_string()));
            }
            "server_info" => {
                // Add server information
                result = result.add_field("server_name".to_string(), 
                    Value::String("production-server-01".to_string()))
                    .add_field("server_region".to_string(), 
                        Value::String("us-east-1".to_string()))
                    .add_field("processed_at".to_string(), 
                        Value::String(chrono::Utc::now().to_rfc3339()));
                stats.add_metric("server_fields_added".to_string(), Value::Number(3.into()));
            }
            _ => {
                // Check for common patterns
                if context.key.ends_with("_url") {
                    if let Value::String(ref url) = value {
                        if !url.starts_with("http") {
                            stats.add_warning(format!("URL field {} may be invalid: {}", context.path, url));
                        }
                    }
                }
            }
        }
        
        // Track if value was actually modified
        if *value != original_value {
            result.modified = true;
        }
        
        result
    }
}

fn process_json_advanced<P: AdvancedJsonProcessor>(
    value: &mut Value,
    processor: &P,
    parent_path: &str,
    depth: usize,
    stats: &mut ProcessingStats,
) -> (Map<String, Value>, bool) {
    let mut new_map = Map::new();
    let mut was_modified = false;
    
    if let Value::Object(map) = value {
        for (key, val) in map.iter_mut() {
            stats.total_fields_processed += 1;
            
            let current_path = if parent_path.is_empty() {
                key.clone()
            } else {
                format!("{}.{}", parent_path, key)
            };
            
            let context = ProcessingContext {
                path: current_path.clone(),
                key: key.clone(),
                parent_path: parent_path.to_string(),
                depth,
                is_array_item: false,
                array_index: None,
            };
            
            // Process nested structures
            match val {
                Value::Object(_) => {
                    let (nested_result, nested_modified) = process_json_advanced(val, processor, &current_path, depth + 1, stats);
                    *val = Value::Object(nested_result);
                    if nested_modified {
                        was_modified = true;
                    }
                }
                Value::Array(arr) => {
                    for (index, item) in arr.iter_mut().enumerate() {
                        if let Value::Object(_) = item {
                            let array_path = format!("{}[{}]", current_path, index);
                            let _array_context = ProcessingContext {
                                path: array_path.clone(),
                                key: format!("[{}]", index),
                                parent_path: current_path.clone(),
                                depth: depth + 1,
                                is_array_item: true,
                                array_index: Some(index),
                            };
                            let (nested_result, nested_modified) = process_json_advanced(item, processor, &array_path, depth + 1, stats);
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
            let result = processor.process(&context, val, stats);
            
            // Track modifications
            if result.modified {
                was_modified = true;
                stats.fields_modified += 1;
            }
            
            // Apply processing result
            if result.remove {
                stats.fields_removed += 1;
                was_modified = true;
            } else {
                let final_key = if let Some(new_key) = result.new_key {
                    stats.fields_renamed += 1;
                    was_modified = true;
                    new_key
                } else {
                    key.clone()
                };
                
                new_map.insert(final_key, val.clone());
                
                // Add any additional fields
                for (add_key, add_value) in result.add_fields {
                    new_map.insert(add_key, add_value);
                    stats.fields_added += 1;
                    was_modified = true;
                }
            }
        }
    }
    
    (new_map, was_modified)
}

pub fn process_json<P: AdvancedJsonProcessor>(
    data: &mut Value, 
    processor: &P
) -> ProcessingResponse {
    let mut stats = ProcessingStats::new();
    let mut global_metadata = HashMap::new();
    
    // Add processing metadata
    global_metadata.insert("processing_started_at".to_string(), 
        Value::String(chrono::Utc::now().to_rfc3339()));
    global_metadata.insert("processor_version".to_string(), 
        Value::String("1.0.0".to_string()));
    
    let was_modified = if let Value::Object(_) = data {
        let (processed_map, modified) = process_json_advanced(data, processor, "", 0, &mut stats);
        *data = Value::Object(processed_map);
        modified
    } else {
        false
    };
    
    global_metadata.insert("processing_completed_at".to_string(), 
        Value::String(chrono::Utc::now().to_rfc3339()));
    
    // Add final statistics to metadata
    global_metadata.insert("total_modifications".to_string(), 
        Value::Number(stats.fields_modified.into()));
    
    ProcessingResponse {
        data: data.clone(),
        was_modified,
        stats,
        metadata: global_metadata,
    }
}



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_modification_tracking() {
        let json_str = r#"{"name":"John","email":"invalid-email"}"#;
        let mut data: Value = serde_json::from_str(json_str).unwrap();
        let processor = ExampleProcessor;
        
        let response = process_json(&mut data, &processor);
        
        assert!(response.was_modified);
        assert!(response.stats.fields_modified > 0);
        assert!(response.data.get("email_valid").is_some());
    }
    
    #[test]
    fn test_no_modification() {
        let json_str = r#"{"name":"John","email":"john@test.com"}"#;
        let mut data: Value = serde_json::from_str(json_str).unwrap();
        let processor = ExampleProcessor;
        
        let response = process_json(&mut data, &processor);
        
        // Should still be modified due to email_valid field being added
        assert!(response.was_modified);
    }
    
    #[test]
    fn test_comprehensive_processing() {
        let json_str = r#"
        {
            "name": "  John  ",
            "age": -5,
            "email": "john",
            "password": "secret123",
            "temp_id": "12345",
            "deprecated_field": "old_value",
            "server_info": "placeholder",
            "profile_url": "invalid-url",
            "address": {
                "street": "123 Main St",
                "city": "Boston",
                "api_key": "super-secret-key"
            },
            "contacts": [
                {
                    "name": "Alice",
                    "email": "alice"
                },
                {
                    "name": "",
                    "email": "bob@test.com",
                    "password": "another-secret"
                }
            ]
        }"#;

        let mut data: Value = serde_json::from_str(json_str).unwrap();
        let processor = ExampleProcessor;
        
        println!("Original JSON:");
        println!("{}\n", serde_json::to_string_pretty(&data).unwrap());
        
        println!("Processing...\n");
        let response = process_json(&mut data, &processor);
        
        println!("=== PROCESSING RESULTS ===");
        println!("Was Modified: {}", response.was_modified);
        println!("\nStatistics:");
        println!("  Total fields processed: {}", response.stats.total_fields_processed);
        println!("  Fields modified: {}", response.stats.fields_modified);
        println!("  Fields removed: {}", response.stats.fields_removed);
        println!("  Fields added: {}", response.stats.fields_added);
        println!("  Fields renamed: {}", response.stats.fields_renamed);
        
        if !response.stats.processing_errors.is_empty() {
            println!("\nErrors:");
            for error in &response.stats.processing_errors {
                println!("  ❌ {}", error);
            }
        }
        
        if !response.stats.processing_warnings.is_empty() {
            println!("\nWarnings:");
            for warning in &response.stats.processing_warnings {
                println!("  ⚠️  {}", warning);
            }
        }
        
        if !response.stats.custom_metrics.is_empty() {
            println!("\nCustom Metrics:");
            for (key, value) in &response.stats.custom_metrics {
                println!("  📊 {}: {}", key, value);
            }
        }
        
        println!("\nGlobal Metadata:");
        for (key, value) in &response.metadata {
            println!("  📋 {}: {}", key, value);
        }
        
        println!("\nProcessed JSON:");
        println!("{}", serde_json::to_string_pretty(&response.data).unwrap());
        
        // Assertions to verify the processing worked correctly
        assert!(response.was_modified);
        assert!(response.stats.total_fields_processed > 0);
        assert!(response.stats.fields_modified > 0);
        assert!(response.stats.fields_removed > 0); // deprecated_field should be removed
        assert!(response.stats.fields_added > 0); // email_valid, server fields should be added
        assert!(response.stats.fields_renamed > 0); // temp_id -> id
        
        // Verify specific transformations
        assert_eq!(response.data.get("password").unwrap(), &Value::String("***".to_string()));
        assert!(response.data.get("deprecated_field").is_none()); // Should be removed
        assert!(response.data.get("id").is_some()); // temp_id should be renamed to id
        assert!(response.data.get("temp_id").is_none()); // temp_id should no longer exist
        assert!(response.data.get("email_valid").is_some()); // Should have email validation field
        
        // Verify nested processing worked
        let address = response.data.get("address").unwrap().as_object().unwrap();
        assert_eq!(address.get("api_key").unwrap(), &Value::String("***".to_string()));
        
        // Verify array processing worked
        let contacts = response.data.get("contacts").unwrap().as_array().unwrap();
        for contact in contacts {
            let contact_obj = contact.as_object().unwrap();
            if contact_obj.contains_key("password") {
                assert_eq!(contact_obj.get("password").unwrap(), &Value::String("***".to_string()));
            }
        }
    }
}