//! Translate request/response JSON fields and URLs through the shared proxy processor pipeline.
pub mod field_matcher;
mod json_processor;
pub mod request_analyzer;
pub mod request_processor;
pub mod response_processor;
pub mod url_processor;

pub use json_processor::{analyze_json, process_json};

mod pipeline;
pub(crate) use pipeline::ProxyProcessors;
