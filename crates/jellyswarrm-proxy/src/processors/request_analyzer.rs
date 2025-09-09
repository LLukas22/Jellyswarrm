use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::{
    processors::{
        field_matcher::{ID_FIELDS, SESSION_FIELDS, USER_FIELDS},
        json_processor::{JsonAnalyzer, JsonProcessingContext},
    },
    DataContext,
};

pub struct RequestAnalyzer {
    pub data_context: DataContext,
}

impl RequestAnalyzer {
    pub fn new(data_context: DataContext) -> Self {
        Self {
            data_context: data_context,
        }
    }
}

#[derive(Debug, Default)]
pub struct RequestAnalysisResult {
    pub found_ids: Vec<String>,
    pub found_session_ids: Vec<String>,
    pub found_user_ids: Vec<String>,
}

pub struct RequestAnalysisContext;

#[async_trait]
impl JsonAnalyzer<RequestAnalysisContext, RequestAnalysisResult> for RequestAnalyzer {
    async fn analyze(
        &self,
        json_context: &JsonProcessingContext,
        value: &Value,
        _context: &RequestAnalysisContext,
        accumulator: &mut RequestAnalysisResult,
    ) -> Result<Option<Vec<String>>> {
        // Check if this is an ID field (case-insensitive)
        if ID_FIELDS.contains(&json_context.key) {
            if let serde_json::Value::String(ref virtual_id) = value {
                accumulator.found_ids.push(virtual_id.clone());
            }
        }

        // Check if this is a SessionId field (case-insensitive)
        if SESSION_FIELDS.contains(&json_context.key) {
            if let serde_json::Value::String(ref session_id) = value {
                accumulator.found_session_ids.push(session_id.clone());
            }
        }

        // Check if this is a UserId field (case-insensitive)
        if USER_FIELDS.contains(&json_context.key) {
            if let serde_json::Value::String(ref user_id) = value {
                accumulator.found_user_ids.push(user_id.clone());
            }
        }
        Ok(None)
    }
}
