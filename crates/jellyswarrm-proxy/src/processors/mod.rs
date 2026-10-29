//! Translate request/response JSON fields and URLs through the shared proxy processor pipeline.
mod delivery;
pub(crate) mod field_matcher;
mod json_processor;
pub(crate) mod request_analyzer;
pub(crate) mod request_processor;
pub(crate) mod response_processor;
pub(crate) mod url_processor;
mod user_ids;

pub(crate) use json_processor::ProcessingErrors;
pub(crate) use json_processor::{analyze_json, process_json};

mod pipeline;
pub(crate) use pipeline::ProxyProcessors;
