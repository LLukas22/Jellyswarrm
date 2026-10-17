//! Client-to-proxy-to-upstream HTTP contracts. Run with `cargo test ... http_tests`.
// Keep both configurations independently runnable and visible in test output.
// Arguments are evaluated inside each test, so fixtures and mocks are never shared.
macro_rules! scenario_test {
    ($name:ident, $contract:ident $(, $arg:expr)* $(,)?) => {
        mod $name {
            use super::*;

            #[tokio::test]
            async fn normal_servers() {
                $contract(Scenario::NormalServers $(, $arg)*).await;
            }

            #[tokio::test]
            async fn merged_libraries() {
                $contract(Scenario::MergedLibraries $(, $arg)*).await;
            }
        }
    };
}

mod access;
mod bodies;
mod catalog;
mod coverage;
mod discovery;
mod editing;
mod hls;
mod live_tv;
mod mutations;
mod pipeline;
mod preferences;
mod resources;
pub(crate) mod support;

use crate::{config::MediaStreamingMode, models::Authorization};
use axum::http::StatusCode;
use serde_json::{json, Value};
use std::sync::Arc;
use support::{auth, Fixture, Scenario};

mod playlists;
