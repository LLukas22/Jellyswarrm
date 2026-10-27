//! Jellyswarrm process entry point; startup and service wiring live in `app`.
mod app;
mod application;
mod auth;
mod config;
mod http;
#[cfg(test)]
mod http_tests;
mod libraries;
mod media;
mod models;
mod persistence;
mod playback;
mod processors;
mod servers;
mod sessions;
mod syncplay;
mod ui;
mod upstream;

pub use app::{AppState, DataContext};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    app::run().await
}
