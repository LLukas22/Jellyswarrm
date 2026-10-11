//! Client-to-proxy-to-upstream HTTP contracts. Run with `cargo test ... http_tests`.
mod access;
mod bodies;
mod catalog;
mod coverage;
mod discovery;
mod editing;
mod known_gaps;
mod mutations;
mod pipeline;
mod preferences;
mod resources;
pub(crate) mod support;

use crate::{config::MediaStreamingMode, models::Authorization};
use axum::http::StatusCode;
use serde_json::{json, Value};
use std::sync::Arc;
use support::{auth, Fixture};
