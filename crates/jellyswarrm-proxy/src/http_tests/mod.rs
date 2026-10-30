//! Client-to-proxy-to-upstream HTTP contracts. Run with `cargo test ... http_tests`.
// Source-owned transport contracts run once. Opt into scenario_test! only when
// merging changes the fixture's identities or the behavior being asserted.
macro_rules! contract_test {
    ($name:ident, $contract:ident $(, $arg:expr)* $(,)?) => {
        #[tokio::test]
        async fn $name() {
            $contract($($arg),*).await;
        }
    };
}

// Keep genuine configuration-dependent workflows independently runnable.
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
mod composition;
mod coverage;
mod discovery;
mod editing;
mod hls;
mod live_tv;
mod mutations;
mod pipeline;
mod preferences;
mod resources;
mod sessions;
pub(crate) mod support;

use crate::{config::MediaStreamingMode, protocol::authorization::Authorization};
use axum::http::StatusCode;
use serde_json::{json, Value};
use std::sync::Arc;
use support::{auth, Fixture, Scenario};

mod playlists;

mod federated;
